// Merge request queries and mutations (U1). Keys start with
// ['mr', projectId] so one invalidation reaches everything of a project.
import { queryOptions, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { invalidateActivity } from './activity';
import { IMMUTABLE } from './cachePolicy';
import { ApiFailure, call, type ApiClient } from './client';
import { useApi } from './context';
import type {
  CreateMergeRequestBody,
  LatestMerge,
  MergeAttempt,
  MergeStatus,
  Position,
} from './types';

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
  merge: (project: number, iid: number) => ['mr', project, iid, 'merge'] as const,
  /** Everything of one merge request (a reload after a stale view). */
  whole: (project: number, iid: number) => ['mr', project, iid] as const,
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

export const mergeRequestsQuery = (api: ApiClient, project: number, filter: MrListFilter = {}) =>
  queryOptions({
    queryKey: mrKeys.list(project, filter),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests', {
          signal,
          params: { path: { id: project }, query: filter },
        }),
      ),
  });

export const mergeRequestQuery = (api: ApiClient, project: number, iid: number) =>
  queryOptions({
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

export function useMergeRequests(project: number, filter: MrListFilter = {}) {
  return useQuery(mergeRequestsQuery(useApi(), project, filter));
}

export function useMergeRequest(project: number, iid: number) {
  return useQuery(mergeRequestQuery(useApi(), project, iid));
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
    // A version is a pair of commit ids: its diff never changes.
    ...IMMUTABLE,
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
    onSuccess: () =>
      Promise.all([queryClient.invalidateQueries({ queryKey: mrKeys.all(project) }), invalidateActivity(queryClient)]),
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
      invalidateActivity(queryClient),
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

/** After an approval changes, the status (counts, viewer flags) is stale. */
function useAfterApprovalChange(project: number, iid: number) {
  const queryClient = useQueryClient();
  return () =>
    Promise.all([
      queryClient.invalidateQueries({ queryKey: mrKeys.status(project, iid) }),
      invalidateActivity(queryClient),
    ]);
}

/** Approve the head the person saw (`sha` from the status they read): the
 * server refuses another head (409 stale view) rather than approving code
 * nobody looked at. */
export function useApprove(project: number, iid: number) {
  const api = useApi();
  const refreshSession = useRefreshSessionOn403();
  const after = useAfterApprovalChange(project, iid);
  return useMutation({
    mutationFn: (sha: string) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/merge-requests/{iid}/approve', {
          params: { path: { id: project, iid: String(iid) } },
          body: { sha },
        }),
      ),
    onSuccess: after,
    onError: refreshSession,
  });
}

export function useUnapprove(project: number, iid: number) {
  const api = useApi();
  const refreshSession = useRefreshSessionOn403();
  const after = useAfterApprovalChange(project, iid);
  return useMutation({
    mutationFn: () =>
      call(() =>
        api.POST('/api/v1/projects/{id}/merge-requests/{iid}/unapprove', {
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
    onSuccess: after,
    onError: refreshSession,
  });
}

/** Take a draft out of draft (`PATCH …` with `draft: false` only). */
export function useMarkReady(project: number, iid: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: () =>
      call(() =>
        api.PATCH('/api/v1/projects/{id}/merge-requests/{iid}', {
          params: { path: { id: project, iid: String(iid) } },
          body: { draft: false },
        }),
      ),
    onSuccess: async (next) => {
      queryClient.setQueryData(mrKeys.one(project, iid), next);
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: mrKeys.status(project, iid) }),
        queryClient.invalidateQueries({ queryKey: ['mr', project, 'list'] }),
      ]);
    },
    onError: refreshSession,
  });
}

/** Read everything of one merge request again (after a stale view). */
export function useReloadMergeRequest(project: number, iid: number) {
  const queryClient = useQueryClient();
  return () => queryClient.invalidateQueries({ queryKey: mrKeys.whole(project, iid) });
}

export interface StartMerge {
  /** The head the person saw (from the status). */
  sha: string;
  method: string;
  deleteSourceBranch: boolean;
}

/** Ask for a merge (202: the attempt is queued; the job does the rest). */
export function useStartMerge(project: number, iid: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (input: StartMerge) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/merge-requests/{iid}/merge', {
          params: { path: { id: project, iid: String(iid) } },
          body: {
            sha: input.sha,
            method: input.method,
            delete_source_branch: input.deleteSourceBranch,
          },
        }),
      ),
    onSuccess: (attempt) => {
      const latest: LatestMerge = { iid, merge: attempt };
      queryClient.setQueryData(mrKeys.merge(project, iid), latest);
      return queryClient.invalidateQueries({ queryKey: mrKeys.status(project, iid) });
    },
    onError: refreshSession,
  });
}

/** The attempt states after which nothing changes any more. */
export function isFinished(attempt: Pick<MergeAttempt, 'state'> | null | undefined): boolean {
  const s = attempt?.state;
  return s === 'applied' || s === 'superseded' || s === 'failed';
}

/** The merge attempt is read every 1 s at first, then less often up to every
 * 5 s; after 2 minutes the screen stops asking (the merge goes on in the
 * server; the person may leave). */
export const MERGE_POLL_FIRST_MS = 1000;
export const MERGE_POLL_MAX_MS = 5000;
export const MERGE_POLL_GIVE_UP_MS = 120_000;

/** The wait before the next read: 1 s, 2 s, 4 s, then 5 s; `false` once the
 * attempt finished or the time is up. Pure (tests/mr-merge.test.tsx). */
export function mergePollDelay(
  reads: number,
  elapsedMs: number,
  attempt: Pick<MergeAttempt, 'state'> | null | undefined,
): number | false {
  if (isFinished(attempt) || elapsedMs >= MERGE_POLL_GIVE_UP_MS) return false;
  const n = Math.max(0, reads - 1);
  return Math.min(MERGE_POLL_FIRST_MS * 2 ** Math.min(n, 3), MERGE_POLL_MAX_MS);
}

/** The latest merge attempt, read while it runs. `since`: when the screen
 * started watching (Date.now()); `null`: not watching (nothing is read). */
export function useMergeAttempt(project: number, iid: number, since: number | null, minMergeId = 0) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useQuery<LatestMerge>({
    queryKey: mrKeys.merge(project, iid),
    enabled: valid(iid) && since !== null,
    // The 202 answer is the first state; the next read is the poll's.
    staleTime: MERGE_POLL_FIRST_MS,
    refetchInterval: (query) => {
      if (since === null) return false;
      // An attempt older than the one started here is not finished news.
      const cached = query.state.data?.merge ?? null;
      const current = cached !== null && cached.merge_id >= minMergeId ? cached : null;
      return mergePollDelay(query.state.dataUpdateCount, Date.now() - since, current);
    },
    queryFn: async ({ signal }) => {
      const latest = await call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/merge', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      );
      // A finished merge changes the merge request (state, status): read it again.
      if (isFinished(latest.merge)) {
        void queryClient.invalidateQueries({ queryKey: mrKeys.one(project, iid) });
        void queryClient.invalidateQueries({ queryKey: mrKeys.status(project, iid) });
        void invalidateActivity(queryClient);
      }
      return latest;
    },
  });
}

/** The newest version of a list (what the Changes tab renders), or null. */
export function latestVersion<V extends { version: number }>(list: readonly V[]): V | null {
  return list.reduce<V | null>((best, v) => (best === null || v.version > best.version ? v : best), null);
}
