// A project's merge settings (U2): methods, approvals on push, committers'
// approvals, CODEOWNERS enforcement, approval rules and the merge conditions
// of protected branches. `updated_at` is an opaque string: it is sent back
// verbatim as `expected_updated_at` (never through a Date, which would round
// it to milliseconds and always conflict; docs/api/merge-status.md).
import { useQuery } from '@tanstack/react-query';

import { call } from './client';
import { useApi } from './context';

export const settingsKeys = {
  merge: (project: number) => ['merge-settings', project] as const,
  approvalRules: (project: number) => ['approval-rules', project] as const,
  protectedRefs: (project: number) => ['protected-refs', project] as const,
};

/** The merge methods forge knows, in the order a screen lists them. */
export const MERGE_METHODS = ['merge_commit', 'squash', 'fast_forward'] as const;
export type MergeMethod = (typeof MERGE_METHODS)[number];

export function isMergeMethod(value: string): value is MergeMethod {
  return (MERGE_METHODS as readonly string[]).includes(value);
}

export function useMergeSettings(project: number, enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: settingsKeys.merge(project),
    enabled,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-settings', { signal, params: { path: { id: project } } }),
      ),
  });
}
