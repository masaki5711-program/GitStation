// Starting the browser sign-in (ADR-0021 §3): the server route
// `GET /-/auth/login?next=` answers 302 to the identity provider, or an
// error body (503 FG-AUTH-0018 when no provider is configured). A plain link
// would show that JSON to a person, so the UI first asks with
// `redirect: 'manual'` and only then leaves the SPA.
import { isRecord, toFailure, type ApiClient, type Failure } from '../api/client';

export const LOGIN_PATH = '/-/auth/login';
const NOT_CONFIGURED = 'FG-AUTH-0018';

export type SignInResult =
  | { kind: 'redirecting' }
  | { kind: 'notConfigured'; requestId: string }
  | { kind: 'failed'; failure: Failure };

/** `next` must be a path of this app: `/…`, not `//…`, no backslash or
 * control character (the server applies the same rule and falls back to `/`). */
export function safeNext(path: string): string {
  // eslint-disable-next-line no-control-regex
  if (!path.startsWith('/') || path.startsWith('//') || /[\\\u0000-\u001f\u007f]/.test(path)) {
    return '/';
  }
  return path;
}

export function loginUrl(next: string): string {
  return `${LOGIN_PATH}?next=${encodeURIComponent(safeNext(next))}`;
}

export async function startSignIn(api: ApiClient, next: string): Promise<SignInResult> {
  const url = loginUrl(next);
  let response: Response;
  try {
    response = await api.fetch(`${window.location.origin}${url}`, {
      redirect: 'manual',
      credentials: 'same-origin',
      headers: { Accept: 'application/json', 'Accept-Language': api.language() },
    });
  } catch {
    // `fetch` rejects only when no answer arrived.
    return { kind: 'failed', failure: { kind: 'network' } };
  }
  if (response.type === 'opaqueredirect' || (response.status >= 300 && response.status < 400)) {
    api.assign(url);
    return { kind: 'redirecting' };
  }
  let body: unknown = null;
  try {
    body = await response.json();
  } catch {
    // Not JSON: toFailure() reports it as unexpected, never raw.
  }
  if (
    response.status === 503 &&
    isRecord(body) &&
    body.error_code === NOT_CONFIGURED &&
    typeof body.request_id === 'string'
  ) {
    return { kind: 'notConfigured', requestId: body.request_id };
  }
  return { kind: 'failed', failure: toFailure(response, body) };
}
