// What a namespace path means (`GET /api/v1/namespaces/resolve`) and the
// project behind it. Every address of a project's screens asks the
// resolver first, so its answer is cached per path (RESOLVE_STALE_MS) and
// shared by all the tabs of the project; a transfer, a deletion or a
// restore drops every cached answer (`forgetResolutions`).
import { queryOptions, type QueryClient } from '@tanstack/react-query';

import { RESOLVE_STALE_MS } from './cachePolicy';
import { call, type ApiClient } from './client';
import type { Resolution } from './types';

export const resolveKeys = {
  all: ['resolve'] as const,
  /** `prefix`: the project top page form (`/{path}`, a GitHub-shaped tail
   * is the `remainder`); `false` for `/{path}/-/{screen}`. */
  one: (path: string, prefix: boolean) => ['resolve', path, prefix] as const,
};

/** The other form's answer stands for this one when it says the path itself
 * is the namespace (canonical, nothing after it): `/acme/web` and
 * `/acme/web/-/tree` then ask the server once between them. */
function sameAnswer(other: Resolution | undefined, path: string): Resolution | undefined {
  if (other === undefined || other.canonical_path !== path) return undefined;
  if (other.remainder !== undefined && other.remainder !== null && other.remainder !== '') return undefined;
  return other;
}

export const resolveQuery = (api: ApiClient, queryClient: QueryClient, path: string, prefix: boolean) => {
  const otherKey = resolveKeys.one(path, !prefix);
  return queryOptions({
    queryKey: resolveKeys.one(path, prefix),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/namespaces/resolve', {
          signal,
          params: { query: { path, ...(prefix ? { prefix: 'true' } : {}) } },
        }),
      ),
    staleTime: RESOLVE_STALE_MS,
    initialData: () => sameAnswer(queryClient.getQueryData<Resolution>(otherKey), path),
    initialDataUpdatedAt: () => queryClient.getQueryState(otherKey)?.dataUpdatedAt,
  });
};

/** After a transfer, a deletion or a restore: no cached path meaning may
 * stand (a removed query is asked again, never shown stale). */
export function forgetResolutions(queryClient: QueryClient): void {
  queryClient.removeQueries({ queryKey: resolveKeys.all });
}

export const projectQuery = (api: ApiClient, id: number) =>
  queryOptions({
    queryKey: ['project', id],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}', { signal, params: { path: { id } } })),
  });
