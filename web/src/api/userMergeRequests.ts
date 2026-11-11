// The signed-in person's merge requests across projects (plan 15 MR 6/7):
// the counts of each scope (Home's cards, the sidebar's badge; one query,
// shared cache) and one scope's list, page by page (`before` = the last id
// the server read). The server decides on every read which rows the person
// may still see; the screen shows only what it is given.
import { queryOptions, useInfiniteQuery, useQuery } from '@tanstack/react-query';

import { call, type ApiClient } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type UserMergeRequest = S['UserMergeRequestBody'];
export type UserMergeRequestCount = S['UserMergeRequestCount'];
export type UserMergeRequestCounts = S['UserMergeRequestCounts'];

/** The scopes, in the order the screens show them: what waits on me first. */
export const MR_SCOPES = ['attention', 'approvable', 'authored'] as const;
export type MrScope = (typeof MR_SCOPES)[number];

export function isMrScope(value: string | null): value is MrScope {
  return value !== null && (MR_SCOPES as readonly string[]).includes(value);
}

export const USER_MRS_KEY = ['user-merge-requests'] as const;

/** Rows asked for at a time (the API allows 1-100). */
export const USER_MR_PAGE = 20;
/** Counts are read again when older than this (and on window focus). */
const COUNTS_STALE_MS = 60_000;

export function userMrCountsQuery(api: ApiClient) {
  return queryOptions({
    queryKey: [...USER_MRS_KEY, 'counts'] as const,
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/user/merge-requests/counts', { signal })),
    staleTime: COUNTS_STALE_MS,
  });
}

export function useUserMrCounts() {
  const api = useApi();
  return useQuery(userMrCountsQuery(api));
}

/** One scope's list; read only while `enabled` (its section is open). */
export function useUserMergeRequests(scope: MrScope, enabled: boolean) {
  const api = useApi();
  return useInfiniteQuery({
    queryKey: [...USER_MRS_KEY, 'list', scope] as const,
    initialPageParam: undefined as number | undefined,
    queryFn: ({ signal, pageParam }) =>
      call(() =>
        api.GET('/api/v1/user/merge-requests', {
          signal,
          params: {
            query: {
              scope,
              limit: USER_MR_PAGE,
              ...(pageParam === undefined ? {} : { before: pageParam }),
            },
          },
        }),
      ),
    getNextPageParam: (last) => last.next_before ?? undefined,
    enabled,
  });
}
