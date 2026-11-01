// `/-/projects/{id}[/…]` (ADR-0015 "固定リンク"): survives renames and moves.
// The SPA looks the project up and replaces the address by its current path.
import { useQuery } from '@tanstack/react-query';
import { Navigate, useLocation, useParams } from 'react-router';

import { ApiFailure, call } from '../api/client';
import { Loading } from '../components/Loading';
import { useApi } from '../api/context';
import { QueryError } from '../components/QueryError';
import { NotFound } from './NotFound';

export function Permalink() {
  const api = useApi();
  const location = useLocation();
  const params = useParams();
  const raw = params.id ?? '';
  const id = /^[1-9][0-9]{0,14}$/.test(raw) ? Number(raw) : null;
  const project = useQuery({
    queryKey: ['project', id],
    enabled: id !== null,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}', { signal, params: { path: { id: id ?? 0 } } }),
      ),
  });

  if (id === null) return <NotFound />;
  if (project.isPending) return <Loading />;
  if (project.isError) {
    const e = project.error;
    if (e instanceof ApiFailure && e.failure.kind === 'api' && e.failure.status === 404) {
      return <NotFound />;
    }
    return (
      <QueryError
        error={e}
        onRetry={() => {
          void project.refetch();
        }}
      />
    );
  }
  const rest = params['*'] ?? '';
  const tail = rest === '' ? '' : `/-/${rest.split('/').map(encodeURIComponent).join('/')}`;
  return <Navigate replace to={`/${project.data.path}${tail}${location.search}${location.hash}`} />;
}
