// The list queries shared by Home and the namespace pages. Keys start with
// 'projects' / 'groups' so one invalidation after a create refreshes all.
import { queryOptions } from '@tanstack/react-query';

import { call, type ApiClient } from './client';
import { fetchAllPages } from './pages';
import type { ListedGroup, Project } from './types';

export function projectsQuery(
  api: ApiClient,
  filter: { membership?: boolean; kind?: 'spec' | 'system' } = {},
) {
  return queryOptions({
    queryKey: ['projects', filter],
    queryFn: ({ signal }) =>
      fetchAllPages<Project>((cursor) =>
        call(() =>
          api.GET('/api/v1/projects', {
            signal,
            params: {
              query: {
                limit: 100,
                ...(cursor === undefined ? {} : { cursor }),
                ...(filter.membership === true ? { membership: 'true' } : {}),
                ...(filter.kind === undefined ? {} : { kind: filter.kind }),
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
