// The signed-in person's activity (plan 15 MR 8/9): the events of the
// projects they take part in, newest first, page by page (`before` = the
// server's next_before, a keyset on the event's seq). The server decides on
// every read which events the person may see; the screen shows what it gets.
import { useInfiniteQuery, useQuery } from '@tanstack/react-query';

import { call } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type ActivityEvent = S['UserActivityEventBody'];
export type ActivityList = S['UserActivityList'];

export const USER_ACTIVITY_KEY = ['user-activity'] as const;

/** Events asked for per page on /-/activity (plan 15 §3.3: 30). */
export const ACTIVITY_PAGE = 30;
/** Events on Home's "recent updates" (plan 15 §3.3: 10). */
export const ACTIVITY_RECENT = 10;

/** /-/activity: every page read so far. */
export function useUserActivity() {
  const api = useApi();
  return useInfiniteQuery({
    queryKey: [...USER_ACTIVITY_KEY, 'list'] as const,
    initialPageParam: undefined as number | undefined,
    queryFn: ({ signal, pageParam }) =>
      call(() =>
        api.GET('/api/v1/user/activity', {
          signal,
          params: {
            query: { limit: ACTIVITY_PAGE, ...(pageParam === undefined ? {} : { before: pageParam }) },
          },
        }),
      ),
    getNextPageParam: (last) => last.next_before ?? undefined,
  });
}

/** Home: the newest few events (one page, no paging). */
export function useRecentActivity() {
  const api = useApi();
  return useQuery({
    queryKey: [...USER_ACTIVITY_KEY, 'recent'] as const,
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/user/activity', { signal, params: { query: { limit: ACTIVITY_RECENT } } })),
  });
}
