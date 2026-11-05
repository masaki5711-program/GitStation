// Synthetic backend for the settings part of the browser journey (MR11):
// project settings, protected refs, members, tokens, sessions, deleted
// projects and administration; U2 adds the merge settings (with the
// expected_updated_at check: another value is 409, as FG-REVIEW-0017), the
// CODEOWNERS suspension, approval rules and the merge conditions of protected
// branches (required checks answer the required_checks_unreportable warning).
// The rule writes check expected_updated_at too (null: create only; another
// value is 409, as FG-REVIEW-0018 / FG-REVIEW-0019), and an approval rule
// names its approvers (users / groups) as the API does.
// Stateful, so a created token appears in the list; every unsafe request must
// carry the CSRF header. The shapes are those of tests/settings.test.tsx and
// tests/merge-settings-fixtures.ts (keep them in step).
export const SECRET = 'fgp_abcd_JOURNEYSYNTHETICSECRET';

export function settingsBackend(csrf) {
  const rules = [
    {
      id: 1,
      pattern: 'refs/heads/main',
      push_min_role: 'maintainer',
      created_at: '2026-10-01T00:00:00Z',
      updated_at: '2026-10-01T00:00:00Z',
      merge_rule: null,
      warnings: [],
    },
  ];
  let nextRule = 2;
  const merge = {
    project_id: 7,
    allowed_methods: ['merge_commit', 'squash'],
    default_method: 'merge_commit',
    delete_source_default: false,
    reset_approvals_on_push: true,
    committer_approvals_excluded: true,
    codeowners: { enforced: true, suspended_at: null, suspended_until: null },
    stored: false,
    updated_at: null,
    viewer: { can_manage: true, can_loosen: true, can_suspend_codeowners: true },
  };
  let stamps = 0;
  /** RFC 3339 to the microsecond, as forge answers (an opaque string to the client). */
  const stamp = () => {
    stamps += 1;
    return `2026-11-03T01:00:${String(stamps).padStart(2, '0')}.123456Z`;
  };
  const approvalRules = [];
  let nextApprovalRule = 1;
  const groupIds = { acme: 3 };
  const members = [
    { user_id: 5, username: 'taro', display_name: 'Taro Yamada', role: 'owner', active: true, created_at: '2026-10-01T00:00:00Z' },
    { user_id: 6, username: 'jiro', display_name: 'Jiro Suzuki', role: 'developer', active: true, created_at: '2026-10-02T00:00:00Z' },
  ];
  const found = [
    { id: 6, username: 'jiro', display_name: 'Jiro Suzuki' },
    { id: 9, username: 'hanako', display_name: 'Hanako Sato' },
  ];
  const tokens = [];
  const sessions = [
    {
      public_id: '0123456789abcdef',
      created_at: '2026-11-01T00:00:00Z',
      last_seen_at: '2026-11-01T01:00:00Z',
      idle_expires_at: '2026-11-01T05:00:00Z',
      absolute_expires_at: '2026-11-02T00:00:00Z',
      oidc: true,
      current: true,
    },
    {
      public_id: 'fedcba9876543210',
      created_at: '2026-10-31T00:00:00Z',
      last_seen_at: '2026-10-31T01:00:00Z',
      idle_expires_at: '2026-11-01T05:00:00Z',
      absolute_expires_at: '2026-11-02T00:00:00Z',
      oidc: true,
      current: false,
    },
  ];
  const users = [
    { id: 5, username: 'taro', display_name: 'Taro Yamada', kind: 'human', state: 'active', is_admin: true, created_at: '2026-09-01T00:00:00Z' },
    { id: 9, username: 'hanako', display_name: 'Hanako Sato', kind: 'human', state: 'active', is_admin: false, created_at: '2026-09-02T00:00:00Z' },
  ];
  const deleted = [];

  /** Answers `{ status, body }`, `{ status: 204 }` or `{ error: [message, request id, code] }`;
   * `null` when the request is not one of these. `project` is the shared project record. */
  function answer(req, url, body, project) {
    const path = url.pathname;
    const method = req.method;
    const unsafe = !['GET', 'HEAD', 'OPTIONS'].includes(method);
    const mine =
      /^\/api\/v1\/(projects\/7(\/|$)|user\/|admin\/|users|groups\/3\/members)/.test(path) ||
      path === '/api/v1/projects';
    if (!mine) return null;
    if (unsafe && req.headers['x-gitstation-csrf'] !== csrf) {
      return { error: ['The request was rejected.', 'req-csrf-settings', 'FG-AUTH-0016'], status: 403 };
    }
    const json = (b, status = 200) => ({ status, body: b });
    const input = body === '' ? null : JSON.parse(body);
    const key = `${method} ${path}`;

    if (key === 'GET /api/v1/projects' && url.searchParams.get('state') === 'pending_deletion') {
      return json({ items: deleted, has_more: false, next_cursor: null });
    }
    if (key === 'PATCH /api/v1/projects/7') {
      Object.assign(project, input);
      return json(project);
    }
    if (key === 'PUT /api/v1/projects/7/visibility') {
      project.visibility = input.visibility;
      return json(project);
    }
    if (key === 'DELETE /api/v1/projects/7') {
      deleted.push({ ...project });
      return json({ id: 7, deleted_at: '2026-11-01T00:00:00Z', purge_after: '2026-11-08T00:00:00Z' });
    }
    if (key === 'POST /api/v1/projects/7/restore') {
      deleted.length = 0;
      return json(project);
    }
    if (key === 'POST /api/v1/projects/7/transfer') {
      return {
        status: 429,
        error: ['This was renamed or moved too often recently. Try again later.', 'req-move-429', 'FG-REPO-0018'],
      };
    }
    if (key === 'GET /api/v1/projects/7/protected-refs') return json({ rules });
    if (key === 'POST /api/v1/projects/7/protected-refs') {
      const rule = {
        id: nextRule++,
        pattern: input.pattern,
        push_min_role: input.push_min_role,
        created_at: '2026-11-01T00:00:00Z',
        updated_at: '2026-11-01T00:00:00Z',
        merge_rule: null,
        warnings: [],
      };
      rules.push(rule);
      return json(rule, 201);
    }
    const rule = path.match(/^\/api\/v1\/projects\/7\/protected-refs\/(\d+)$/);
    if (rule && method === 'DELETE') {
      const at = rules.findIndex((r) => r.id === Number(rule[1]));
      if (at >= 0) rules.splice(at, 1);
      return { status: 204 };
    }

    if (key === 'GET /api/v1/projects/7/merge-settings') return json(merge);
    if (key === 'PUT /api/v1/projects/7/merge-settings') {
      if (!('expected_updated_at' in input)) {
        return { status: 400, error: ['A field is missing.', 'req-ms-400', 'FG-WEB-0006'] };
      }
      if (input.expected_updated_at !== merge.updated_at) {
        return {
          status: 409,
          error: ['The merge settings were changed by someone else. Read them again.', 'req-ms-409', 'FG-REVIEW-0017'],
        };
      }
      const { expected_updated_at, ...values } = input;
      void expected_updated_at;
      Object.assign(merge, values, { stored: true, updated_at: stamp() });
      return json(merge);
    }
    if (key === 'PUT /api/v1/projects/7/merge-settings/codeowners-enforcement') {
      if (input.enforced) merge.codeowners = { enforced: true, suspended_at: null, suspended_until: null };
      else {
        if (!(input.for_minutes >= 1 && input.for_minutes <= 1440)) {
          return { status: 403, error: ['The suspension must be 1 to 1440 minutes.', 'req-co-403', 'FG-AUTH-0003'] };
        }
        merge.codeowners = { enforced: false, suspended_at: '2026-11-03T01:00:00Z', suspended_until: '2026-11-03T01:30:00Z' };
      }
      merge.stored = true;
      merge.updated_at = stamp();
      return json(merge);
    }
    if (key === 'GET /api/v1/projects/7/approval-rules') return json({ rules: approvalRules });
    const approvalRule = path.match(/^\/api\/v1\/projects\/7\/approval-rules\/([a-z0-9_.-]+)$/);
    if (approvalRule && method === 'PUT') {
      if (!('expected_updated_at' in input)) {
        return { status: 400, error: ['expected_updated_at is required.', 'req-rule-400', 'FG-WEB-0006'] };
      }
      const existing = approvalRules.find((r) => r.name === approvalRule[1]);
      if (input.expected_updated_at !== (existing?.updated_at ?? null)) {
        return { status: 409, error: ['This approval rule changed after you opened it.', 'req-rule-409', 'FG-REVIEW-0018'] };
      }
      const users = (input.users ?? []).map((u) => found.find((f) => f.username === u));
      const group_ids = (input.groups ?? []).map((g) => groupIds[g] ?? 0);
      if (users.includes(undefined) || group_ids.includes(0)) {
        return { status: 422, error: ['An approver does not exist.', 'req-rule-422', 'FG-WEB-0006'] };
      }
      const next = {
        approval_rule_id: existing?.approval_rule_id ?? nextApprovalRule++,
        name: approvalRule[1],
        target_pattern: input.target_pattern ?? null,
        required_approvals: input.required_approvals,
        user_ids: users.map((u) => u.id),
        group_ids,
        users,
        groups: (input.groups ?? []).map((g) => ({ id: groupIds[g], full_path: g, display_name: 'Acme Plant' })),
        updated_at: stamp(),
      };
      if (existing) Object.assign(existing, next);
      else approvalRules.push(next);
      return json(next);
    }
    if (approvalRule && method === 'DELETE') {
      const at = approvalRules.findIndex((r) => r.name === approvalRule[1]);
      if (at >= 0) approvalRules.splice(at, 1);
      return { status: 204 };
    }
    const mergeRule = path.match(/^\/api\/v1\/projects\/7\/protected-refs\/(\d+)\/merge-rule$/);
    if (mergeRule) {
      const r = rules.find((x) => x.id === Number(mergeRule[1]));
      if (r === undefined) return { status: 404, error: ['Not found.', 'req-mrule-404', 'FG-REPO-0013'] };
      if (method === 'DELETE') {
        r.merge_rule = null;
        r.warnings = [];
        return { status: 204 };
      }
      if (method === 'PUT') {
        if (!('expected_updated_at' in input)) {
          return { status: 400, error: ['expected_updated_at is required.', 'req-mrule-400', 'FG-WEB-0006'] };
        }
        if (input.expected_updated_at !== (r.merge_rule?.updated_at ?? null)) {
          return { status: 409, error: ['The merge conditions changed after you opened them.', 'req-mrule-409', 'FG-REVIEW-0019'] };
        }
        r.merge_rule = {
          required_approvals: input.required_approvals ?? 0,
          required_checks: input.required_checks ?? [],
          require_resolved_discussions: input.require_resolved_discussions === true,
          direct_push_forbidden: input.direct_push_forbidden === true,
          // M3 I1d: the work item link condition (this project has no binding).
          require_work_item_link: input.require_work_item_link === true,
          updated_at: stamp(),
        };
        r.warnings = [
          ...(r.merge_rule.required_checks.length > 0 ? ['required_checks_unreportable'] : []),
          ...(r.merge_rule.require_work_item_link ? ['work_item_link_unbound'] : []),
        ];
        return json(r);
      }
    }

    if (key === 'GET /api/v1/projects/7/members' || key === 'GET /api/v1/groups/3/members') {
      return json({ members, next_after: null });
    }
    if (key === 'GET /api/v1/users') {
      const q = (url.searchParams.get('search') ?? '').toLowerCase();
      return json({ items: found.filter((u) => u.username.includes(q) || u.display_name.toLowerCase().includes(q)) });
    }
    if (key === 'POST /api/v1/projects/7/members') {
      const who = found.find((u) => u.id === input.user_id);
      const member = { user_id: who.id, username: who.username, display_name: who.display_name, role: input.role, active: true, created_at: '2026-11-01T00:00:00Z' };
      members.push(member);
      return json(member, 201);
    }
    const member = path.match(/^\/api\/v1\/projects\/7\/members\/(\d+)$/);
    if (member && method === 'DELETE') {
      const at = members.findIndex((m) => m.user_id === Number(member[1]));
      if (at >= 0) members.splice(at, 1);
      return { status: 204 };
    }
    if (member && method === 'PATCH') {
      const m = members.find((x) => x.user_id === Number(member[1]));
      m.role = input.role;
      return json(m);
    }

    if (key === 'GET /api/v1/user/tokens') return json({ items: tokens });
    if (key === 'POST /api/v1/user/tokens') {
      const token = {
        id: tokens.length + 1,
        public_id: 'abcd',
        name: input.name,
        scopes: input.scopes,
        created_at: '2026-11-01T00:00:00Z',
        expires_at: '2027-01-30T00:00:00Z',
        revoked_at: null,
        last_used_at: null,
      };
      tokens.push(token);
      return json({ ...token, is_agent: input.is_agent === true, token: SECRET }, 201);
    }
    if (key === 'GET /api/v1/user/sessions') return json({ items: sessions, truncated: false });
    const session = path.match(/^\/api\/v1\/user\/sessions\/([0-9a-f]{16})$/);
    if (session && method === 'DELETE') {
      const at = sessions.findIndex((s) => s.public_id === session[1] && !s.current);
      if (at >= 0) sessions.splice(at, 1);
      return { status: 204 };
    }

    if (key === 'GET /api/v1/admin/users') {
      const state = url.searchParams.get('state');
      return json({ items: users.filter((u) => state === null || u.state === state), next_cursor: null, has_more: false });
    }
    const disable = path.match(/^\/api\/v1\/admin\/users\/(\d+)\/disable$/);
    if (disable && method === 'POST') {
      if (Number(disable[1]) === 5) {
        return {
          status: 409,
          error: [
            'This was refused because it would leave no administrator or group Owner. Contact the administrator with the request ID.',
            'req-disable-self',
            'FG-AUTH-0009',
          ],
        };
      }
      users.find((u) => u.id === Number(disable[1])).state = 'disabled';
      return json({ user_id: Number(disable[1]), outcome: 'disabled' });
    }
    return null;
  }
  /** Someone else saves the merge settings behind the page (the journey's 409). */
  const changeMergeSettingsElsewhere = () => {
    merge.stored = true;
    merge.updated_at = stamp();
  };
  return { answer, tokens, changeMergeSettingsElsewhere };
}
