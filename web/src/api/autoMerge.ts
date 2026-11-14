// Auto-merge (M4 C9, ADR-0017 Amendment 2026-11-13): a project's merge.auto
// and emergency stop, and what auto-merge did with one merge request (the
// one line on its screen). Keys start with ['autoMerge', projectId]. The
// merge request's state is read again while it may still change by itself.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type AutoMerge = S['AutoMergeBody'];
export type MergeRequestAutoMerge = S['MergeRequestAutoMergeBody'];
export type AutoMergeState = S['AutoMergeStateBody'];

export const AUTO_MERGE_MODES = ['off', 'reserve', 'conditional', 'full'] as const;
export type AutoMergeMode = (typeof AUTO_MERGE_MODES)[number];
export const isAutoMergeMode = (s: string): s is AutoMergeMode =>
  (AUTO_MERGE_MODES as readonly string[]).includes(s);

/** How often a merge request's auto-merge line is read again (ms). */
export const AUTO_MERGE_POLL_MS = 10000;

export const autoMergeKeys = {
  all: (project: number) => ['autoMerge', project] as const,
  project: (project: number) => ['autoMerge', project, 'project'] as const,
  mr: (project: number, iid: number) => ['autoMerge', project, 'mr', iid] as const,
};

export function useAutoMerge(project: number) {
  const api = useApi();
  return useQuery({
    queryKey: autoMergeKeys.project(project),
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}/auto-merge', { signal, params: { path: { id: project } } })),
  });
}

export function useMergeRequestAutoMerge(project: number, iid: number, open: boolean) {
  const api = useApi();
  return useQuery({
    queryKey: autoMergeKeys.mr(project, iid),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/auto-merge', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
    // Only an open merge request of a project that uses auto-merge moves.
    refetchInterval: (q) => (open && q.state.data?.acts === true ? AUTO_MERGE_POLL_MS : false),
  });
}

/** A 403 on a save is most often a stale CSRF token: read the session again. */
function useRefreshSessionOn403() {
  const queryClient = useQueryClient();
  return (error: unknown) => {
    if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 403) {
      void queryClient.invalidateQueries({ queryKey: SESSION_KEY });
    }
  };
}

export function useSetAutoMergeMode(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (mode: AutoMergeMode) =>
      call(() =>
        api.PUT('/api/v1/projects/{id}/auto-merge', {
          params: { path: { id: project } },
          body: { mode },
        }),
      ),
    onSuccess: async (next: AutoMerge) => {
      queryClient.setQueryData(autoMergeKeys.project(project), next);
      await queryClient.invalidateQueries({ queryKey: autoMergeKeys.all(project) });
    },
    onError: refreshSession,
  });
}

export function useSetAutoMergeStop(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (stopped: boolean) =>
      call(() =>
        stopped
          ? api.POST('/api/v1/projects/{id}/auto-merge/stop', { params: { path: { id: project } } })
          : api.POST('/api/v1/projects/{id}/auto-merge/resume', { params: { path: { id: project } } }),
      ),
    onSuccess: async (next: AutoMerge) => {
      queryClient.setQueryData(autoMergeKeys.project(project), next);
      await queryClient.invalidateQueries({ queryKey: autoMergeKeys.all(project) });
    },
    onError: refreshSession,
  });
}
