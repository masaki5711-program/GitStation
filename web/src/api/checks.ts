// The check runs of a merge request's head (docs/api/checks.md) and its
// predicted conflicts with other open merge requests (M3 X6). Keys sit under
// ['mr', project, iid] so a reload of the merge request reaches them.
import { useQuery } from '@tanstack/react-query';

import { call } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type CheckRun = S['CheckRunBody'];
export type CheckAnnotation = S['AnnotationBody'];
export type Conflicts = S['ConflictsBody'];
export type ConflictPrediction = S['ConflictPredictionBody'];

export const checkKeys = {
  runs: (project: number, iid: number, sha: string) => ['mr', project, iid, 'check-runs', sha] as const,
  conflicts: (project: number, iid: number) => ['mr', project, iid, 'conflicts'] as const,
};

/** How often the runs are read again while one is still queued or running (ms). */
export const CHECK_RUNS_POLL_MS = 15_000;

/** A run that has not finished yet. */
export const isRunning = (run: Pick<CheckRun, 'status'>) => run.status !== 'completed';

export function useCheckRuns(project: number, iid: number, sha: string | null) {
  const api = useApi();
  return useQuery({
    queryKey: checkKeys.runs(project, iid, sha ?? ''),
    enabled: sha !== null,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/commits/{sha}/checks', {
          signal,
          params: { path: { id: project, sha: sha ?? '' } },
        }),
      ),
    refetchInterval: (query) =>
      query.state.data?.check_runs.some(isRunning) === true ? CHECK_RUNS_POLL_MS : false,
  });
}

export function useConflicts(project: number, iid: number) {
  const api = useApi();
  return useQuery({
    queryKey: checkKeys.conflicts(project, iid),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/conflicts', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
  });
}

/** `ci/build` → `ci`; a name without a namespace → null. */
export function namespaceOf(name: string): string | null {
  const slash = name.indexOf('/');
  return slash > 0 ? name.slice(0, slash) : null;
}

/** Only http(s) links are shown as links (the server allows nothing else). */
export function safeDetailsUrl(url: string | null): string | null {
  if (url === null) return null;
  return /^https?:\/\/[^\s]+$/i.test(url) ? url : null;
}

/** Failing runs first (what needs attention), then running, then the rest; by name inside. */
export function orderRuns(runs: readonly CheckRun[]): CheckRun[] {
  const rank = (r: CheckRun) =>
    r.status !== 'completed' ? 1 : r.conclusion === 'success' || r.conclusion === 'neutral' ? 2 : 0;
  return [...runs].sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
}
