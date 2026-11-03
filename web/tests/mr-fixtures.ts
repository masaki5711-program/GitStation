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
    target_branch: 'main',
    method: 'merge_commit',
    mergeable,
    // A newer server may add reason names under schema_version 1 (docs/api/merge-status.md);
    // tests feed unknown names on purpose, so the wire type is widened here only.
    reasons: reasons as MergeStatus['reasons'],
    approvals: null,
    checks: [],
    discussions: null,
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

export function mrRoutes(over: Record<string, Handler> = {}): Record<string, Handler> {
  return repoRoutes({
    [`GET ${MR_API}`]: () => json(200, { merge_requests: [mr(3)], next_before: null }),
    [`GET ${MR_API}/3`]: () => json(200, mr(3)),
    [`GET ${MR_API}/3/status`]: () => json(200, status([])),
    [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([])),
    [`GET ${MR_API}/3/versions`]: () => json(200, versions([version(1)])),
    [`GET ${MR_API}/3/versions/1/diff`]: () => json(200, versionDiff(1, DIFF)),
    // U2: the latest merge attempt (read while a merge runs).
    [`GET ${MR_API}/3/merge`]: () => json(200, { iid: 3, merge: null }),
    ...over,
  });
}
