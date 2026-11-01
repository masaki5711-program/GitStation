// The API client: types generated from docs/api/openapi.json
// (`npm run gen:api`, checked in CI), never hand-written (ADR-0022 §1).
import createClient, { type Client } from 'openapi-fetch';

import type { paths } from './schema.gen';

/** What the UI may show of a failure: `user_message` + `request_id` only
 * (CLAUDE.md contract 3). `error_code` stays in the logs. */
export type Failure =
  | { kind: 'api'; status: number; userMessage: string; requestId: string }
  | { kind: 'unauthenticated'; status: 401; requestId: string | null }
  /** An answer outside the error contract, or a failure in the client
   * itself (`status` null: no answer to speak of). */
  | { kind: 'unexpected'; status: number | null; requestId: string | null }
  | { kind: 'network' };

export class ApiFailure extends Error {
  readonly failure: Failure;

  /** `cause`: the original error (kept for the console, never shown). */
  constructor(failure: Failure, cause?: unknown) {
    super(failure.kind, cause === undefined ? undefined : { cause });
    this.name = 'ApiFailure';
    this.failure = failure;
  }
}

/** A failure of the client code itself (not the network, not the server):
 * logged once here, with the original error, and shown as `unexpected`. */
export function unexpectedFailure(cause: unknown, where: string): ApiFailure {
  console.error(`gitstation: unexpected failure in ${where}`, cause);
  return new ApiFailure({ kind: 'unexpected', status: null, requestId: null }, cause);
}

/** Any error a query can end with, as what the UI shows. Non-`ApiFailure`
 * errors are logged once (by `unexpectedFailure`) by the caller. */
export function failureOf(error: unknown): Failure {
  return error instanceof ApiFailure
    ? error.failure
    : { kind: 'unexpected', status: null, requestId: null };
}

function isAbort(e: unknown): boolean {
  return e instanceof DOMException && e.name === 'AbortError';
}

/** The CSRF token of the web session (`GET /api/v1/session`), kept in
 * memory only: never in storage, never logged. */
export interface CsrfStore {
  token: string | null;
}

/** What the pages need besides the typed calls. */
export interface ClientEnv {
  readonly csrf: CsrfStore;
  /** The `fetch` the client uses (the login probe goes through it too). */
  readonly fetch: typeof globalThis.fetch;
  /** Leave the SPA for a server route (login): `window.location.assign`. */
  readonly assign: (url: string) => void;
  readonly language: () => string;
}

export type ApiClient = Client<paths> & ClientEnv;

const REQUEST_ID_HEADER = 'x-request-id';
const CSRF_HEADER = 'X-GitStation-CSRF';
const SAFE_METHODS = new Set(['GET', 'HEAD', 'OPTIONS']);

/** One client per app; `language` is sent as `Accept-Language`, so the
 * server's `user_message` comes back in the UI's language. */
export function createApiClient(options: {
  language: () => string;
  fetch?: typeof globalThis.fetch;
  assign?: (url: string) => void;
}): ApiClient {
  const csrf: CsrfStore = { token: null };
  const rawFetch: typeof globalThis.fetch =
    options.fetch ?? ((input, init) => globalThis.fetch(input, init));
  const client = createClient<paths>({
    // Same origin (the shell and the API are one server, ADR-0022 §2).
    baseUrl: window.location.origin,
    credentials: 'same-origin',
    fetch: rawFetch,
  });
  client.use({
    onRequest({ request }) {
      request.headers.set('Accept-Language', options.language());
      request.headers.set('Accept', 'application/json');
      // Every unsafe request of a session carries the CSRF token (ADR-0021 §6).
      if (!SAFE_METHODS.has(request.method) && csrf.token !== null) {
        request.headers.set(CSRF_HEADER, csrf.token);
      }
      return request;
    },
  });
  return Object.assign(client, {
    csrf,
    fetch: rawFetch,
    assign:
      options.assign ??
      ((url: string) => {
        window.location.assign(url);
      }),
    language: options.language,
  });
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** The server's error body `{ error_code, user_message, request_id }` into
 * what the UI shows; anything else is `unexpected` (never shown raw). */
export function toFailure(response: Response, body: unknown): Failure {
  const headerId = response.headers.get(REQUEST_ID_HEADER);
  if (response.status === 401) {
    const id = isRecord(body) && typeof body.request_id === 'string' ? body.request_id : headerId;
    return { kind: 'unauthenticated', status: 401, requestId: id };
  }
  if (
    isRecord(body) &&
    typeof body.user_message === 'string' &&
    typeof body.request_id === 'string' &&
    body.user_message.length > 0
  ) {
    return {
      kind: 'api',
      status: response.status,
      userMessage: body.user_message,
      requestId: body.request_id,
    };
  }
  return { kind: 'unexpected', status: response.status, requestId: headerId };
}

/** Run one openapi-fetch call; a failed answer or a network error throws an
 * [`ApiFailure`] (TanStack Query shows it through ErrorNotice).
 *
 * - an abort (the query was cancelled) is rethrown as it is: not a failure;
 * - `TypeError` is what `fetch` throws when no answer arrived: `network`;
 * - anything else (a body that is not JSON, a bug) is `unexpected`, logged
 *   once with the original error. */
export async function call<T>(
  run: () => Promise<{ data?: T; error?: unknown; response: Response }>,
): Promise<T> {
  let result: { data?: T; error?: unknown; response: Response };
  try {
    result = await run();
  } catch (e) {
    if (e instanceof ApiFailure || isAbort(e)) throw e;
    if (e instanceof TypeError) throw new ApiFailure({ kind: 'network' }, e);
    throw unexpectedFailure(e, 'an API call');
  }
  if (result.response.ok && result.data === undefined) {
    throw unexpectedFailure(
      new Error(`a ${result.response.status} answer without a body`),
      'an API call',
    );
  }
  if (!result.response.ok || result.data === undefined) {
    throw new ApiFailure(toFailure(result.response, result.error));
  }
  return result.data;
}

/** Like [`call`] for an answer that has no body on success (204): only a
 * failed answer throws. */
export async function callNoContent(
  run: () => Promise<{ error?: unknown; response: Response }>,
): Promise<void> {
  let result: { error?: unknown; response: Response };
  try {
    result = await run();
  } catch (e) {
    if (e instanceof ApiFailure || isAbort(e)) throw e;
    if (e instanceof TypeError) throw new ApiFailure({ kind: 'network' }, e);
    throw unexpectedFailure(e, 'an API call');
  }
  if (!result.response.ok) throw new ApiFailure(toFailure(result.response, result.error));
}
