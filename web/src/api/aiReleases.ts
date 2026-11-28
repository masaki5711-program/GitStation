// A project's signed pfs-ai-bundle releases (plan 17 P17-7 / P17-7b): the
// list with each release's state on shadow / canary / prod, what runs on
// each stage and what a rollback would request, what the caller may press;
// a stage request, a rollback and a withdrawal. Keys start with
// ['aiReleases', projectId]; a change also reads the deployments again
// (a request is a deployment waiting for an approval there).
import { queryOptions, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call, type ApiClient } from './client';
import { useApi } from './context';
import { deploymentKeys, isDeployMoving } from './deployments';
import type { components } from './schema.gen';

type S = components['schemas'];
export type ReleaseList = S['ReleaseList'];
export type Release = S['ReleaseBody'];
export type ReleaseStage = S['ReleaseStageBody'];
export type Stage = S['StageBody'];
export type Withdrawal = S['WithdrawalBody'];

/** While a deployment of a release moves (ms): the same pace as the list
 * of deployments. */
export const RELEASES_POLL_MS = 15000;
/** The list's length at most (the API's newest 100). */
export const RELEASES_MAX = 100;
/** The longest withdrawal reason (characters; the server checks it again). */
export const WITHDRAW_REASON_MAX = 500;

export const aiReleaseKeys = {
  all: (project: number) => ['aiReleases', project] as const,
};

/** Whether any release has a deployment under way (read again meanwhile). */
export const releasesMoving = (data: ReleaseList | undefined) =>
  data?.releases.some((r) => (r.stages ?? []).some((s) => s.status != null && isDeployMoving(s.status))) === true;

export const aiReleasesQuery = (api: ApiClient, project: number) =>
  queryOptions({
    queryKey: aiReleaseKeys.all(project),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/ai-asset-releases', { signal, params: { path: { id: project } } }),
      ),
    refetchInterval: (q) => (releasesMoving(q.state.data) ? RELEASES_POLL_MS : false),
  });

export function useAiReleases(project: number) {
  return useQuery(aiReleasesQuery(useApi(), project));
}

/** Whether a reason may be sent: 1..=500 characters, not only blanks, one
 * line (no control character). */
export function withdrawReasonValid(reason: string): boolean {
  // Code points, as the server counts characters.
  const chars = Array.from(reason).length;
  // eslint-disable-next-line no-control-regex -- the server's own rule: no control character
  return reason.trim() !== '' && chars <= WITHDRAW_REASON_MAX && !/[\u0000-\u001f\u007f-\u009f]/.test(reason);
}

function useAfterChange(project: number) {
  const client = useQueryClient();
  return {
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: aiReleaseKeys.all(project) });
      void client.invalidateQueries({ queryKey: deploymentKeys.all(project) });
    },
    // A 403 on a change is most often a stale CSRF token: read the session again.
    onError: (error: unknown) => {
      if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 403) {
        void client.invalidateQueries({ queryKey: SESSION_KEY });
      }
    },
  };
}

/** `POST …/{version}/deployments`: the release on its next stage, waiting
 * for a person's approval on the deployment screen. */
export function useRequestStage(project: number) {
  const api = useApi();
  return useMutation({
    mutationFn: ({ version, environment }: { version: string; environment: string }) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/ai-asset-releases/{version}/deployments', {
          params: { path: { id: project, version } },
          body: { environment },
        }),
      ),
    ...useAfterChange(project),
  });
}

/** `POST …/ai-asset-rollbacks`: the release last approved before the
 * running one, waiting for a person's approval. */
export function useRollbackStage(project: number) {
  const api = useApi();
  return useMutation({
    mutationFn: (environment: string) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/ai-asset-rollbacks', {
          params: { path: { id: project } },
          body: { environment },
        }),
      ),
    ...useAfterChange(project),
  });
}

/** `POST …/{version}/withdrawal`: for good. */
export function useWithdrawRelease(project: number) {
  const api = useApi();
  return useMutation({
    mutationFn: ({ version, reason }: { version: string; reason: string }) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/ai-asset-releases/{version}/withdrawal', {
          params: { path: { id: project, version } },
          body: { reason },
        }),
      ),
    ...useAfterChange(project),
  });
}
