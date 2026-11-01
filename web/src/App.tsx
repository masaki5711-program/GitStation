import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { i18n } from 'i18next';
import { I18nextProvider } from 'react-i18next';
import { Route, Routes } from 'react-router';

import { ApiFailure, type ApiClient } from './api/client';
import { ApiProvider } from './api/context';
import { Header } from './components/Header';
import { Home } from './pages/Home';
import { NotFound } from './pages/NotFound';
import styles from './App.module.css';

/** A 4xx is an answer, not a blip: retry only network failures and 5xx. */
function shouldRetry(failureCount: number, error: unknown): boolean {
  if (failureCount >= 2) return false;
  if (!(error instanceof ApiFailure)) return false;
  const f = error.failure;
  return f.kind === 'network' || (f.kind !== 'unauthenticated' && f.status >= 500);
}

export function createQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: { retry: shouldRetry, refetchOnWindowFocus: false, staleTime: 30_000 },
    },
  });
}

export function AppRoutes() {
  return (
    <div className={styles.app}>
      <Header />
      <main id="main" className={styles.main}>
        <Routes>
          <Route path="/" element={<Home />} />
          <Route path="*" element={<NotFound />} />
        </Routes>
      </main>
    </div>
  );
}

/** Everything but the router (main.tsx adds BrowserRouter, tests a
 * MemoryRouter). */
export function AppProviders({
  i18n,
  api,
  queryClient,
  children,
}: {
  i18n: i18n;
  api: ApiClient;
  queryClient: QueryClient;
  children: React.ReactNode;
}) {
  return (
    <I18nextProvider i18n={i18n}>
      <ApiProvider client={api}>
        <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
      </ApiProvider>
    </I18nextProvider>
  );
}
