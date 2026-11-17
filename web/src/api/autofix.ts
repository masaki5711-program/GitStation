// The deterministic autofix (M4 C14, ADR-0017 Amendment C14): a project's
// switch (off by default; a Maintainer turns it on) and what it did with one
// merge request (the one line on its screen). Keys start with
// ['autofix', projectId]. The merge request's line is read again while an
// attempt is queued or running.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type Autofix = S['AutofixBody'];
export type MergeRequestAutofix = S['MergeRequestAutofixBody'];
export type AutofixAttempt = S['AutofixAttemptBody'];

/** How often a merge request's autofix line is read again while it runs (ms). */
export const AUTOFIX_POLL_MS = 10000;

export const autofixKeys = {
  all: (project: number) => ['autofix', project] as const,
  project: (project: number) => ['autofix', project, 'project'] as const,
  mr: (project: number, iid: number) => ['autofix', project, 'mr', iid] as const,
};

export function useAutofix(project: number) {
  const api = useApi();
  return useQuery({
    queryKey: autofixKeys.project(project),
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}/autofix', { signal, params: { path: { id: project } } })),
  });
}

/** An attempt that may still change by itself. */
export const isLive = (a: AutofixAttempt | null | undefined): boolean =>
  a?.state === 'queued' || a?.state === 'running';

export function useMergeRequestAutofix(project: number, iid: number, open: boolean) {
  const api = useApi();
  return useQuery({
    queryKey: autofixKeys.mr(project, iid),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/autofix', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
    refetchInterval: (q) => (open && isLive(q.state.data?.attempt) ? AUTOFIX_POLL_MS : false),
  });
}

export function useSetAutofix(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (enabled: boolean) =>
      call(() =>
        api.PUT('/api/v1/projects/{id}/autofix', {
          params: { path: { id: project } },
          body: { enabled },
        }),
      ),
    onSuccess: async (next: Autofix) => {
      queryClient.setQueryData(autofixKeys.project(project), next);
      await queryClient.invalidateQueries({ queryKey: autofixKeys.all(project) });
    },
    // A 403 on a save is most often a stale CSRF token: read the session again.
    onError: (error: unknown) => {
      if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 403) {
        void queryClient.invalidateQueries({ queryKey: SESSION_KEY });
      }
    },
  });
}
