// Synthetic backend for the in-app notifications part of the browser journey
// (M3 U4): the bell's unread count (polled on every signed-in page), the list
// (newest first, `before` pages, `unread=true` filters), marking one read and
// marking all read up to an id. Stateful, so a mark shows in the count; every
// unsafe request must carry the CSRF header. The shapes are those of
// tests/inbox.test.tsx (keep them in step).
const LIST = '/api/v1/user/notifications';

export function inboxBackend(csrf) {
  const items = [
    {
      id: 12,
      kind: 'mr_commented',
      project_id: 7,
      project_path: 'acme/forge-web',
      merge_request_iid: 3,
      merge_request_title: 'Show the clone URL',
      actor_name: 'Hanako Suzuki',
      created_at: '2026-11-09T09:30:00Z',
      read_at: null,
    },
    {
      id: 11,
      kind: 'mr_approved',
      project_id: 7,
      project_path: 'acme/forge-web',
      merge_request_iid: 3,
      merge_request_title: 'Show the clone URL',
      actor_name: 'Hanako Suzuki',
      created_at: '2026-11-09T09:00:00Z',
      read_at: null,
    },
    {
      id: 10,
      kind: 'mr_conflict_predicted',
      project_id: 7,
      project_path: 'acme/forge-web',
      merge_request_iid: 5,
      merge_request_title: 'Merge the approved change',
      actor_name: null,
      created_at: '2026-11-08T18:00:00Z',
      read_at: '2026-11-08T19:00:00Z',
    },
  ];
  const READ_AT = '2026-11-09T10:00:00Z';

  /** `{ status, body }`, `{ status: 204 }` or `{ error: [message, request id, code], status }`;
   * `null` when the request is not one of these. */
  function answer(req, url, text) {
    const path = url.pathname;
    const method = req.method;
    if (path !== LIST && !path.startsWith(`${LIST}/`)) return null;
    if (method !== 'GET' && req.headers['x-gitstation-csrf'] !== csrf) {
      return { status: 403, error: ['The request was rejected.', 'req-csrf-inbox', 'FG-AUTH-0016'] };
    }
    const json = (b, status = 200) => ({ status, body: b });
    if (method === 'GET' && path === `${LIST}/unread-count`) {
      return json({ count: items.filter((i) => i.read_at === null).length, capped: false });
    }
    if (method === 'GET' && path === LIST) {
      const unread = url.searchParams.get('unread') === 'true';
      const before = Number(url.searchParams.get('before') ?? 'Infinity');
      const rows = items.filter((i) => i.id < before && (!unread || i.read_at === null));
      return json({ items: rows, next_before: null });
    }
    if (method === 'POST' && path === `${LIST}/read-all`) {
      const input = text === '' ? {} : JSON.parse(text);
      let marked = 0;
      for (const i of items) {
        if (i.read_at === null && (input.up_to === undefined || i.id <= input.up_to)) {
          i.read_at = READ_AT;
          marked += 1;
        }
      }
      void marked;
      return { status: 204 };
    }
    const one = path.match(/^\/api\/v1\/user\/notifications\/(\d+)\/read$/);
    if (method === 'POST' && one) {
      const hit = items.find((i) => i.id === Number(one[1]));
      if (hit === undefined) return { status: 404, error: ['Not found.', 'req-inbox-404', 'FG-REPO-0005'] };
      hit.read_at ??= READ_AT;
      return json({ id: hit.id, read_at: hit.read_at });
    }
    return null;
  }
  return { answer };
}
