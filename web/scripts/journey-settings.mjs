// Synthetic backend for the settings part of the browser journey (MR11):
// project settings, protected refs, members, tokens, sessions, deleted
// projects and administration. Stateful, so a created token appears in the
// list; every unsafe request must carry the CSRF header. The shapes are those
// of tests/settings.test.tsx (keep the two in step).
export const SECRET = 'fgp_abcd_JOURNEYSYNTHETICSECRET';

export function settingsBackend(csrf) {
  const rules = [
    {
      id: 1,
      pattern: 'refs/heads/main',
      push_min_role: 'maintainer',
      created_at: '2026-10-01T00:00:00Z',
      updated_at: '2026-10-01T00:00:00Z',
    },
  ];
  let nextRule = 2;
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
  return { answer, tokens };
}
