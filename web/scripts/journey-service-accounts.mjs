// Synthetic backend for the service-account part of the browser journey: the
// list, create (a taken username is refused 409), one account, its tokens
// (the secret is in the issue answer once; a name that starts with "refuse"
// is refused 403 as a stale session would be), revoke, the check grants and
// disable (POST /admin/users/{id}/disable). Stateful, and strict where the
// real server is: every unsafe request needs the CSRF header. The shapes follow
// docs/api/openapi.json (ServiceAccountBody, ServiceAccountTokenBody,
// IssuedServiceAccountToken, CheckGrants; keep them in step).
export const SA_SECRET = 'fgp_journeysa_SYNTHETICSERVICESECRET0123456789';

export function serviceAccountsBackend(csrf, owner) {
  const stamp = '2026-11-03T01:00:00Z';
  const expiry = '2027-02-01T00:00:00Z';
  const accounts = [
    {
      id: 3,
      username: 'sa-ci-runner',
      display_name: 'CI runner',
      purpose: 'Reports CI results',
      state: 'active',
      created_at: stamp,
      created_by: owner.id,
      owner: { id: owner.id, username: owner.username, active: true },
    },
  ];
  const tokens = new Map([[3, []]]);
  let nextAccount = 4;
  let nextToken = 20;

  const body = (a) => {
    const live = (tokens.get(a.id) ?? []).filter((k) => k.revoked_at === null);
    return {
      ...a,
      active_tokens: live.length,
      last_token_expiry: live.length > 0 ? expiry : null,
    };
  };

  /** `{ status, body }`, `{ status: 204 }` or `{ error: [message, request id, code], status }`;
   * `null` when the request is not one of these. */
  function answer(req, url, text) {
    const path = url.pathname;
    const method = req.method;
    const disable = path.match(/^\/api\/v1\/admin\/users\/(\d+)\/disable$/);
    const ours =
      path.startsWith('/api/v1/admin/service-accounts') ||
      (disable !== null && accounts.some((a) => a.id === Number(disable[1])));
    if (!ours) return null;
    if (!['GET', 'HEAD', 'OPTIONS'].includes(method) && req.headers['x-gitstation-csrf'] !== csrf) {
      return { status: 403, error: ['The request was rejected.', 'req-csrf-sa', 'FG-AUTH-0016'] };
    }
    const input = text === '' ? null : JSON.parse(text);
    const json = (b, status = 200) => ({ status, body: b });
    const gone = () => ({
      status: 404,
      error: ['That does not exist or you cannot see it.', 'req-sa-404', 'FG-REPO-0005'],
    });

    if (disable !== null && method === 'POST') {
      const a = accounts.find((x) => x.id === Number(disable[1]));
      a.state = 'disabled';
      for (const k of tokens.get(a.id) ?? []) k.revoked_at ??= stamp;
      return json({ user_id: a.id, outcome: 'disabled' });
    }
    if (path === '/api/v1/admin/service-accounts') {
      if (method === 'GET') return json({ items: accounts.map(body), truncated: false });
      if (method === 'POST') {
        if (accounts.some((a) => a.username === input.username)) {
          return { status: 409, error: ['That username is already in use.', 'req-sa-dup', 'FG-REPO-0003'] };
        }
        const made = {
          id: nextAccount++,
          username: input.username,
          display_name: input.display_name ?? input.username,
          purpose: input.purpose,
          state: 'active',
          created_at: stamp,
          created_by: owner.id,
          owner: { id: input.owner_id, username: owner.username, active: true },
        };
        accounts.push(made);
        tokens.set(made.id, []);
        return json(body(made), 201);
      }
      return null;
    }
    const one = path.match(
      /^\/api\/v1\/admin\/service-accounts\/(\d+)(\/tokens|\/check-grants|\/tokens\/(\d+))?$/,
    );
    if (one === null) return null;
    const a = accounts.find((x) => x.id === Number(one[1]));
    if (a === undefined) return gone();
    if (one[2] === undefined && method === 'GET') return json(body(a));
    if (one[2] === '/check-grants' && method === 'GET') {
      return json({ grants: [{ project_id: 7, namespace: 'ci' }] });
    }
    if (one[2] === '/tokens') {
      if (method === 'GET') return json({ items: tokens.get(a.id) });
      if (method === 'POST') {
        if (input.name.startsWith('refuse')) {
          return {
            status: 403,
            error: ['A browser session is needed for this. Sign in again.', 'req-sa-session', 'FG-AUTH-0002'],
          };
        }
        if (a.state !== 'active') {
          return { status: 409, error: ['This account is disabled.', 'req-sa-off', 'FG-AUTH-0006'] };
        }
        const k = {
          id: nextToken++,
          public_id: `jsa${nextToken}`,
          name: input.name,
          scopes: [...input.scopes],
          created_at: stamp,
          expires_at: expiry,
          last_used_at: null,
          revoked_at: null,
          issued_by_user_id: owner.id,
        };
        tokens.get(a.id).push(k);
        return json(
          {
            id: k.id,
            public_id: k.public_id,
            name: k.name,
            scopes: k.scopes,
            expires_at: expiry,
            is_agent: input.is_agent ?? false,
            issued_by_user_id: owner.id,
            token: SA_SECRET,
          },
          201,
        );
      }
      return null;
    }
    if (one[3] !== undefined && method === 'DELETE') {
      const k = (tokens.get(a.id) ?? []).find((x) => x.id === Number(one[3]));
      if (k === undefined) return gone();
      k.revoked_at ??= stamp;
      return { status: 204 };
    }
    return null;
  }

  return { answer, accounts };
}
