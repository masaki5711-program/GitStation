// Merge request queries and mutations (U1). Keys start with
// ['mr', projectId] so one invalidation reaches everything of a project.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call } from './client';
import { useApi } from './context';
import type { CreateMergeRequestBody } from './types';

export type MrState = 'open' | 'closed' | 'merged' | 'all';
export interface MrListFilter {
  state?: MrState;
  limit?: number;
  before?: number;
}

export const mrKeys = {
  all: (project: number) => ['mr', project] as const,
  list: (project: number, filter: MrListFilter) => ['mr', project, 'list', filter] as const,
  one: (project: number, iid: number) => ['mr', project, iid, 'one'] as const,
  status: (project: number, iid: number) => ['mr', project, iid, 'status'] as const,
  discussions: (project: number, iid: number) => ['mr', project, iid, 'discussions'] as const,
  versions: (project: number, iid: number) => ['mr', project, iid, 'versions'] as const,
  diff: (project: number, iid: number, version: number) =>
    ['mr', project, iid, 'diff', version] as const,
};

export function useMergeRequests(project: number, filter: MrListFilter = {}) {
  const api = useApi();
  return useQuery({
    queryKey: mrKeys.list(project, filter),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests', {
          signal,
          params: { path: { id: project }, query: filter },
        }),
      ),
  });
}

export function useMergeRequest(project: number, iid: number) {
  const api = useApi();
  return useQuery({
    queryKey: mrKeys.one(project, iid),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
  });
}

export function useMergeStatus(project: number, iid: number) {
  const api = useApi();
  return useQuery({
    queryKey: mrKeys.status(project, iid),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/status', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
  });
}

export function useDiscussions(project: number, iid: number) {
  const api = useApi();
  return useQuery({
    queryKey: mrKeys.discussions(project, iid),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/discussions', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
  });
}

export function useVersions(project: number, iid: number) {
  const api = useApi();
  return useQuery({
    queryKey: mrKeys.versions(project, iid),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/versions', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
  });
}

export function useVersionDiff(project: number, iid: number, version: number) {
  const api = useApi();
  return useQuery({
    queryKey: mrKeys.diff(project, iid, version),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/versions/{version}/diff', {
          signal,
          params: { path: { id: project, iid: String(iid), version: String(version) } },
        }),
      ),
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

export function useCreateMergeRequest(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (body: CreateMergeRequestBody) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/merge-requests', {
          params: { path: { id: project } },
          body,
        }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: mrKeys.all(project) }),
    onError: refreshSession,
  });
}

/** After a discussion change the status (the unresolved count) is stale too. */
function useAfterDiscussionChange(project: number, iid: number) {
  const queryClient = useQueryClient();
  return () =>
    Promise.all([
      queryClient.invalidateQueries({ queryKey: mrKeys.discussions(project, iid) }),
      queryClient.invalidateQueries({ queryKey: mrKeys.status(project, iid) }),
    ]);
}

export function useAddComment(project: number, iid: number) {
  const api = useApi();
  const refreshSession = useRefreshSessionOn403();
  const after = useAfterDiscussionChange(project, iid);
  return useMutation({
    mutationFn: (input: { body: string }) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/merge-requests/{iid}/discussions', {
          params: { path: { id: project, iid: String(iid) } },
          body: { body: input.body },
        }),
      ),
    onSuccess: after,
    onError: refreshSession,
  });
}

export function useReply(project: number, iid: number) {
  const api = useApi();
  const refreshSession = useRefreshSessionOn403();
  const after = useAfterDiscussionChange(project, iid);
  return useMutation({
    mutationFn: (input: { discussionId: number; body: string }) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/notes', {
          params: {
            path: { id: project, iid: String(iid), discussion_id: input.discussionId },
          },
          body: { body: input.body },
        }),
      ),
    onSuccess: after,
    onError: refreshSession,
  });
}

export function useResolve(project: number, iid: number) {
  const api = useApi();
  const refreshSession = useRefreshSessionOn403();
  const after = useAfterDiscussionChange(project, iid);
  return useMutation({
    mutationFn: (discussionId: number) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/resolve', {
          params: { path: { id: project, iid: String(iid), discussion_id: discussionId } },
        }),
      ),
    onSuccess: after,
    onError: refreshSession,
  });
}

export function useUnresolve(project: number, iid: number) {
  const api = useApi();
  const refreshSession = useRefreshSessionOn403();
  const after = useAfterDiscussionChange(project, iid);
  return useMutation({
    mutationFn: (discussionId: number) =>
      call(() =>
        api.POST(
          '/api/v1/projects/{id}/merge-requests/{iid}/discussions/{discussion_id}/unresolve',
          { params: { path: { id: project, iid: String(iid), discussion_id: discussionId } } },
        ),
      ),
    onSuccess: after,
    onError: refreshSession,
  });
}
