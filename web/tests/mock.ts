// A route table over `fetch` for the page tests (the same idea as MSW, with
// no dependency: ADR-0022 §1 names MSW for the request layer; every test here
// goes through the real client, so the requests are the real ones).
//
//   const server = mockServer({ 'GET /api/v1/session': () => json(200, SESSION) });
//   await renderApp({ fetch: server.fetch });
//   expect(server.unmatched).toEqual([]);
import { json } from './render';

export interface Call {
  method: string;
  path: string;
  query: URLSearchParams;
  headers: Headers;
  body: unknown;
}

export type Handler = (call: Call) => Response | Promise<Response>;

export function mockServer(routes: Record<string, Handler>) {
  const calls: Call[] = [];
  const unmatched: string[] = [];
  const fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const req = input instanceof Request ? input : new Request(input, init);
    const url = new URL(req.url);
    let body: unknown = null;
    if (req.method !== 'GET' && req.method !== 'HEAD') {
      const text = await req.clone().text();
      body = text === '' ? null : (JSON.parse(text) as unknown);
    }
    const call: Call = {
      method: req.method,
      path: url.pathname,
      query: url.searchParams,
      headers: req.headers,
      body,
    };
    calls.push(call);
    const handler = routes[`${req.method} ${url.pathname}`];
    if (handler === undefined) {
      unmatched.push(`${req.method} ${url.pathname}`);
      return json(500, {
        error_code: 'FG-TEST-0000',
        user_message: 'unmatched test request',
        request_id: 'req-unmatched',
      });
    }
    return handler(call);
  };
  return {
    fetch,
    calls,
    unmatched,
    /** The recorded calls of one route. */
    to: (method: string, path: string) => calls.filter((c) => c.method === method && c.path === path),
  };
}

export const page = (items: unknown[], extra: { has_more?: boolean; next_cursor?: string | null } = {}) =>
  json(200, { items, has_more: false, next_cursor: null, ...extra });

export const apiError = (status: number, user_message: string, request_id: string, code = 'FG-X-0001') =>
  json(status, { error_code: code, user_message, request_id });

export const SESSION = {
  absolute_expires_at: '2026-11-02T00:00:00Z',
  idle_expires_at: '2026-11-01T04:00:00Z',
  public_id: '0123456789abcdef',
  csrf_token: 'c'.repeat(64),
  user: { id: 5, username: 'taro', display_name: '山田 太郎', is_admin: false },
};

export const PROJECT = {
  id: 7,
  namespace_id: 11,
  path: 'acme/forge-web',
  name: 'forge-web',
  display_name: 'Forge Web',
  description: 'The web UI',
  visibility: 'internal',
  created_at: '2026-10-01T00:00:00Z',
  role: 'developer',
};

export const GROUP = {
  id: 3,
  namespace_id: 30,
  parent_id: null,
  path: 'acme',
  name: 'acme',
  display_name: 'Acme Plant',
  description: 'The plant group',
  created_at: '2026-09-01T00:00:00Z',
  role: 'owner',
};

export const LISTED_GROUP = {
  id: 3,
  namespace_id: 30,
  path: 'acme',
  name: 'acme',
  display_name: 'Acme Plant',
  description: 'The plant group',
  created_at: '2026-09-01T00:00:00Z',
  role: 'owner',
};

export const RESOLVE_PROJECT = {
  canonical_path: 'acme/forge-web',
  kind: 'project',
  moved: false,
  namespace_id: 11,
  project_id: 7,
};

export const RESOLVE_GROUP = {
  canonical_path: 'acme',
  kind: 'group',
  moved: false,
  namespace_id: 30,
  group_id: 3,
};

export const REFS_EMPTY = { default_branch: null, refs: [], truncated: false };
export const REFS_FILLED = {
  default_branch: 'main',
  truncated: false,
  refs: [
    { name: 'main', full_name: 'refs/heads/main', type: 'branch', target: 'a'.repeat(40), commit: 'a'.repeat(40) },
    { name: 'dev', full_name: 'refs/heads/dev', type: 'branch', target: 'b'.repeat(40), commit: 'b'.repeat(40) },
    { name: 'v1', full_name: 'refs/tags/v1', type: 'tag', target: 'a'.repeat(40), commit: 'a'.repeat(40) },
  ],
};

/** The routes every signed-in page needs; spread into a test's table. */
export const signedIn = (): Record<string, Handler> => ({
  'GET /api/v1/session': () => json(200, SESSION),
});
