// The merge queue (M4 C8, ADR-0030): one merge request's entry ("merge
// when ready"), a target branch's queue, and its pause / resume. Keys start
// with ['mergeQueue', projectId]. Reads poll while something moves (an
// entry queued or being tested).
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { call } from './client';
import { useApi } from './context';
import { mrKeys } from './mergeRequests';
import type { components } from './schema.gen';

type S = components['schemas'];
export type QueueEntry = S['QueueEntryBody'];
export type QueueEntryStatus = S['QueueEntryStatusBody'];
export type MergeQueue = S['MergeQueueBody'];

/** How often a moving queue (or entry) is read again (ms). */
export const QUEUE_POLL_MS = 5000;

export const queueKeys = {
  all: (project: number) => ['mergeQueue', project] as const,
  entry: (project: number, iid: number) => ['mergeQueue', project, 'entry', iid] as const,
  queue: (project: number, target: string) => ['mergeQueue', project, 'queue', target] as const,
};

/** An entry that may still change by itself. */
export const isLive = (state: string) => state === 'queued' || state === 'testing';

export function useQueueEntry(project: number, iid: number) {
  const api = useApi();
  return useQuery({
    queryKey: queueKeys.entry(project, iid),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/merge-queue', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
    refetchInterval: (q) => {
      const state = q.state.data?.entry?.state;
      return state !== undefined && isLive(state) ? QUEUE_POLL_MS : false;
    },
  });
}

export function useMergeQueue(project: number, target: string) {
  const api = useApi();
  return useQuery({
    queryKey: queueKeys.queue(project, target),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-queue', {
          signal,
          params: { path: { id: project }, query: { target } },
        }),
      ),
    refetchInterval: (q) =>
      (q.state.data?.entries.length ?? 0) > 0 ? QUEUE_POLL_MS : false,
  });
}

export function useEnqueue(project: number, iid: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (sha: string) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/merge-requests/{iid}/merge-queue', {
          params: { path: { id: project, iid: String(iid) } },
          body: { sha },
        }),
      ),
    onSuccess: () =>
      Promise.all([
        queryClient.invalidateQueries({ queryKey: queueKeys.all(project) }),
        queryClient.invalidateQueries({ queryKey: mrKeys.status(project, iid) }),
      ]),
  });
}

export function useDequeue(project: number, iid: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () =>
      call(() =>
        api.DELETE('/api/v1/projects/{id}/merge-requests/{iid}/merge-queue', {
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: queueKeys.all(project) }),
  });
}

export function useSetPaused(project: number, target: string) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (paused: boolean) =>
      call(() =>
        paused
          ? api.POST('/api/v1/projects/{id}/merge-queue/pause', {
              params: { path: { id: project } },
              body: { target },
            })
          : api.POST('/api/v1/projects/{id}/merge-queue/resume', {
              params: { path: { id: project } },
              body: { target },
            }),
      ),
    onSuccess: (queue) => {
      queryClient.setQueryData(queueKeys.queue(project, target), queue);
    },
  });
}
