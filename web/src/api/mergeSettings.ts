// A project's merge settings (U2): methods, approvals on push, committers'
// approvals, CODEOWNERS enforcement, approval rules and the merge conditions
// of protected branches. `updated_at` (of the settings, of each approval rule,
// of each merge rule) is an opaque string: it is sent back verbatim as
// `expected_updated_at` (never through a Date, which would round it to
// milliseconds and always conflict; docs/api/merge-status.md). `null` says
// "there is none yet": a rule write then only creates.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call, callNoContent } from './client';
import { useApi } from './context';
import type { MergeSettings, PutApprovalRuleBody, PutMergeRuleBody, PutMergeSettingsBody } from './types';

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

/** A 403 on a save is most often a stale CSRF token: read the session again. */
function useRefreshSessionOn403() {
  const queryClient = useQueryClient();
  return (error: unknown) => {
    if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 403) {
      void queryClient.invalidateQueries({ queryKey: SESSION_KEY });
    }
  };
}

/** Replace the settings. `body.expected_updated_at` is the `updated_at` that
 * was read, as it was read (a 409 means someone saved in between). */
export function usePutMergeSettings(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (body: PutMergeSettingsBody) =>
      call(() =>
        api.PUT('/api/v1/projects/{id}/merge-settings', { params: { path: { id: project } }, body }),
      ),
    onSuccess: (next: MergeSettings) => {
      queryClient.setQueryData(settingsKeys.merge(project), next);
    },
    onError: refreshSession,
  });
}

/** Suspend CODEOWNERS for 1-1440 minutes, or enforce it again. */
export function useCodeownersEnforcement(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (body: { enforced: true } | { enforced: false; for_minutes: number }) =>
      call(() =>
        api.PUT('/api/v1/projects/{id}/merge-settings/codeowners-enforcement', {
          params: { path: { id: project } },
          body,
        }),
      ),
    onSuccess: (next: MergeSettings) => {
      queryClient.setQueryData(settingsKeys.merge(project), next);
    },
    onError: refreshSession,
  });
}

export function useApprovalRules(project: number) {
  const api = useApi();
  return useQuery({
    queryKey: settingsKeys.approvalRules(project),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/approval-rules', { signal, params: { path: { id: project } } }),
      ),
  });
}

export function usePutApprovalRule(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (v: { name: string; body: PutApprovalRuleBody }) =>
      call(() =>
        api.PUT('/api/v1/projects/{id}/approval-rules/{name}', {
          params: { path: { id: project, name: v.name } },
          body: v.body,
        }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: settingsKeys.approvalRules(project) }),
    onError: refreshSession,
  });
}

export function useDeleteApprovalRule(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (name: string) =>
      callNoContent(() =>
        api.DELETE('/api/v1/projects/{id}/approval-rules/{name}', {
          params: { path: { id: project, name } },
        }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: settingsKeys.approvalRules(project) }),
    onError: refreshSession,
  });
}

export function useProtectedRefs(project: number) {
  const api = useApi();
  return useQuery({
    queryKey: settingsKeys.protectedRefs(project),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/protected-refs', { signal, params: { path: { id: project } } }),
      ),
  });
}

/** Set a protected branch's merge conditions (the work-item link condition
 * is not offered until work items exist, M3 I1). */
export function usePutMergeRule(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (v: { ruleId: number; body: PutMergeRuleBody }) =>
      call(() =>
        api.PUT('/api/v1/projects/{id}/protected-refs/{rule_id}/merge-rule', {
          params: { path: { id: project, rule_id: v.ruleId } },
          body: v.body,
        }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: settingsKeys.protectedRefs(project) }),
    onError: refreshSession,
  });
}

export function useDeleteMergeRule(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (ruleId: number) =>
      callNoContent(() =>
        api.DELETE('/api/v1/projects/{id}/protected-refs/{rule_id}/merge-rule', {
          params: { path: { id: project, rule_id: ruleId } },
        }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: settingsKeys.protectedRefs(project) }),
    onError: refreshSession,
  });
}

/** At most this many pages of 100 groups are offered in the group picker
 * (2 000 groups). The rules name their approvers themselves (`users`,
 * `groups`), so a rule's approvers never depend on this list. */
export const NAME_PAGES = 20;

/** The groups the picker of an approval rule offers, every page (up to
 * NAME_PAGES). */
export function useGroupChoices(enabled: boolean) {
  const api = useApi();
  return useQuery({
    queryKey: ['groups', 'names'],
    enabled,
    queryFn: async ({ signal }) => {
      const all = [];
      let cursor: string | undefined;
      for (let page = 0; page < NAME_PAGES; page += 1) {
        const query: { limit: number; cursor?: string } = { limit: 100 };
        if (cursor !== undefined) query.cursor = cursor;
        const got = await call(() => api.GET('/api/v1/groups', { signal, params: { query } }));
        all.push(...got.items);
        if (got.next_cursor === null) break;
        cursor = got.next_cursor;
      }
      return all;
    },
  });
}
