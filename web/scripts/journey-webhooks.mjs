// Synthetic backend for the webhook and notification part of the browser
// journey (U3a): create (the secret is in the answer once), list (origin
// only), disable / enable, rotate, redeliver (the first answer for a delivery
// still in progress is 429, as FG-WEB-0010), the history paged by `before`,
// delete, and the person's notification preferences. Stateful, and strict
// where the real server is: every unsafe request needs the CSRF header; the
// full URL is never answered, only its origin. The shapes follow
// tests/webhook-fixtures.ts and docs/api/webhooks.md (keep them in step).
export const HOOK_SECRET = 'whsec_JOURNEYSYNTHETICSECRET0123456789';
export const ROTATED_SECRET = 'whsec_JOURNEYROTATEDSYNTHETICSECRET0123';

export function webhooksBackend(csrf) {
  const hooks = [];
  let nextHook = 1;
  const stamp = '2026-11-03T01:00:00Z';
  const deliveries = Array.from({ length: 25 }, (_, i) => {
    const id = 25 - i;
    return {
      id,
      delivery_id: `0b9c1f0e-0000-4000-8000-${String(id).padStart(12, '0')}`,
      event_seq: id * 10,
      event_id: `evt_${String(id).padStart(32, '0')}`,
      event_type: id % 2 === 0 ? 'merge_request.merged' : 'merge_request.opened',
      state: id === 25 ? 'failed' : id === 24 ? 'pending' : 'succeeded',
      attempts: id === 25 ? 10 : 1,
      redeliveries: 0,
      created_at: stamp,
      last_attempt_at: stamp,
      finished_at: id === 24 ? null : stamp,
      last_status: id === 25 ? 503 : id === 24 ? null : 200,
      last_reason: null,
      last_error_code: null,
      last_duration_ms: 120,
    };
  });
  let refusedOnce = false;
  let prefs = {
    email_enabled: true,
    language: null,
    kinds: ['review.requested', 'merge_request.merged', 'mention'],
    muted_kinds: ['mention'],
  };

  const body = (h) => {
    const { url, ...rest } = h;
    void url;
    return rest;
  };

  /** `{ status, body }`, `{ status: 204 }` or `{ error: [message, request id, code], status }`;
   * `null` when the request is not one of these. */
  function answer(req, url, text) {
    const path = url.pathname;
    const method = req.method;
    const mine = /^\/api\/v1\/projects\/7\/webhooks(\/|$)/.test(path) || path === '/api/v1/user/notification-preferences';
    if (!mine) return null;
    if (!['GET', 'HEAD', 'OPTIONS'].includes(method) && req.headers['x-gitstation-csrf'] !== csrf) {
      return { status: 403, error: ['The request was rejected.', 'req-csrf-hooks', 'FG-AUTH-0016'] };
    }
    const input = text === '' ? null : JSON.parse(text);
    const json = (b, status = 200) => ({ status, body: b });

    if (path === '/api/v1/user/notification-preferences') {
      if (method === 'GET') return json(prefs);
      if (method === 'PUT') {
        if (input.language !== null && !['ja', 'en', 'id'].includes(input.language)) {
          return { status: 400, error: ['That language is not offered.', 'req-pref-bad', 'FG-WEB-0006'] };
        }
        prefs = { ...prefs, ...input };
        return json(prefs);
      }
      return null;
    }

    if (path === '/api/v1/projects/7/webhooks') {
      if (method === 'GET') return json({ items: hooks.map(body) });
      if (method === 'POST') {
        if (!/^https:\/\/[a-z0-9.-]+\.example\.test(:\d+)?\//.test(input.url)) {
          return { status: 400, error: ['This destination is not allowed.', 'req-allow', 'FG-INT-0010'] };
        }
        const origin = new URL(input.url).origin;
        const hook = {
          id: nextHook++,
          project_id: 7,
          url: input.url,
          origin,
          enabled: true,
          event_types: [...(input.event_types ?? ['*'])].sort(),
          disabled_reason: null,
          consecutive_failures: 0,
          failing_since: null,
          last_delivery_at: null,
          owner_id: 5,
          previous_secret_valid_until: null,
          secret_version: 1,
          created_at: stamp,
          updated_at: stamp,
        };
        hooks.push(hook);
        return json({ webhook: body(hook), secret: HOOK_SECRET }, 201);
      }
      return null;
    }

    const one = path.match(/^\/api\/v1\/projects\/7\/webhooks\/(\d+)(\/secret|\/deliveries(?:\/(\d+)\/redeliver)?)?$/);
    if (one === null) return null;
    const hook = hooks.find((h) => h.id === Number(one[1]));
    if (hook === undefined) return { status: 404, error: ['That webhook does not exist.', 'req-hook-404', 'FG-REPO-0005'] };
    const sub = one[2];
    if (sub === undefined) {
      if (method === 'GET') return json(body(hook));
      if (method === 'DELETE') {
        hooks.splice(hooks.indexOf(hook), 1);
        return { status: 204 };
      }
      if (method === 'PATCH') {
        const fields = [];
        if (typeof input.url === 'string') {
          hook.url = input.url;
          hook.origin = new URL(input.url).origin;
          fields.push('url');
        }
        if (Array.isArray(input.event_types)) {
          hook.event_types = [...input.event_types].sort();
          fields.push('event_types');
        }
        if (typeof input.enabled === 'boolean' && input.enabled !== hook.enabled) {
          hook.enabled = input.enabled;
          hook.disabled_reason = input.enabled ? null : 'manual';
          hook.consecutive_failures = 0;
          fields.push('enabled');
        }
        return json({ outcome: fields.length > 0 ? 'updated' : 'unchanged', fields, webhook: body(hook) });
      }
      return null;
    }
    if (sub === '/secret' && method === 'POST') {
      hook.secret_version += 1;
      return json({ secret: ROTATED_SECRET, secret_version: hook.secret_version, previous_secret_valid_until: '2026-11-04T01:00:00Z' });
    }
    if (sub === '/deliveries' && method === 'GET') {
      const limit = Number(url.searchParams.get('limit') ?? '20');
      const before = url.searchParams.get('before');
      const older = before === null ? deliveries : deliveries.filter((d) => d.id < Number(before));
      const items = older.slice(0, limit);
      const next = older.length > limit ? (items[items.length - 1]?.id ?? null) : null;
      return json({ items, next_before: next });
    }
    if (one[3] !== undefined && method === 'POST') {
      if (!hook.enabled) {
        return { status: 400, error: ['This webhook is turned off. Enable it first.', 'req-redeliver-off', 'FG-WEB-0006'] };
      }
      if (!refusedOnce) {
        refusedOnce = true;
        return { status: 429, error: ['This delivery is still in progress. Try again in a minute.', 'req-redeliver-429', 'FG-WEB-0010'] };
      }
      return json({ outcome: 'queued' }, 202);
    }
    return null;
  }

  return { answer, hooks };
}
