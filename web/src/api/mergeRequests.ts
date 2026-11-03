// Merge request queries and mutations (U1). Keys start with
// ['mr', projectId] so one invalidation reaches everything of a project.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call } from './client';
import { useApi } from './context';
import type { CreateMergeRequestBody, MergeStatus, Position } from './types';

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

/** A merge request number from the address (`/-/pull/{iid}`): a positive
 * decimal without a leading zero that fits a safe integer; anything else is
 * `null` (the screen is "not found" and asks the API nothing). */
export function parseIid(raw: string): number | null {
  if (!/^[1-9][0-9]{0,15}$/.test(raw)) return null;
  const n = Number(raw);
  return Number.isSafeInteger(n) ? n : null;
}

/** An iid or version number the API can be asked about. */
const valid = (n: number) => Number.isSafeInteger(n) && n > 0;

/** How often the status is read again while it waits (ms). */
export const STATUS_POLL_MS = 3000;

/** The status is read again while its first reason (the one-line answer) is
 * one that changes by itself: the mergeability check or a merge running. */
export function shouldPollStatus(status: Pick<MergeStatus, 'reasons'> | undefined): boolean {
  const first = status?.reasons[0]?.reason;
  return first === 'mergeability_unknown' || first === 'merge_in_progress';
}

function statusPollInterval(query: { state: { data: MergeStatus | undefined } }): number | false {
  return shouldPollStatus(query.state.data) ? STATUS_POLL_MS : false;
}

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
    enabled: valid(iid),
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
    enabled: valid(iid),
    refetchInterval: statusPollInterval,
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
    enabled: valid(iid),
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
    enabled: valid(iid),
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
    enabled: valid(iid) && valid(version),
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
    /** A general thread; with `position`, a thread on that line of a version. */
    mutationFn: (input: { body: string; position?: Position }) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/merge-requests/{iid}/discussions', {
          params: { path: { id: project, iid: String(iid) } },
          body:
            input.position === undefined
              ? { body: input.body }
              : { body: input.body, position: input.position },
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
