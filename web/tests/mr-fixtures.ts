// Synthetic merge request data for the MR screens (no real names or ids).
// `mrRoutes()` is the route table the MR page tests share; it builds on the
// repository routes (session, resolve, project, refs).
import type { MergeStatus } from '../src/api/types';
import { DIFF, repoRoutes } from './repo-fixtures';
import { type Handler } from './mock';
import { json } from './render';

export const MR_API = '/api/v1/projects/7/merge-requests';
export const MR_SCREENS = '/acme/forge-web/-';

const AUTHOR = { username: 'hanako', display_name: '佐藤 花子' };

export function mr(iid: number, over: Record<string, unknown> = {}) {
  return {
    iid,
    project_id: 7,
    title: `Synthetic change ${iid}`,
    description: '',
    state: 'open',
    draft: false,
    source_branch: 'dev',
    target_branch: 'main',
    author_id: 6,
    author: AUTHOR,
    head_sha: 'b'.repeat(40),
    merge_result_sha: null,
    created_at: '2026-10-20T01:00:00Z',
    updated_at: '2026-10-20T01:00:00Z',
    closed_at: null,
    closed_by: null,
    ...over,
  };
}

export function status(
  reasons: { reason: string; detail: string | null }[],
  mergeable = reasons.length === 0,
): MergeStatus {
  return {
    schema_version: 1,
    iid: 3,
    head_sha: 'b'.repeat(40),
    merge_result_sha: null,
    target_branch: 'main',
    method: 'merge_commit',
    mergeable,
    // A newer server may add reason names under schema_version 1 (docs/api/merge-status.md);
    // tests feed unknown names on purpose, so the wire type is widened here only.
    reasons: reasons as MergeStatus['reasons'],
    approvals: null,
    checks: [],
    discussions: null,
    work_item_link: null,
    codeowners: { enforced: true, suspended_until: null },
    viewer: {
      can_approve: false,
      can_unapprove: false,
      can_merge: false,
      can_mark_ready: false,
      approved_by_me: false,
    },
  } satisfies MergeStatus;
}

export function note(id: number, body: string, over: Record<string, unknown> = {}) {
  return {
    id,
    author_id: 6,
    author: AUTHOR,
    body,
    created_at: '2026-10-21T02:00:00Z',
    ...over,
  };
}

export function discussion(id: number, over: Record<string, unknown> = {}) {
  return {
    id,
    kind: 'general',
    author: AUTHOR,
    started_by: 6,
    created_at: '2026-10-21T02:00:00Z',
    notes: [note(id * 10, `Synthetic comment ${id}`)],
    outdated: false,
    outdated_unknown: false,
    position: null,
    resolved: false,
    resolved_at: null,
    resolved_by: null,
    ...over,
  };
}

export const discussions = (list: unknown[], truncated = false) => ({
  iid: 3,
  discussions: list,
  truncated,
});

export function version(n: number, over: Record<string, unknown> = {}) {
  return {
    version: n,
    head_sha: String(n).repeat(40).slice(0, 40),
    base_sha: 'a'.repeat(40),
    merge_base_sha: 'a'.repeat(40),
    pushed_by: null,
    refs_purged: false,
    created_at: '2026-10-20T01:00:00Z',
    ...over,
  };
}

export const versions = (list: unknown[]) => ({ iid: 3, versions: list });

export function versionDiff(n: number, diff: unknown) {
  return {
    version: version(n),
    from: 'a'.repeat(40),
    to: String(n).repeat(40).slice(0, 40),
    commits: [],
    commits_truncated: false,
    diff,
  };
}

/** U2: a project's merge settings as `GET …/merge-settings` answers them. */
export function mergeSettings(over: Record<string, unknown> = {}) {
  return {
    project_id: 7,
    allowed_methods: ['merge_commit', 'squash'],
    default_method: 'merge_commit',
    delete_source_default: false,
    reset_approvals_on_push: true,
    committer_approvals_excluded: true,
    codeowners: { enforced: true, suspended_at: null, suspended_until: null },
    stored: true,
    updated_at: '2026-11-03T01:02:03.123456Z',
    viewer: { can_manage: false, can_loosen: false, can_suspend_codeowners: false },
    ...over,
  };
}

export function attempt(state: string, over: Record<string, unknown> = {}) {
  return {
    merge_id: 41,
    iid: 3,
    state,
    method: 'squash',
    head_sha: 'b'.repeat(40),
    merge_result_sha: null,
    target_branch: 'main',
    delete_source_branch: true,
    expected_old: null,
    new_sha: null,
    reason: null,
    error_code: null,
    source_outcome: null,
    request_id: 'req-merge-1',
    created_at: '2026-11-03T01:00:00Z',
    updated_at: '2026-11-03T01:00:00Z',
    finished_at: null,
    ...over,
  };
}

/** The check runs of the default head (`b` × 40). */
export const CHECKS_API = `/api/v1/projects/7/commits/${'b'.repeat(40)}/checks`;

export function checkRun(id: number, name: string, over: Record<string, unknown> = {}) {
  return {
    id,
    name,
    head_sha: 'b'.repeat(40),
    merge_result_sha: null,
    status: 'completed',
    conclusion: 'success',
    summary: '',
    details_url: null,
    annotations: [],
    reported_by: { id: 9, username: 'sa-ci' },
    revision: 1,
    created_at: '2026-11-03T01:00:00.000000Z',
    updated_at: '2026-11-03T01:00:00.000000Z',
    completed_at: '2026-11-03T01:00:00.000000Z',
    ...over,
  };
}

export const checkRuns = (runs: unknown[]) => ({ schema_version: 1, head_sha: 'b'.repeat(40), check_runs: runs });

export function conflicts(predictions: unknown[], scan: Record<string, unknown> = {}) {
  return {
    iid: 3,
    head_sha: 'b'.repeat(40),
    merge_result_sha: null,
    predictions,
    scan: {
      status: predictions.length === 0 ? 'none' : 'scanned',
      reason: null,
      stale: false,
      scanned_head_sha: null,
      scanned_target_sha: null,
      scanned_at: null,
      peers: 0,
      pairs_tried: 0,
      pairs_skipped: 0,
      ...scan,
    },
  };
}

export function mrRoutes(over: Record<string, Handler> = {}): Record<string, Handler> {
  return repoRoutes({
    [`GET ${MR_API}`]: () => json(200, { merge_requests: [mr(3)], next_before: null }),
    [`GET ${MR_API}/3`]: () => json(200, mr(3)),
    [`GET ${MR_API}/3/status`]: () => json(200, status([])),
    [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([])),
    // The version on screen is the head the status is about (U2 review HIGH 1).
    [`GET ${MR_API}/3/versions`]: () => json(200, versions([version(1, { head_sha: 'b'.repeat(40) })])),
    [`GET ${MR_API}/3/versions/1/diff`]: () => json(200, versionDiff(1, DIFF)),
    // U2: the latest merge attempt (read while a merge runs).
    [`GET ${MR_API}/3/merge`]: () => json(200, { iid: 3, merge: null }),
    // The detail under the status: the head's check runs and the predicted conflicts.
    [`GET ${CHECKS_API}`]: () => json(200, checkRuns([])),
    [`GET ${MR_API}/3/conflicts`]: () => json(200, conflicts([])),
    ...over,
  });
}
