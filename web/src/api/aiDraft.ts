// The AI asset draft beside a draft improvement merge request (plan 17
// P17-10; ADR-0039 amendment P17-10b): `GET …/ai-draft`, read like the
// merge request — the branch `ai-draft/<id>` and its one commit, the asset
// paths it changes, how many of the model's edits the checks dropped, and
// whether the branch still holds the draft. Code-made facts only (never
// the model's words). The draft decides nothing: nothing here is read by
// the merge, approve or ready controls.
import { useQuery } from '@tanstack/react-query';

import { call } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type AiDraft = S['AiDraftBody'];
export type AiDraftDetail = S['AiDraftDetailBody'];

/** While an open improvement merge request has no draft yet, read again
 * this often (ms): the worker looks every 60 s. */
export const AI_DRAFT_POLL_MS = 60000;

export const aiDraftKeys = {
  mr: (project: number, iid: number) => ['ai-draft', project, iid] as const,
};

/** Only a merge request from `improve/<id>` gets a draft (the worker's
 * rule); the screen asks for no other. */
export function isImprovementBranch(source: string): boolean {
  const short = source.startsWith('refs/heads/') ? source.slice('refs/heads/'.length) : source;
  return short.startsWith('improve/') && short.length > 'improve/'.length;
}

/** Read again only while an open draft improvement merge request waits for
 * its draft. */
export function aiDraftPollInterval(data: AiDraft | undefined, waiting: boolean): number | false {
  if (data === undefined || !waiting) return false;
  return data.improvement && data.state === 'none' ? AI_DRAFT_POLL_MS : false;
}

export function useAiDraft(project: number, iid: number, enabled: boolean, waiting: boolean) {
  const api = useApi();
  return useQuery({
    queryKey: aiDraftKeys.mr(project, iid),
    enabled,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/ai-draft', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
    refetchInterval: (q) => aiDraftPollInterval(q.state.data, waiting),
  });
}
