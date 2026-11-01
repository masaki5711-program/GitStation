// The API client: types generated from docs/api/openapi.json
// (`npm run gen:api`, checked in CI), never hand-written (ADR-0022 §1).
import createClient, { type Client } from 'openapi-fetch';

import type { paths } from './schema.gen';

/** What the UI may show of a failure: `user_message` + `request_id` only
 * (CLAUDE.md contract 3). `error_code` stays in the logs. */
export type Failure =
  | { kind: 'api'; status: number; userMessage: string; requestId: string }
  | { kind: 'unauthenticated'; status: 401; requestId: string | null }
  | { kind: 'unexpected'; status: number; requestId: string | null }
  | { kind: 'network' };

export class ApiFailure extends Error {
  readonly failure: Failure;

  constructor(failure: Failure) {
    super(failure.kind);
    this.name = 'ApiFailure';
    this.failure = failure;
  }
}

export type ApiClient = Client<paths>;

const REQUEST_ID_HEADER = 'x-request-id';

/** One client per app; `language` is sent as `Accept-Language`, so the
 * server's `user_message` comes back in the UI's language. */
export function createApiClient(options: {
  language: () => string;
  fetch?: typeof globalThis.fetch;
}): ApiClient {
  const client = createClient<paths>({
    // Same origin (the shell and the API are one server, ADR-0022 §2).
    baseUrl: window.location.origin,
    credentials: 'same-origin',
    ...(options.fetch ? { fetch: options.fetch } : {}),
  });
  client.use({
    onRequest({ request }) {
      request.headers.set('Accept-Language', options.language());
      request.headers.set('Accept', 'application/json');
      return request;
    },
  });
  return client;
}

function isRecord(value: unknown): value is Record<string, unknown> {
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
 * [`ApiFailure`] (TanStack Query shows it through ErrorNotice). */
export async function call<T>(
  run: () => Promise<{ data?: T; error?: unknown; response: Response }>,
): Promise<T> {
  let result: { data?: T; error?: unknown; response: Response };
  try {
    result = await run();
  } catch (e) {
    if (e instanceof ApiFailure) throw e;
    throw new ApiFailure({ kind: 'network' });
  }
  if (!result.response.ok || result.data === undefined) {
    throw new ApiFailure(toFailure(result.response, result.error));
  }
  return result.data;
}
