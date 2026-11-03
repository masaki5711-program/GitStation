// Synthetic backend for the merge request part of the browser journey (U1):
// the list, create, one merge request, its status, versions, the latest
// version's diff and its threads. Stateful, so a created merge request, a
// line comment and a resolved thread show up on the next read, and the status
// follows the threads (the merge condition requires them resolved). Every
// unsafe request must carry the CSRF header (403 otherwise, as forge's
// FG-AUTH-0016). The shapes are those of tests/mr-fixtures.ts (keep the two in
// step); no real names, ids or paths.
const C1 = 'a'.repeat(40);
const C2 = 'b'.repeat(40);
const BRANCHES = ['main', 'dev'];

const PATCH = [
  '@@ -1,3 +1,4 @@',
  ' pub fn add(a: i32, b: i32) -> i32 {',
  '-    a - b',
  '+    // sum',
  '+    a + b',
  ' }',
].join('\n');

const DIFF = {
  truncated: false,
  files: [
    {
      status: 'modified',
      old_path: 'src/lib.rs',
      new_path: 'src/lib.rs',
      old_mode: '100644',
      new_mode: '100644',
      old_oid: '1'.repeat(40),
      new_oid: '2'.repeat(40),
      similarity: null,
      binary: false,
      patch: PATCH,
      omitted: null,
    },
  ],
};

/** The lines a position may name: [side, path, line]. */
const COMMENTABLE = new Set(['new src/lib.rs 1', 'new src/lib.rs 2', 'new src/lib.rs 3', 'new src/lib.rs 4', 'old src/lib.rs 1', 'old src/lib.rs 2', 'old src/lib.rs 3']);

export function mrBackend(csrf, user) {
  const author = { username: 'hanako', display_name: 'Hanako Sato' };
  const me = { username: user.username, display_name: user.display_name };
  const mrs = [
    {
      iid: 1,
      project_id: 7,
      title: 'Describe the press line',
      description: 'A **synthetic** merge request.',
      state: 'open',
      draft: false,
      source_branch: 'dev',
      target_branch: 'main',
      author_id: 9,
      author,
      head_sha: C2,
      created_at: '2026-10-28T01:00:00Z',
      updated_at: '2026-10-28T01:00:00Z',
      closed_at: null,
      closed_by: null,
    },
  ];
  /** iid -> discussions */
  const threads = new Map([[1, []]]);
  let nextIid = 2;
  let nextId = 100;

  const version = (n) => ({
    version: n,
    head_sha: C2,
    base_sha: C1,
    merge_base_sha: C1,
    pushed_by: null,
    refs_purged: false,
    created_at: '2026-10-28T01:00:00Z',
  });

  function status(mr) {
    const unresolved = (threads.get(mr.iid) ?? []).filter((d) => !d.resolved).length;
    const reasons = [];
    if (mr.state !== 'open') reasons.push({ reason: 'not_open', detail: null });
    if (unresolved > 0) reasons.push({ reason: 'unresolved_discussions', detail: `count=${unresolved}` });
    return {
      schema_version: 1,
      iid: mr.iid,
      head_sha: C2,
      target_branch: mr.target_branch,
      method: 'merge_commit',
      mergeable: reasons.length === 0,
      reasons,
      approvals: null,
      checks: [],
      discussions: { unresolved },
      codeowners: { enforced: true, suspended_until: null },
    };
  }

  const note = (body) => ({
    id: nextId++,
    author_id: user.id,
    author: me,
    body,
    created_at: '2026-11-03T01:00:00Z',
  });

  /** Answers `{ status, body }` or `{ status, error: [message, request id, code] }`;
   * `null` when the request is not a merge request one. */
  function answer(req, url, body) {
    const path = url.pathname;
    const m = path.match(/^\/api\/v1\/projects\/7\/merge-requests(?:\/(\d+)(\/.*)?)?$/);
    if (m === null) return null;
    const method = req.method;
    if (!['GET', 'HEAD', 'OPTIONS'].includes(method) && req.headers['x-gitstation-csrf'] !== csrf) {
      return { status: 403, error: ['The request was rejected.', 'req-csrf-mr', 'FG-AUTH-0016'] };
    }
    const ok = (b, s = 200) => ({ status: s, body: b });
    const refused = (message) => ({ status: 422, error: [message, 'req-mr-422', 'FG-REVIEW-0004'] });
    const input = body === '' ? null : JSON.parse(body);

    if (m[1] === undefined) {
      if (method === 'GET') {
        const state = url.searchParams.get('state') ?? 'open';
        const list = mrs
          .filter((x) => state === 'all' || x.state === state)
          .sort((a, b) => b.iid - a.iid);
        return ok({ merge_requests: list, next_before: null });
      }
      if (method === 'POST') {
        if (!BRANCHES.includes(input.source_branch) || !BRANCHES.includes(input.target_branch)) {
          return refused('The branch does not exist.');
        }
        if (input.source_branch === input.target_branch || input.title.trim() === '') {
          return refused('The merge request is not valid.');
        }
        const created = {
          ...mrs[0],
          iid: nextIid++,
          title: input.title,
          description: input.description ?? '',
          source_branch: input.source_branch,
          target_branch: input.target_branch,
          author_id: user.id,
          author: me,
          created_at: '2026-11-03T00:00:00Z',
          updated_at: '2026-11-03T00:00:00Z',
        };
        mrs.push(created);
        threads.set(created.iid, []);
        return ok(created, 201);
      }
      return null;
    }

    const mr = mrs.find((x) => x.iid === Number(m[1]));
    if (mr === undefined) return { status: 404, error: ['Not found.', 'req-mr-404', 'FG-REPO-0005'] };
    const list = threads.get(mr.iid) ?? [];
    const rest = m[2] ?? '';
    const key = `${method} ${rest}`;

    if (key === 'GET ') return ok(mr);
    if (key === 'GET /status') return ok(status(mr));
    if (key === 'GET /versions') return ok({ iid: mr.iid, versions: [version(1)] });
    if (key === 'GET /versions/1/diff') {
      return ok({ version: version(1), from: C1, to: C2, commits: [], commits_truncated: false, diff: DIFF });
    }
    if (key === 'GET /discussions') return ok({ iid: mr.iid, discussions: list, truncated: false });
    if (key === 'POST /discussions') {
      const position = input.position ?? null;
      if (
        position !== null &&
        (position.version !== 1 || !COMMENTABLE.has(`${position.side} ${position.path} ${position.line}`))
      ) {
        return refused('That line cannot be commented on.');
      }
      const created = {
        id: nextId++,
        kind: position === null ? 'general' : 'inline',
        author: me,
        started_by: user.id,
        created_at: '2026-11-03T01:00:00Z',
        notes: [note(input.body)],
        outdated: false,
        outdated_unknown: false,
        position,
        resolved: false,
        resolved_at: null,
        resolved_by: null,
      };
      list.push(created);
      return ok(created, 201);
    }
    const thread = rest.match(/^\/discussions\/(\d+)\/(notes|resolve|unresolve)$/);
    if (method === 'POST' && thread !== null) {
      const d = list.find((x) => x.id === Number(thread[1]));
      if (d === undefined) return { status: 404, error: ['Not found.', 'req-thread-404', 'FG-REPO-0005'] };
      if (thread[2] === 'notes') {
        const n = note(input.body);
        d.notes.push(n);
        return ok(n, 201);
      }
      d.resolved = thread[2] === 'resolve';
      d.resolved_at = d.resolved ? '2026-11-03T02:00:00Z' : null;
      d.resolved_by = d.resolved ? user.id : null;
      return ok(d);
    }
    return null;
  }

  return { answer };
}
