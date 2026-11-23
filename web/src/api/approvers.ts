// Approvers by organisation, named people and groups; delegation (ADR-0038
// §3, step C). The approval state of a merge request (`eligible`: who can
// approve for each rule and why no one can), each deploy environment's
// approvers, the person's own delegation, and the unit picker's search.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call, callNoContent } from './client';
import { useApi } from './context';
import type { DelegationState, PutApprovers, PutDelegation } from './types';

export const approverKeys = {
  /** Under the merge request's key: a reload of it reloads this too. */
  state: (project: number, iid: number) => ['mr', project, iid, 'approvals'] as const,
  deploy: (project: number) => ['deploy-approvers', project] as const,
  delegation: ['approval-delegation'] as const,
  units: (term: string) => ['org-units', term] as const,
};

/** One organisation selector (ADR-0038 §3.1) as the API carries it. */
export interface ApproverSelector {
  relation: 'requester_section' | 'requester_department' | 'unit';
  unit?: string | null;
  include_descendants?: boolean;
  min_rank?: number | null;
  positions?: string[] | null;
  head_only?: false;
  escalate?: 'none' | 'parent';
}

const RELATIONS = ['requester_section', 'requester_department', 'unit'] as const;

/** A selector from the server's JSON (`null` when it is not one: shown as
 * unknown, never guessed). */
export function asSelector(value: unknown): ApproverSelector | null {
  if (typeof value !== 'object' || value === null) return null;
  const v = value as Record<string, unknown>;
  const relation = v.relation;
  if (typeof relation !== 'string' || !(RELATIONS as readonly string[]).includes(relation)) return null;
  const s: ApproverSelector = { relation: relation as ApproverSelector['relation'] };
  if (typeof v.unit === 'string') s.unit = v.unit;
  if (v.include_descendants === true) s.include_descendants = true;
  if (typeof v.min_rank === 'number') s.min_rank = v.min_rank;
  if (Array.isArray(v.positions)) s.positions = v.positions.filter((p): p is string => typeof p === 'string');
  if (v.escalate === 'parent') s.escalate = 'parent';
  return s;
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

/** `GET …/merge-requests/{iid}/approvals` (only while `enabled`: a rule
 * that names approvers applies). */
export function useApprovalState(project: number, iid: number, enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: approverKeys.state(project, iid),
    enabled,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/merge-requests/{iid}/approvals', {
          signal,
          params: { path: { id: project, iid: String(iid) } },
        }),
      ),
  });
}

export function useDeployApprovers(project: number) {
  const api = useApi();
  return useQuery({
    queryKey: approverKeys.deploy(project),
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}/deploy/approvers', { signal, params: { path: { id: project } } })),
  });
}

export function usePutDeployApprovers(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (v: { tier: string; body: PutApprovers }) =>
      call(() =>
        api.PUT('/api/v1/projects/{id}/deploy/environments/{tier}/approvers', {
          params: { path: { id: project, tier: v.tier } },
          body: v.body,
        }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: approverKeys.deploy(project) }),
    onError: refreshSession,
  });
}

export function useDelegation() {
  const api = useApi();
  return useQuery({
    queryKey: approverKeys.delegation,
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/user/approval-delegation', { signal })),
  });
}

export function usePutDelegation() {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (body: PutDelegation) => call(() => api.PUT('/api/v1/user/approval-delegation', { body })),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: approverKeys.delegation }),
    onError: refreshSession,
  });
}

export function useDeleteDelegation() {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: () => callNoContent(() => api.DELETE('/api/v1/user/approval-delegation', {})),
    onSuccess: () => {
      queryClient.setQueryData<DelegationState | undefined>(approverKeys.delegation, (old) =>
        old === undefined ? old : { ...old, mine: null },
      );
      void queryClient.invalidateQueries({ queryKey: approverKeys.delegation });
    },
    onError: refreshSession,
  });
}

/** Org units by code prefix or name (the unit picker; `term` empty: none). */
export function useOrgUnitSearch(term: string) {
  const api = useApi();
  return useQuery({
    queryKey: approverKeys.units(term),
    enabled: term !== '',
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/org/units', { signal, params: { query: { search: term } } })),
  });
}

/** The most days a delegation may last (ADR-0038 §3.5). */
export const MAX_DELEGATION_DAYS = 30;

/** A `datetime-local` value (`YYYY-MM-DDTHH:MM`, the browser's zone) as an
 * RFC 3339 timestamp with its offset, or `null` when it is not one. */
export function localToRfc3339(local: string): string | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})$/.exec(local);
  if (m === null) return null;
  const d = new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]), Number(m[4]), Number(m[5]));
  if (Number.isNaN(d.getTime())) return null;
  const off = -d.getTimezoneOffset();
  const sign = off >= 0 ? '+' : '-';
  const abs = Math.abs(off);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${m[1]}-${m[2]}-${m[3]}T${m[4]}:${m[5]}:00${sign}${pad(Math.floor(abs / 60))}:${pad(abs % 60)}`;
}

/** Why a delegation period is refused before it is sent (`null`: fine). */
export function periodProblem(
  start: string,
  end: string,
  now: Date = new Date(),
): 'invalid' | 'order' | 'ended' | 'tooLong' | null {
  const s = localToRfc3339(start);
  const e = localToRfc3339(end);
  if (s === null || e === null) return 'invalid';
  const sm = Date.parse(s);
  const em = Date.parse(e);
  if (!(em > sm)) return 'order';
  if (em <= now.getTime()) return 'ended';
  if (em - sm > MAX_DELEGATION_DAYS * 24 * 60 * 60 * 1000) return 'tooLong';
  return null;
}
