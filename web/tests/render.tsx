import { render } from '@testing-library/react';
import { MemoryRouter } from 'react-router';

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

export async function renderApp(options: {
  fetch: typeof globalThis.fetch;
  path?: string;
  lang?: Language;
}) {
  const i18n = await createI18n(options.lang ?? 'ja');
  const api = createApiClient({ language: () => i18n.language, fetch: options.fetch });
  const queryClient = createQueryClient();
  queryClient.setDefaultOptions({ queries: { retry: false } });
  return render(
    <AppProviders i18n={i18n} api={api} queryClient={queryClient}>
      <MemoryRouter initialEntries={[options.path ?? '/']}>
        <AppRoutes />
      </MemoryRouter>
    </AppProviders>,
  );
}
