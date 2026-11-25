// What a namespace path means (`GET /api/v1/namespaces/resolve`) and the
// project behind it. Every address of a project's screens asks the
// resolver first, so its answer is cached per path (RESOLVE_STALE_MS) and
// shared by all the tabs of the project. What changes a path's meaning drops
// or refreshes the cached answers: a transfer, a deletion or a restore
// (`forgetResolutions`), a visibility change or a member removal
// (`refreshResolutions`), another signed-in user (`watchSessionUser`).
// A failed answer (404, 5xx) is never served from the cache: a query in
// error is asked again when its screen mounts.
import { queryOptions, type QueryClient } from '@tanstack/react-query';

import { RESOLVE_STALE_MS } from './cachePolicy';
import { ApiFailure, call, type ApiClient } from './client';
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
    queryFn: async ({ signal }) => {
      const ask = (top: boolean) =>
        call(() =>
          api.GET('/api/v1/namespaces/resolve', {
            signal,
            params: { query: { path, ...(top ? { prefix: 'true' } : {}) } },
          }),
        );
      if (!prefix) return ask(false);
      try {
        return await ask(true);
      } catch (e) {
        // The top-page form answers with a project only (ADR-0015: a prefix
        // answer never reveals a group or a user), so a 404 there may still
        // be a group or a user page: the exact form decides.
        if (e instanceof ApiFailure && e.failure.kind === 'api' && e.failure.status === 404) {
          return ask(false);
        }
        throw e;
      }
    },
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

/** Who may see what changed (visibility, membership) while the page stays:
 * answers of other paths are dropped, the shown one is read again in the
 * background (dropping it would blank the page under the person). */
export function refreshResolutions(queryClient: QueryClient): Promise<void> {
  queryClient.removeQueries({ queryKey: resolveKeys.all, type: 'inactive' });
  return queryClient.invalidateQueries({ queryKey: resolveKeys.all });
}

/** Drops the cached path meanings whenever the signed-in user changes (a
 * new sign-in in this tab): what one person may see says nothing about
 * another. Returns the unsubscribe. */
export function watchSessionUser(queryClient: QueryClient): () => void {
  let user: number | null | undefined;
  return queryClient.getQueryCache().subscribe((event) => {
    const key = event.query.queryKey as readonly unknown[];
    if (event.type !== 'updated' || key[0] !== 'session') return;
    const data: unknown = event.query.state.data;
    const id =
      typeof data === 'object' && data !== null && 'user' in data && typeof data.user === 'object' && data.user !== null && 'id' in data.user && typeof data.user.id === 'number'
        ? data.user.id
        : null;
    if (user !== undefined && id !== user) void refreshResolutions(queryClient);
    user = id;
  });
}

export const projectQuery = (api: ApiClient, id: number) =>
  queryOptions({
    queryKey: ['project', id],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}', { signal, params: { path: { id } } })),
  });
