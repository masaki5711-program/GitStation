import { render } from '@testing-library/react';
import { StrictMode, type ReactNode } from 'react';
import { MemoryRouter, useLocation } from 'react-router';

import { createApiClient } from '../src/api/client';
import { AppProviders, AppRoutes, createQueryClient } from '../src/App';
import { createI18n, type Language } from '../src/i18n';

export interface Recorded {
  url: string;
  headers: Headers;
}

/** A fetch that answers one canned response and records the requests. */
export function fakeFetch(answer: () => Response | Promise<Response>) {
  const calls: Recorded[] = [];
  const fetch = (input: RequestInfo | URL, init?: RequestInit) => {
    const req = input instanceof Request ? input : new Request(input, init);
    calls.push({ url: req.url, headers: req.headers });
    return Promise.resolve(answer());
  };
  return { fetch, calls };
}

export function json(status: number, body: unknown, headers: Record<string, string> = {}) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json', ...headers },
  });
}

/** Shows the router's location, so a test can see a `replace`. */
function LocationProbe() {
  const location = useLocation();
  return (
    <output data-testid="location">{location.pathname + location.search + location.hash}</output>
  );
}

export async function renderApp(options: {
  fetch: typeof globalThis.fetch;
  path?: string;
  lang?: Language;
  assign?: (url: string) => void;
  /** Router state of the first entry (a link's `state`). */
  state?: unknown;
  /** Render under StrictMode, as main.tsx does (effects run twice). */
  strict?: boolean;
  /** Keep the app's query stale time (createQueryClient) instead of 0. */
  appStaleTime?: boolean;
}) {
  const i18n = await createI18n(options.lang ?? 'ja');
  const api = createApiClient({
    language: () => i18n.language,
    fetch: options.fetch,
    ...(options.assign ? { assign: options.assign } : {}),
  });
  const queryClient = createQueryClient();
  // No retries in tests. `appStaleTime` keeps the app's stale time (the
  // cache tests); otherwise every mount reads again (staleTime 0).
  queryClient.setDefaultOptions({
    queries:
      options.appStaleTime === true ? { ...queryClient.getDefaultOptions().queries, retry: false } : { retry: false },
  });
  const entry =
    options.state === undefined
      ? (options.path ?? '/')
      : { pathname: (options.path ?? '/').split('?')[0] ?? '/', state: options.state };
  const tree = (
    <AppProviders i18n={i18n} api={api} queryClient={queryClient}>
      <MemoryRouter initialEntries={[entry]}>
        <AppRoutes />
        <LocationProbe />
      </MemoryRouter>
    </AppProviders>
  );
  const wrap = (node: ReactNode) => (options.strict === true ? <StrictMode>{node}</StrictMode> : node);
  return { api, queryClient, ...render(wrap(tree)) };
}
