// The list queries shared by Home and the namespace pages. Keys start with
// 'projects' / 'groups' so one invalidation after a create refreshes all.
import { queryOptions } from '@tanstack/react-query';

import { call, type ApiClient } from './client';
import { fetchAllPages } from './pages';
import type { ListedGroup, Project } from './types';

/**
 * Which projects a list asks for: `all` the ones I may read; `member` the ones
 * I hold a role in (`membership=true`); `created` the ones I created
 * (`created_by=me`, plan 15 MR 5 — the server accepts only `me`).
 */
export type ProjectScope = 'all' | 'member' | 'created';

const SCOPE_QUERY: Record<ProjectScope, { membership?: 'true'; created_by?: 'me' }> = {
  all: {},
  member: { membership: 'true' },
  created: { created_by: 'me' },
};

export function projectsQuery(api: ApiClient, scope: ProjectScope = 'all') {
  return queryOptions({
    queryKey: ['projects', scope],
    queryFn: ({ signal }) =>
      fetchAllPages<Project>((cursor) =>
        call(() =>
          api.GET('/api/v1/projects', {
            signal,
            params: {
              query: {
                limit: 100,
                ...(cursor === undefined ? {} : { cursor }),
                ...SCOPE_QUERY[scope],
              },
            },
          }),
        ),
      ),
  });
}

export function groupsQuery(api: ApiClient, filter: { membership?: boolean } = {}) {
  return queryOptions({
    queryKey: ['groups', filter],
    queryFn: ({ signal }) =>
      fetchAllPages<ListedGroup>((cursor) =>
        call(() =>
          api.GET('/api/v1/groups', {
            signal,
            params: {
              query: {
                limit: 100,
                ...(cursor === undefined ? {} : { cursor }),
                ...(filter.membership === true ? { membership: 'true' } : {}),
              },
            },
          }),
        ),
      ),
  });
}

/** The parent of a full path (`a/b/c` → `a/b`; a top-level path → ''). */
export function parentPath(path: string): string {
  const i = path.lastIndexOf('/');
  return i < 0 ? '' : path.slice(0, i);
}
