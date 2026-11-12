import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { i18n } from 'i18next';
import { I18nextProvider } from 'react-i18next';
import { Route, Routes } from 'react-router';

import { ApiFailure, type ApiClient } from './api/client';
import { ApiProvider } from './api/context';
import { watchSessionUser } from './api/namespaces';
import { Layout } from './components/Layout';
import { Activity } from './pages/Activity';
import { GroupsPage } from './pages/Groups';
import { Home } from './pages/Home';
import { Inbox } from './pages/Inbox';
import { ActAsBindingConfirm, ActAsConfirm } from './pages/lazyScreens';
import { MyMergeRequests } from './pages/MyMergeRequests';
import { NamespaceRoute } from './pages/Namespace';
import { NotFound } from './pages/NotFound';
import { Permalink } from './pages/Permalink';
import { ProjectsPage } from './pages/Projects';

/** A 4xx is an answer, not a blip: retry only network failures and 5xx. */
function shouldRetry(failureCount: number, error: unknown): boolean {
  if (failureCount >= 2) return false;
  if (!(error instanceof ApiFailure)) return false;
  const f = error.failure;
  if (f.kind === 'network') return true;
  // A 5xx may pass; a client-side unexpected (status null) will not.
  return f.kind !== 'unauthenticated' && f.status !== null && f.status >= 500;
}

export function createQueryClient(): QueryClient {
  const client = new QueryClient({
    defaultOptions: {
      queries: { retry: shouldRetry, refetchOnWindowFocus: false, staleTime: 30_000 },
    },
  });
  // Lives as long as the client (one per app; tests make their own).
  watchSessionUser(client);
  return client;
}

/** `/` is Home; `/-/projects` and `/-/groups` the "My work" lists (plan 15);
 * `/-/projects/{id}` the permalink; every other path is asked
 * of the resolver (Namespace.tsx). Anything under `/-/` that the SPA has no
 * screen for ends in the not-found page. */
export function AppRoutes() {
  return (
    <Routes>
      {/* The act-as popup: the session gate without the navigation. */}
      <Route element={<Layout bare />}>
        <Route path="-/confirm/projects/:id/merge-requests/:iid" element={<ActAsConfirm />} />
        <Route path="-/confirm/projects/:id/integration-bindings" element={<ActAsBindingConfirm />} />
      </Route>
      <Route element={<Layout />}>
        <Route index element={<Home />} />
        <Route path="-/projects" element={<ProjectsPage />} />
        <Route path="-/groups" element={<GroupsPage />} />
        <Route path="-/merge-requests" element={<MyMergeRequests />} />
        <Route path="-/activity" element={<Activity />} />
        <Route path="-/projects/:id/*" element={<Permalink />} />
        <Route path="-/notifications" element={<Inbox />} />
        <Route path="-/*" element={<NotFound />} />
        <Route path="*" element={<NamespaceRoute />} />
      </Route>
    </Routes>
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
