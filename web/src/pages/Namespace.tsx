// Every address that is not a fixed screen: `/{group}/{...}/{project}` and
// `/{group}/{...}/{project}/-/{screen}` (ADR-0015). The SPA cannot know what a
// path means, so it asks the resolver (plan 10 §4.3 "URL rescue"):
//
// - `resolve?prefix=true` finds the longest prefix that is a project, with the
//   rest as `remainder`. A GitHub-shaped `/acme/web/tree/main/src` becomes
//   `/acme/web/-/tree/main/src` (history.replaceState, no new history entry);
// - a moved path (tombstone) or a different spelling is replaced by the
//   canonical path;
// - 404 (unknown, or not visible: the API says the same for both) is the
//   not-found screen.
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Navigate, useLocation } from 'react-router';

import { ApiFailure, call } from '../api/client';
import { useApi } from '../api/context';
import { isViewWord, pathSegments } from '../features/paths';
import { QueryError } from '../components/QueryError';
import { GroupPage, UserPage } from './GroupPage';
import { NotFound, Unavailable } from './NotFound';
import { ProjectPage } from './ProjectPage';

const encode = (segments: string[]) => segments.map(encodeURIComponent).join('/');

export function NamespaceRoute() {
  const { pathname } = useLocation();
  const segments = pathSegments(pathname);
  if (segments === null || segments.length === 0) return <NotFound />;
  const dash = segments.indexOf('-');
  const namespace = dash < 0 ? segments : segments.slice(0, dash);
  // `/-/…` alone is an instance route the SPA has no screen for.
  if (namespace.length === 0) return <NotFound />;
  const view = dash < 0 ? null : segments.slice(dash + 1);
  return <Resolved path={namespace.join('/')} view={view} />;
}

function Resolved({ path, view }: { path: string; view: string[] | null }) {
  const { t } = useTranslation();
  const api = useApi();
  const location = useLocation();
  const prefix = view === null;
  const resolution = useQuery({
    queryKey: ['resolve', path, prefix],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/namespaces/resolve', {
          signal,
          params: { query: { path, ...(prefix ? { prefix: 'true' } : {}) } },
        }),
      ),
    // A path can move or be deleted: ask again on every visit.
    staleTime: 0,
  });

  if (resolution.isPending) return <p role="status">{t('common.loading')}</p>;
  if (resolution.isError) {
    const e = resolution.error;
    if (e instanceof ApiFailure && e.failure.kind === 'api' && e.failure.status === 404) {
      return <NotFound />;
    }
    return (
      <QueryError
        error={e}
        onRetry={() => {
          void resolution.refetch();
        }}
      />
    );
  }

  const r = resolution.data;
  const rest = location.search + location.hash;
  const movedState = r.moved ? { movedFrom: path } : undefined;

  if (view !== null) {
    if (r.canonical_path !== path) {
      const tail = view.length > 0 ? `/-/${encode(view)}` : '';
      return <Navigate replace to={`/${r.canonical_path}${tail}${rest}`} state={movedState} />;
    }
    return r.kind === 'project' ? (
      <Unavailable backTo={`/${r.canonical_path}`} />
    ) : (
      <NotFound />
    );
  }

  if (r.kind === 'project' && r.remainder !== undefined && r.remainder !== null && r.remainder !== '') {
    const remainder = r.remainder.split('/');
    if (!isViewWord(remainder[0])) return <NotFound />;
    return (
      <Navigate
        replace
        to={`/${r.canonical_path}/-/${remainder.map(encodeURIComponent).join('/')}${rest}`}
        state={movedState}
      />
    );
  }
  if (r.canonical_path !== path) {
    return <Navigate replace to={`/${r.canonical_path}${rest}`} state={movedState} />;
  }

  if (r.kind === 'project' && r.project_id != null) return <ProjectPage id={r.project_id} />;
  if (r.kind === 'group' && r.group_id != null) {
    return <GroupPage id={r.group_id} path={r.canonical_path} />;
  }
  if (r.kind === 'user') return <UserPage path={r.canonical_path} namespaceId={r.namespace_id} />;
  return <NotFound />;
}
