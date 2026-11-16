// A project's pipeline schedules (M4 C12): the list (project readers) and,
// for a Maintainer, create / change / delete. Whoever saves a schedule
// becomes its owner: it runs with that person's rights at each run.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { call, callNoContent } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type PipelineSchedule = S['PipelineScheduleBody'];
export type PipelineScheduleInput = S['PipelineScheduleInput'];

export const scheduleKeys = {
  list: (project: number) => ['pipelines', project, 'schedules'] as const,
};

/** The offsets offered (the sites' zones; any quarter hour is accepted by
 * the API). */
export const OFFSETS: readonly { minutes: number; label: string }[] = [
  { minutes: 540, label: 'UTC+09:00 (JST)' },
  { minutes: 420, label: 'UTC+07:00 (WIB)' },
  { minutes: 0, label: 'UTC' },
];

/** A role that may manage schedules (Maintainer or above; the server
 * decides again and refuses tokens of agents). */
export function mayManageSchedules(role: string | null | undefined, isAdmin: boolean): boolean {
  return isAdmin || role === 'maintainer' || role === 'owner';
}

export function useSchedules(project: number) {
  const api = useApi();
  return useQuery({
    queryKey: scheduleKeys.list(project),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/pipeline-schedules', {
          signal,
          params: { path: { id: project } },
        }),
      ),
  });
}

export function useSaveSchedule(project: number) {
  const api = useApi();
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ id, body }: { id: number | null; body: PipelineScheduleInput }) =>
      id === null
        ? call(() =>
            api.POST('/api/v1/projects/{id}/pipeline-schedules', {
              params: { path: { id: project } },
              body,
            }),
          )
        : call(() =>
            api.PUT('/api/v1/projects/{id}/pipeline-schedules/{schedule_id}', {
              params: { path: { id: project, schedule_id: id } },
              body,
            }),
          ),
    onSuccess: () => client.invalidateQueries({ queryKey: scheduleKeys.list(project) }),
  });
}

export function useDeleteSchedule(project: number) {
  const api = useApi();
  const client = useQueryClient();
  return useMutation({
    mutationFn: (id: number) =>
      callNoContent(() =>
        api.DELETE('/api/v1/projects/{id}/pipeline-schedules/{schedule_id}', {
          params: { path: { id: project, schedule_id: id } },
        }),
      ),
    onSuccess: () => client.invalidateQueries({ queryKey: scheduleKeys.list(project) }),
  });
}
