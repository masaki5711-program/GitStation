// A project's llm.review (plan 19 §3.1 / §8.2, ADR-0039): whether the local
// LLM's first pass reviews the project's merge requests — off (the default),
// advisory, or required. Read by everyone who reads the project; changed by
// a Maintainer in a web session. Keys start with ['llm', projectId].
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { aiReviewKeys } from './aiReview';
import { ApiFailure, call } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

export type LlmSetting = components['schemas']['LlmSettingBody'];

export const LLM_REVIEW_MODES = ['off', 'advisory', 'required'] as const;
export type LlmReviewMode = (typeof LLM_REVIEW_MODES)[number];

export function isLlmReviewMode(v: string): v is LlmReviewMode {
  return (LLM_REVIEW_MODES as readonly string[]).includes(v);
}

export const llmKeys = {
  project: (project: number) => ['llm', project] as const,
};

export function useLlmSetting(project: number) {
  const api = useApi();
  return useQuery({
    queryKey: llmKeys.project(project),
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}/llm', { signal, params: { path: { id: project } } })),
  });
}

export function useSetLlmSetting(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (review: LlmReviewMode) =>
      call(() =>
        api.PUT('/api/v1/projects/{id}/llm', {
          params: { path: { id: project } },
          body: { review },
        }),
      ),
    onSuccess: (next: LlmSetting) => {
      queryClient.setQueryData(llmKeys.project(project), next);
      // The merge request screens show the mode next to the advice.
      void queryClient.invalidateQueries({ queryKey: aiReviewKeys.all(project) });
    },
    // A 403 on a save is most often a stale CSRF token: read the session again.
    onError: (error: unknown) => {
      if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 403) {
        void queryClient.invalidateQueries({ queryKey: SESSION_KEY });
      }
    },
  });
}
