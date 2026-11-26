// Deployments (ADR-0038, plan 19 §3.4 / L5-6): a project's list, one
// deployment's approval facts with the local LLM's summary apart from them,
// and the approval itself. Keys start with ['deployments', projectId] so an
// approval reaches the list and the detail at once.
import { queryOptions, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { ApiFailure, call, type ApiClient } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';
import { SESSION_KEY } from '../session/session';

type S = components['schemas'];
export type Deployment = S['DeploymentBody'];
export type DeploymentList = S['DeploymentList'];
export type ApprovalSummary = S['ApprovalSummaryBody'];
export type ApprovalFacts = S['ApprovalFactsBody'];
export type SummaryAdvice = S['ApprovalSummaryAdvice'];

/** While a deployment waits for an approval and its summary has not come
 * yet, the detail is read again this often (ms): the worker looks every
 * 30 s (ADR-0039 amendment L5-8, decision 3). Never faster: each read makes
 * the facts again (git compare). */
export const SUMMARY_POLL_MS = 30000;
/** While a deployment on the list moves (waits, queued, running) (ms). */
export const DEPLOY_LIST_POLL_MS = 15000;

export const deploymentKeys = {
  all: (project: number) => ['deployments', project] as const,
  list: (project: number) => ['deployments', project, 'list'] as const,
  summary: (project: number, id: number) => ['deployments', project, id, 'approval-summary'] as const,
};

/** A deployment that may still change by itself. */
export const isDeployMoving = (status: string) =>
  status === 'pending_approval' || status === 'queued' || status === 'running';

export const deploymentsQuery = (api: ApiClient, project: number) =>
  queryOptions({
    queryKey: deploymentKeys.list(project),
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}/deployments', { signal, params: { path: { id: project } } })),
    refetchInterval: (q) =>
      q.state.data?.deployments.some((d) => isDeployMoving(d.status)) === true ? DEPLOY_LIST_POLL_MS : false,
  });

export function useDeployments(project: number) {
  return useQuery(deploymentsQuery(useApi(), project));
}

/** Read again only while it waits for an approval and no summary is there
 * yet: the approver is not made to wait for it (approving never does). */
export function summaryPollInterval(data: ApprovalSummary | undefined): number | false {
  if (data === undefined) return false;
  return data.deployment.status === 'pending_approval' && data.summary.state === 'none'
    ? SUMMARY_POLL_MS
    : false;
}

export function useApprovalSummary(project: number, id: number) {
  const api = useApi();
  return useQuery({
    queryKey: deploymentKeys.summary(project, id),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/deployments/{deployment_id}/approval-summary', {
          signal,
          params: { path: { id: project, deployment_id: id } },
        }),
      ),
    refetchInterval: (q) => summaryPollInterval(q.state.data),
  });
}

/** `POST …/approve` from this web session (the server refuses a token:
 * FG-AUTH-0002). It never sends anything of the summary. */
export function useApproveDeployment(project: number, id: number) {
  const api = useApi();
  const client = useQueryClient();
  return useMutation({
    mutationFn: () =>
      call(() =>
        api.POST('/api/v1/projects/{id}/deployments/{deployment_id}/approve', {
          params: { path: { id: project, deployment_id: id } },
        }),
      ),
    onSuccess: (deployment) => {
      client.setQueryData<ApprovalSummary>(deploymentKeys.summary(project, id), (old) =>
        old === undefined ? old : { ...old, deployment },
      );
      void client.invalidateQueries({ queryKey: deploymentKeys.all(project) });
    },
    onError: (error) => {
      // A 403 is most often a stale CSRF token: read the session again.
      if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 403) {
        void client.invalidateQueries({ queryKey: SESSION_KEY });
      }
    },
  });
}
