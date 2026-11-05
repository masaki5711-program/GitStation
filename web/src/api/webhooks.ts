// A project's webhooks and their delivery history (U3a, docs/api/webhooks.md)
// and the signed-in person's notification settings (N1). Maintainer+ for the
// webhooks (the server decides again on every request). The URL is write-only:
// a read shows the `origin`. The secret of a create or a rotation is in the
// answer once; the mutations that carry it use `gcTime: 0` and the screen
// holds it in component state only (never the query cache).
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call, callNoContent } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type Webhook = S['WebhookBody'];
export type Delivery = S['DeliveryBody'];
export type CreateWebhookBody = S['CreateWebhook'];
export type UpdateWebhookBody = S['UpdateWebhook'];
export type NotificationPreferences = S['PreferencesBody'];
export type NotificationPreferencesInput = S['PreferencesInput'];

export const webhookKeys = {
  list: (project: number) => ['webhooks', project] as const,
  // Nested under the list key, so invalidating the list refreshes it too.
  one: (project: number, hook: number) => ['webhooks', project, 'one', hook] as const,
  deliveries: (project: number, hook: number) => ['webhook-deliveries', project, hook] as const,
};
export const PREFERENCES_KEY = ['notification-preferences'] as const;

/** Deliveries asked for at a time (the API allows 1-100). */
export const DELIVERY_PAGE = 20;

export function useWebhooks(project: number) {
  const api = useApi();
  return useQuery({
    queryKey: webhookKeys.list(project),
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}/webhooks', { signal, params: { path: { id: project } } })),
  });
}

export function useWebhook(project: number, hook: number) {
  const api = useApi();
  return useQuery({
    queryKey: webhookKeys.one(project, hook),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/webhooks/{webhook_id}', {
          signal,
          params: { path: { id: project, webhook_id: hook } },
        }),
      ),
  });
}

function useInvalidateWebhooks(project: number) {
  const queryClient = useQueryClient();
  return () => queryClient.invalidateQueries({ queryKey: ['webhooks', project] });
}

/** Create: the answer holds the secret, so the mutation cache keeps nothing. */
export function useCreateWebhook(project: number) {
  const api = useApi();
  const invalidate = useInvalidateWebhooks(project);
  return useMutation({
    gcTime: 0,
    mutationFn: (body: CreateWebhookBody) =>
      call(() => api.POST('/api/v1/projects/{id}/webhooks', { params: { path: { id: project } }, body })),
    onSuccess: invalidate,
  });
}

export function useUpdateWebhook(project: number) {
  const api = useApi();
  const invalidate = useInvalidateWebhooks(project);
  return useMutation({
    mutationFn: (v: { hook: number; body: UpdateWebhookBody }) =>
      call(() =>
        api.PATCH('/api/v1/projects/{id}/webhooks/{webhook_id}', {
          params: { path: { id: project, webhook_id: v.hook } },
          body: v.body,
        }),
      ),
    onSuccess: invalidate,
  });
}

export function useDeleteWebhook(project: number) {
  const api = useApi();
  const invalidate = useInvalidateWebhooks(project);
  return useMutation({
    mutationFn: (hook: number) =>
      callNoContent(() =>
        api.DELETE('/api/v1/projects/{id}/webhooks/{webhook_id}', {
          params: { path: { id: project, webhook_id: hook } },
        }),
      ),
    onSuccess: invalidate,
  });
}

/** Rotate: the answer holds the new secret (nothing is cached). */
export function useRotateWebhookSecret(project: number) {
  const api = useApi();
  const invalidate = useInvalidateWebhooks(project);
  return useMutation({
    gcTime: 0,
    mutationFn: (hook: number) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/webhooks/{webhook_id}/secret', {
          params: { path: { id: project, webhook_id: hook } },
        }),
      ),
    onSuccess: invalidate,
  });
}

/** A webhook's delivery history, newest first, `before` = the last `id` seen. */
export function useDeliveries(project: number, hook: number) {
  const api = useApi();
  return useInfiniteQuery({
    queryKey: webhookKeys.deliveries(project, hook),
    initialPageParam: undefined as number | undefined,
    queryFn: ({ signal, pageParam }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/webhooks/{webhook_id}/deliveries', {
          signal,
          params: {
            path: { id: project, webhook_id: hook },
            query: { limit: DELIVERY_PAGE, ...(pageParam === undefined ? {} : { before: pageParam }) },
          },
        }),
      ),
    getNextPageParam: (last) => last.next_before ?? undefined,
  });
}

export function useRedeliver(project: number, hook: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (delivery: number) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/webhooks/{webhook_id}/deliveries/{delivery_id}/redeliver', {
          params: { path: { id: project, webhook_id: hook, delivery_id: delivery } },
        }),
      ),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: webhookKeys.deliveries(project, hook) }),
        queryClient.invalidateQueries({ queryKey: webhookKeys.list(project) }),
      ]);
    },
    // A 403 is most often a stale CSRF token: read the session again.
    onError: (error: unknown) => {
      if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 403) {
        void queryClient.invalidateQueries({ queryKey: SESSION_KEY });
      }
    },
  });
}

export function useNotificationPreferences() {
  const api = useApi();
  return useQuery({
    queryKey: PREFERENCES_KEY,
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/user/notification-preferences', { signal })),
  });
}

export function usePutNotificationPreferences() {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (body: NotificationPreferencesInput) =>
      call(() => api.PUT('/api/v1/user/notification-preferences', { body })),
    onSuccess: (next) => {
      queryClient.setQueryData(PREFERENCES_KEY, next);
    },
  });
}
