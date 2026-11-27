// The local LLM's first pass on a merge request (plan 19 §3.1 / §4, L5-5 /
// L5-6; ADR-0039): `GET …/ai-review` (the project's llm.review, the
// forge/ai-review run on the head and the newest advice, read like the
// merge request) and a person's 「役に立った / 誤り」 on one finding
// (`POST …/ai-review/feedback`, AiAdviceFeedback). The advice never decides
// anything: nothing here is read by the merge, approve or deploy controls.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { ApiFailure, call } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';
import { SESSION_KEY } from '../session/session';

type S = components['schemas'];
export type AiReview = S['AiReviewBody'];
export type AiReviewAdvice = S['AiReviewAdviceBody'];
export type AiReviewFinding = S['AiReviewFindingBody'];
export type AiVerdict = 'useful' | 'wrong';

/** While the head waits for its first pass, read again this often (ms):
 * the worker looks every 30 s (ADR-0039 amendment L5-5, decision 1). */
export const AI_REVIEW_POLL_MS = 30000;

export const aiReviewKeys = {
  mr: (project: number, iid: number) => ['ai-review', project, iid] as const,
};

/** Whether the screen has anything to say: the first pass is on for the
 * project, or something was made (an advice, or a run on the head, even
 * after the project turned it off). */
export function aiAdviceVisible(data: AiReview): boolean {
  return data.mode !== 'off' || data.advice.state !== 'none' || data.check !== null;
}

/** Read again only while an open merge request's head waits for its first
 * pass (no run yet, or a run not completed); never when it is off. */
export function aiReviewPollInterval(data: AiReview | undefined, open: boolean): number | false {
  if (data === undefined || !open || data.mode === 'off') return false;
  return data.check === null || data.check.status !== 'completed' ? AI_REVIEW_POLL_MS : false;
}

export function useAiReview(project: number, iid: number, open: boolean) {
  const api = useApi();
  return useQuery({
    queryKey: aiReviewKeys.mr(project, iid),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/ai-review', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
    refetchInterval: (q) => aiReviewPollInterval(q.state.data, open),
  });
}

export interface AiVote {
  adviceId: number;
  findingIndex: number;
  verdict: AiVerdict;
}

/** The reader's verdict on finding `index` (`null`: none given). */
export function myVerdict(advice: AiReviewAdvice, index: number): AiVerdict | null {
  const v = advice.my_feedback.find((f) => f.finding_index === index)?.verdict;
  return v === 'useful' || v === 'wrong' ? v : null;
}

/** `POST …/ai-review/feedback` from this web session: one verdict per
 * person and finding (a new one replaces theirs). The answer is written
 * into the read, so the pressed button is the server's word. */
export function useAiFeedback(project: number, iid: number) {
  const api = useApi();
  const client = useQueryClient();
  return useMutation({
    mutationFn: (vote: AiVote) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/merge-requests/{iid}/ai-review/feedback', {
          params: { path: { id: project, iid: String(iid) } },
          body: { advice_id: vote.adviceId, finding_index: vote.findingIndex, verdict: vote.verdict },
        }),
      ),
    onSuccess: (answer) => {
      client.setQueryData<AiReview>(aiReviewKeys.mr(project, iid), (old) => {
        if (old?.advice.advice_id !== answer.advice_id) return old;
        const rest = old.advice.my_feedback.filter((f) => f.finding_index !== answer.finding_index);
        const mine = [...rest, { finding_index: answer.finding_index, verdict: answer.verdict }].sort(
          (a, b) => a.finding_index - b.finding_index,
        );
        return { ...old, advice: { ...old.advice, my_feedback: mine } };
      });
    },
    onError: (error) => {
      // A 403 is most often a stale CSRF token: read the session again.
      if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 403) {
        void client.invalidateQueries({ queryKey: SESSION_KEY });
      }
    },
  });
}
