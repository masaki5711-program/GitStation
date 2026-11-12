// CI pipelines (plan 14 M4): the list, one pipeline with its jobs, its
// diagnosis, a job's log, cancel and retry. Keys start with
// ['pipelines', projectId] so one invalidation reaches everything of a
// project. Reads poll only while something is still moving.
import { queryOptions, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useState } from 'react';

import { call, type ApiClient } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type Pipeline = S['PipelineBody'];
export type PipelinePage = S['PipelinePage'];
export type PipelineJob = S['PipelineJobBody'];
export type Diagnosis = S['DiagnosisBody'];
export type CiFailure = S['FailureBody'];
export type JobLogPage = S['JobLogPage'];

/** How often a moving pipeline (and its log) is read again (ms). */
export const PIPELINE_POLL_MS = 3000;
/** How often the list is read again while one of its pipelines moves (ms). */
export const LIST_POLL_MS = 10000;
/** The log pages one view holds (each at most 1 MiB): beyond, a note. */
export const LOG_MAX_PAGES = 4;

export const pipelineKeys = {
  all: (project: number) => ['pipelines', project] as const,
  list: (project: number, before: number | undefined) =>
    ['pipelines', project, 'list', before ?? null] as const,
  one: (project: number, id: number) => ['pipelines', project, id, 'one'] as const,
  diagnosis: (project: number, id: number) => ['pipelines', project, id, 'diagnosis'] as const,
  log: (project: number, id: number, job: number, from: number) =>
    ['pipelines', project, id, 'log', job, from] as const,
};

/** A pipeline or job id from the address: a positive decimal without a
 * leading zero that fits a safe integer; anything else is `null`. */
export function parseId(raw: string | null | undefined): number | null {
  if (raw === null || raw === undefined || !/^[1-9][0-9]{0,15}$/.test(raw)) return null;
  const n = Number(raw);
  return Number.isSafeInteger(n) ? n : null;
}

/** A pipeline that may still change by itself. */
export const isMoving = (status: string) => status === 'pending' || status === 'running';
/** A job that may still write to its log. */
export const isJobMoving = (status: string) =>
  status === 'created' || status === 'pending' || status === 'running';

/** What the API allows (docs/api/openapi.json): cancel a pending or running
 * pipeline, retry a failed or cancelled one — both for a Developer or
 * above. The server decides again; a refusal shows its user_message. */
export const canCancel = (status: string) => isMoving(status);
export const canRetry = (status: string) => status === 'failed' || status === 'canceled';

/** A role that may cancel and retry (Developer or above). */
export function mayOperate(role: string | null | undefined, isAdmin: boolean): boolean {
  return isAdmin || role === 'developer' || role === 'maintainer' || role === 'owner';
}

/** `refs/heads/main` → `main`. */
export const branchOf = (ref: string) => (ref.startsWith('refs/heads/') ? ref.slice(11) : ref);

/** The pipeline a `ci/pipeline` check run reports (forge-pipelines writes
 * `Pipeline #{id} {status}: …` as its summary and no details URL). */
export function pipelineIdOfCheck(run: { name: string; summary: string }): number | null {
  if (run.name !== 'ci/pipeline') return null;
  const m = /^Pipeline #([1-9][0-9]{0,15}) /.exec(run.summary);
  return parseId(m?.[1]);
}

export const pipelinesQuery = (api: ApiClient, project: number, before: number | undefined) =>
  queryOptions({
    queryKey: pipelineKeys.list(project, before),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/pipelines', {
          signal,
          params: { path: { id: project }, query: before === undefined ? {} : { before } },
        }),
      ),
    refetchInterval: (q) =>
      q.state.data?.pipelines.some((p) => isMoving(p.status)) === true ? LIST_POLL_MS : false,
  });

export function usePipelines(project: number, before: number | undefined) {
  return useQuery(pipelinesQuery(useApi(), project, before));
}

export function usePipeline(project: number, id: number) {
  const api = useApi();
  return useQuery({
    queryKey: pipelineKeys.one(project, id),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/pipelines/{pipeline_id}', {
          signal,
          params: { path: { id: project, pipeline_id: id } },
        }),
      ),
    refetchInterval: (q) => {
      const status = q.state.data?.status;
      return status !== undefined && isMoving(status) ? PIPELINE_POLL_MS : false;
    },
  });
}

/** Asked only for a failed pipeline; read again while the worker has not
 * fingerprinted a failure yet (`pending`). */
export function useDiagnosis(project: number, id: number, enabled: boolean) {
  const api = useApi();
  return useQuery({
    queryKey: pipelineKeys.diagnosis(project, id),
    enabled,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/pipelines/{pipeline_id}/diagnosis', {
          signal,
          params: { path: { id: project, pipeline_id: id } },
        }),
      ),
    refetchInterval: (q) =>
      q.state.data?.failures.some((f) => f.pending) === true ? PIPELINE_POLL_MS : false,
  });
}

/** A job's log (its current attempt), page by page (`next_from`). Only the
 * last page is asked for: the pages before it are over and kept as they
 * are (appended). While the job runs and nothing is stored after the last
 * page, that page alone is read again; when it fills up (`next_from`) the
 * view moves on by itself. At most [`LOG_MAX_PAGES`] are held. */
export function useJobLog(project: number, id: number, job: number) {
  const api = useApi();
  /** The pages before the last one (each has a `next_from`). */
  const [done, setDone] = useState<JobLogPage[]>([]);
  const from = done[done.length - 1]?.next_from ?? 0;
  const tail = useQuery({
    queryKey: pipelineKeys.log(project, id, job, from),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/pipelines/{pipeline_id}/jobs/{job_id}/log', {
          signal,
          params: {
            path: { id: project, pipeline_id: id, job_id: job },
            query: from === 0 ? {} : { from },
          },
        }),
      ),
    refetchInterval: (q) => {
      const last = q.state.data;
      if (last === undefined || last.complete || last.next_from !== null) return false;
      return isJobMoving(last.job_status) ? PIPELINE_POLL_MS : false;
    },
  });
  const last = tail.data;
  const room = done.length + 1 < LOG_MAX_PAGES;
  const hasMore = last !== undefined && last.next_from !== null && room;
  const capped = last !== undefined && last.next_from !== null && !room;
  const more = () => {
    if (last !== undefined && hasMore) setDone((d) => [...d, last]);
  };
  // A running job's full page: follow on to the next one by itself.
  const follow = hasMore && isJobMoving(last.job_status);
  useEffect(() => {
    if (follow) setDone((d) => (d.includes(last) ? d : [...d, last]));
  }, [follow, last]);
  return { done, tail, hasMore, capped, more };
}

function usePipelineAction(project: number, id: number, action: 'cancel' | 'retry') {
  const api = useApi();
  const client = useQueryClient();
  return useMutation({
    mutationFn: () =>
      action === 'cancel'
        ? call(() =>
            api.POST('/api/v1/projects/{id}/pipelines/{pipeline_id}/cancel', {
              params: { path: { id: project, pipeline_id: id } },
            }),
          )
        : call(() =>
            api.POST('/api/v1/projects/{id}/pipelines/{pipeline_id}/retry', {
              params: { path: { id: project, pipeline_id: id } },
            }),
          ),
    onSuccess: (pipeline) => {
      client.setQueryData(pipelineKeys.one(project, id), pipeline);
      void client.invalidateQueries({ queryKey: pipelineKeys.all(project) });
    },
  });
}

export const useCancelPipeline = (project: number, id: number) =>
  usePipelineAction(project, id, 'cancel');
export const useRetryPipeline = (project: number, id: number) =>
  usePipelineAction(project, id, 'retry');
