// Queries of the repository browse API (`/api/v1/projects/{id}/repository/…`,
// ADR-0015 amendments MR7 / MR8). A screen address carries `{ref}[/{path}]`
// as one tail: the server's `resolve` splits it (the longest branch, then tag),
// and every later call passes the resolved full ref name (or commit id).
import { useInfiniteQuery, useQuery } from '@tanstack/react-query';

import { call } from './client';
import { useApi } from './context';
import type { Rev } from './types';

/** The ref to pass on: the full name (`refs/heads/main`) or the commit id. */
export function refParam(rev: Rev): string {
  return rev.full_name ?? rev.commit;
}

export interface Spec {
  rev: Rev;
  path: string;
}

export function useRefs(projectId: number) {
  const api = useApi();
  return useQuery({
    queryKey: ['refs', projectId],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/refs', {
          signal,
          params: { path: { id: projectId } },
        }),
      ),
  });
}

/** `main/src/lib.rs` → the revision and the path. An empty spec is the
 * default branch's root (a repository with no branch has no such spec: the
 * query then ends in `empty`). */
export function useSpec(projectId: number, spec: string) {
  const api = useApi();
  const refs = useRefs(projectId);
  const explicit = spec !== '';
  const resolved = useQuery({
    queryKey: ['resolve-spec', projectId, spec],
    enabled: explicit,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/resolve', {
          signal,
          params: { path: { id: projectId }, query: { spec } },
        }),
      ),
  });
  if (explicit) {
    return {
      status: resolved.status,
      error: resolved.error,
      refetch: resolved.refetch,
      data:
        resolved.data === undefined
          ? undefined
          : ({ rev: resolved.data.ref, path: resolved.data.path } satisfies Spec),
      empty: false,
    } as const;
  }
  const branch = refs.data?.default_branch ?? null;
  const entry = branch === null ? undefined : refs.data?.refs.find((r) => r.full_name === `refs/heads/${branch}`);
  const data: Spec | undefined =
    entry !== undefined && entry.commit !== null
      ? {
          rev: { type: 'branch', name: entry.name, full_name: entry.full_name, commit: entry.commit },
          path: '',
        }
      : undefined;
  return {
    status: refs.status,
    error: refs.error,
    refetch: refs.refetch,
    data,
    empty: refs.isSuccess && data === undefined,
  } as const;
}

export function useTree(projectId: number, ref: string, path: string) {
  const api = useApi();
  return useQuery({
    queryKey: ['tree', projectId, ref, path],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/tree', {
          signal,
          params: { path: { id: projectId }, query: { ref, ...(path === '' ? {} : { path }) } },
        }),
      ),
  });
}

export function useBlob(projectId: number, ref: string, path: string, enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: ['blob', projectId, ref, path],
    enabled,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/blob', {
          signal,
          params: { path: { id: projectId }, query: { ref, path } },
        }),
      ),
  });
}

export function useBlame(projectId: number, ref: string, path: string) {
  const api = useApi();
  return useQuery({
    queryKey: ['blame', projectId, ref, path],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/blame', {
          signal,
          params: { path: { id: projectId }, query: { ref, path } },
        }),
      ),
  });
}

/** History pages by keyset cursor: the next page continues from the commit
 * the first page was read at, so a push in between does not shift it. */
export function useCommits(projectId: number, ref: string, path: string) {
  const api = useApi();
  return useInfiniteQuery({
    queryKey: ['commits', projectId, ref, path],
    initialPageParam: undefined as string | undefined,
    queryFn: ({ signal, pageParam }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/commits', {
          signal,
          params: {
            path: { id: projectId },
            query: {
              ref,
              ...(path === '' ? {} : { path }),
              ...(pageParam === undefined ? {} : { cursor: pageParam }),
            },
          },
        }),
      ),
    getNextPageParam: (last) => (last.has_more ? (last.next_cursor ?? undefined) : undefined),
  });
}

export function useCommit(projectId: number, oid: string, enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: ['commit', projectId, oid],
    enabled,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/commits/{oid}', {
          signal,
          params: { path: { id: projectId, oid } },
        }),
      ),
  });
}

export function useCompare(projectId: number, from: string | null, to: string | null) {
  const api = useApi();
  return useQuery({
    queryKey: ['compare', projectId, from, to],
    enabled: from !== null && to !== null,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/compare', {
          signal,
          params: { path: { id: projectId }, query: { from: from ?? '', to: to ?? '' } },
        }),
      ),
  });
}
