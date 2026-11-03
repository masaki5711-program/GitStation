// Synthetic backend for the approve-and-merge part of the browser journey
// (U2): one draft merge request (!5) with three reasons -> approve -> one
// reason -> mark as ready -> merge -> queued -> executing -> applied.
// Stateful, and strict where the real server is:
// - approve and merge must name the current head (`sha`); the first approve
//   finds a version pushed meanwhile, so it is refused 409 (stale view, as
//   FG-REVIEW-0008) and the page must read the merge request again;
// - the first merge request is refused 403 (the role changed meanwhile);
// - every unsafe request without the CSRF header is refused 403 (FG-AUTH-0016).
// The status shapes follow tests/mr-fixtures.ts (keep the two in step); no
// real names, ids or paths.
const C1 = 'a'.repeat(40);
const OLD_HEAD = 'd'.repeat(40);
const NEW_HEAD = 'e'.repeat(40);

export function mergeBackend(csrf, user) {
  const mr = {
    iid: 5,
    project_id: 7,
    title: 'Add the pump spec',
    description: 'A synthetic change to merge.',
    state: 'open',
    draft: true,
    source_branch: 'dev',
    target_branch: 'main',
    author_id: 9,
    author: { username: 'hanako', display_name: 'Hanako Sato' },
    head_sha: OLD_HEAD,
    created_at: '2026-11-03T00:00:00Z',
    updated_at: '2026-11-03T00:00:00Z',
    closed_at: null,
    closed_by: null,
  };
  let pushedMeanwhile = true;
  let mergeRefusedOnce = false;
  let approvedBy = null;
  let attempt = null;
  let attemptReads = 0;

  function status() {
    const reasons = [];
    if (mr.state !== 'open') reasons.push({ reason: 'not_open', detail: null });
    else {
      if (mr.draft) reasons.push({ reason: 'draft', detail: null });
      if (attempt !== null && !['applied', 'superseded', 'failed'].includes(attempt.state)) {
        reasons.push({ reason: 'merge_in_progress', detail: null });
      }
      if (approvedBy === null) {
        reasons.push({ reason: 'approvals_missing', detail: 'rule=@protected_branch required=1 counted=0' });
        reasons.push({ reason: 'codeowner_approval_missing', detail: 'pattern=/docs/ owners=@taro' });
      }
    }
    const counted = approvedBy === null ? 0 : 1;
    return {
      schema_version: 1,
      iid: mr.iid,
      head_sha: mr.head_sha,
      target_branch: mr.target_branch,
      method: 'merge_commit',
      mergeable: reasons.length === 0,
      reasons,
      approvals: {
        rules: [{ name: '@protected_branch', applies: true, required: 1, counted }],
        approvals:
          approvedBy === null ? [] : [{ approver_id: user.id, username: user.username, counted: true, uncounted_reason: null }],
        codeowners: [{ pattern: '/docs/', owners: ['@taro'], satisfied: approvedBy !== null }],
      },
      checks: [],
      discussions: null,
      codeowners: { enforced: true, suspended_until: null },
      viewer: {
        can_approve: mr.state === 'open',
        can_unapprove: mr.state === 'open',
        can_merge: true,
        can_mark_ready: true,
        approved_by_me: approvedBy !== null,
      },
    };
  }

  const nextState = () => {
    // queued (the 202) -> executing -> applied, one step per read.
    attemptReads += 1;
    if (attempt.state === 'queued') attempt.state = 'executing';
    else if (attempt.state === 'executing') {
      attempt.state = 'applied';
      attempt.new_sha = 'f'.repeat(40);
      attempt.expected_old = C1;
      attempt.source_outcome = attempt.delete_source_branch ? 'deleted' : null;
      attempt.finished_at = '2026-11-03T01:00:05Z';
      mr.state = 'merged';
    }
    attempt.updated_at = `2026-11-03T01:00:0${String(attemptReads)}Z`;
  };

  /** `{ status, body }` or `{ status, error: [message, request id, code] }`;
   * `null` when the request is not about !5. */
  function answer(req, url, body) {
    const m = url.pathname.match(/^\/api\/v1\/projects\/7\/merge-requests\/5(\/.*)?$/);
    if (m === null) return null;
    const method = req.method;
    if (!['GET', 'HEAD', 'OPTIONS'].includes(method) && req.headers['x-gitstation-csrf'] !== csrf) {
      return { status: 403, error: ['The request was rejected.', 'req-csrf-merge', 'FG-AUTH-0016'] };
    }
    const ok = (b, s = 200) => ({ status: s, body: b });
    const input = body === '' ? null : JSON.parse(body);
    const key = `${method} ${m[1] ?? ''}`;

    if (key === 'GET ') return ok(mr);
    if (key === 'GET /status') return ok(status());
    if (key === 'GET /discussions') return ok({ iid: 5, discussions: [], truncated: false });
    if (key === 'GET /versions') {
      return ok({
        iid: 5,
        versions: [{ version: 1, head_sha: mr.head_sha, base_sha: C1, merge_base_sha: C1, pushed_by: null, refs_purged: false, created_at: '2026-11-03T00:00:00Z' }],
      });
    }
    if (key === 'GET /versions/1/diff') {
      const v = { version: 1, head_sha: mr.head_sha, base_sha: C1, merge_base_sha: C1, pushed_by: null, refs_purged: false, created_at: '2026-11-03T00:00:00Z' };
      return ok({ version: v, from: C1, to: mr.head_sha, commits: [], commits_truncated: false, diff: { truncated: false, files: [] } });
    }
    if (key === 'PATCH ') {
      if (input.draft === false) mr.draft = false;
      return ok(mr);
    }
    if (key === 'POST /approve') {
      if (pushedMeanwhile) {
        // A new version arrived after the page read the status.
        pushedMeanwhile = false;
        mr.head_sha = NEW_HEAD;
      }
      if (input?.sha !== mr.head_sha) {
        return { status: 409, error: ['A new version was pushed. Look at it before approving.', 'req-stale-view', 'FG-REVIEW-0008'] };
      }
      approvedBy = user.id;
      return ok({ iid: 5, outcome: 'approved', counted: true, uncounted_reason: null, approval: null });
    }
    if (key === 'POST /unapprove') {
      approvedBy = null;
      return ok({ iid: 5, outcome: 'withdrawn', counted: false, uncounted_reason: null, approval: null });
    }
    if (key === 'POST /merge') {
      if (!mergeRefusedOnce) {
        mergeRefusedOnce = true;
        return { status: 403, error: ['You may not merge into main right now.', 'req-merge-403', 'FG-AUTH-0003'] };
      }
      if (input?.sha !== mr.head_sha) {
        return { status: 409, error: ['A new version was pushed. Look at it before merging.', 'req-merge-stale', 'FG-REVIEW-0008'] };
      }
      if (!status().mergeable) {
        return { status: 409, error: ['It cannot be merged now.', 'req-merge-blocked', 'FG-REVIEW-0002'] };
      }
      attempt = {
        merge_id: 1,
        iid: 5,
        state: 'queued',
        method: input.method ?? 'merge_commit',
        head_sha: mr.head_sha,
        target_branch: 'main',
        delete_source_branch: input.delete_source_branch === true,
        expected_old: null,
        new_sha: null,
        reason: null,
        error_code: null,
        source_outcome: null,
        request_id: 'req-merge-ok',
        created_at: '2026-11-03T01:00:00Z',
        updated_at: '2026-11-03T01:00:00Z',
        finished_at: null,
      };
      return ok(attempt, 202);
    }
    if (key === 'GET /merge') {
      if (attempt !== null) nextState();
      return ok({ iid: 5, merge: attempt });
    }
    return null;
  }

  return { answer };
}
