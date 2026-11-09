// The signed-in person's in-app notifications (M3 U4): the list (newest
// first, `before` = the last row id the server read), the unread count the
// header's bell polls, and marking items read. The server decides on every
// read which items the person may still see; the screen shows only what it
// is given. No websockets: the count is polled, slower after failures.
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useRef } from 'react';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call, callNoContent, failureOf } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type InAppNotification = S['InAppNotificationBody'];
export type UnreadCount = S['UnreadCountBody'];

export const NOTIFICATIONS_KEY = ['in-app-notifications'] as const;
export const notificationKeys = {
  count: [...NOTIFICATIONS_KEY, 'count'] as const,
  list: (unread: boolean) => [...NOTIFICATIONS_KEY, 'list', unread ? 'unread' : 'all'] as const,
};

/** Rows asked for at a time (the API allows 1-100). */
export const NOTIFICATION_PAGE = 20;
/** The bell asks again this often while the answers are good. */
export const POLL_MS = 60_000;
/** The longest wait after repeated failures. */
export const MAX_POLL_MS = 15 * 60_000;

/** The wait before the next poll after `failures` failures in a row:
 * the base period, doubled per failure up to {@link MAX_POLL_MS}. */
export function pollDelay(failures: number): number {
  if (failures <= 0) return POLL_MS;
  return Math.min(POLL_MS * 2 ** Math.min(failures, 10), MAX_POLL_MS);
}

/** The unread count, polled with backoff. Stops when the session ended (the
 * page's own queries show the sign-in screen). */
export function useUnreadCount() {
  const api = useApi();
  const failures = useRef(0);
  return useQuery({
    queryKey: notificationKeys.count,
    queryFn: async ({ signal }) => {
      try {
        const count = await call(() => api.GET('/api/v1/user/notifications/unread-count', { signal }));
        failures.current = 0;
        return count;
      } catch (e) {
        failures.current += 1;
        throw e;
      }
    },
    // A poll is no blip to retry at once: the next one comes after the backoff.
    retry: false,
    staleTime: 0,
    // A hidden tab does not poll (the count is asked again when it shows).
    refetchIntervalInBackground: false,
    refetchInterval: (query) => {
      const error = query.state.error;
      if (error instanceof ApiFailure && error.failure.kind === 'unauthenticated') return false;
      return pollDelay(failures.current);
    },
  });
}

/** The list (all or unread only), page by page. */
export function useNotifications(unread: boolean) {
  const api = useApi();
  return useInfiniteQuery({
    queryKey: notificationKeys.list(unread),
    initialPageParam: undefined as number | undefined,
    queryFn: ({ signal, pageParam }) =>
      call(() =>
        api.GET('/api/v1/user/notifications', {
          signal,
          params: {
            query: {
              limit: NOTIFICATION_PAGE,
              ...(unread ? { unread: 'true' } : {}),
              ...(pageParam === undefined ? {} : { before: pageParam }),
            },
          },
        }),
      ),
    getNextPageParam: (last) => last.next_before ?? undefined,
    staleTime: 0,
  });
}

function useRefreshNotifications() {
  const queryClient = useQueryClient();
  return () => queryClient.invalidateQueries({ queryKey: NOTIFICATIONS_KEY });
}

/** A refused mark, wherever the screen went meanwhile (following an item's
 * link marks it and leaves the page): an ended session or a stale CSRF
 * token reads the session again, the list and the count are read again (the
 * item shows unread), and the failure is logged with its request id. The
 * screen still shows it when it is there to. */
function useOnMarkFailed() {
  const queryClient = useQueryClient();
  return (error: unknown) => {
    const failure = failureOf(error);
    if (failure.kind === 'unauthenticated' || (failure.kind === 'api' && failure.status === 403)) {
      void queryClient.invalidateQueries({ queryKey: SESSION_KEY });
    }
    void queryClient.invalidateQueries({ queryKey: NOTIFICATIONS_KEY });
    const requestId = 'requestId' in failure ? failure.requestId : null;
    console.warn(`gitstation: marking a notification read failed (${failure.kind}, request ${requestId ?? 'none'})`);
  };
}

/** Mark one item read (the answer carries only its id and time). */
export function useMarkRead() {
  const api = useApi();
  const refresh = useRefreshNotifications();
  const onError = useOnMarkFailed();
  return useMutation({
    mutationFn: (id: number) =>
      call(() =>
        api.POST('/api/v1/user/notifications/{id}/read', { params: { path: { id } } }),
      ),
    onSuccess: refresh,
    onError,
  });
}

/** Mark every unread item read, up to the newest one the person saw. */
export function useMarkAllRead() {
  const api = useApi();
  const refresh = useRefreshNotifications();
  const onError = useOnMarkFailed();
  return useMutation({
    // 204: no count (it would include items the person may no longer see).
    mutationFn: (upTo: number | undefined) =>
      callNoContent(() =>
        api.POST('/api/v1/user/notifications/read-all', {
          body: upTo === undefined ? {} : { up_to: upTo },
        }),
      ),
    onSuccess: refresh,
    onError,
  });
}
