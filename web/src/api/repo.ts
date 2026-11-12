// Queries of the repository browse API (`/api/v1/projects/{id}/repository/…`,
// ADR-0015 amendments MR7 / MR8). A screen address carries `{ref}[/{path}]`
// as one tail: the server's `resolve` splits it (the longest branch, then tag),
// and every later call passes the resolved full ref name (or commit id).
import { infiniteQueryOptions, queryOptions, useInfiniteQuery, useQuery } from '@tanstack/react-query';

import { cacheFor } from './cachePolicy';
import { call, type ApiClient } from './client';
import { useApi } from './context';
import type { RefsBody, Rev } from './types';

/** The ref to pass on: the full name (`refs/heads/main`) or the commit id. */
export function refParam(rev: Rev): string {
  return rev.full_name ?? rev.commit;
}

export interface Spec {
  rev: Rev;
  path: string;
}

// The query options are shared by the screens' hooks and by prefetching
// (features/prefetch.ts), so both use one key and one request.

export const refsQuery = (api: ApiClient, projectId: number) =>
  queryOptions({
    queryKey: ['refs', projectId],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/refs', {
          signal,
          params: { path: { id: projectId } },
        }),
      ),
  });

export const resolveSpecQuery = (api: ApiClient, projectId: number, spec: string) =>
  queryOptions({
    queryKey: ['resolve-spec', projectId, spec],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/resolve', {
          signal,
          params: { path: { id: projectId }, query: { spec } },
        }),
      ),
  });

export const treeQuery = (api: ApiClient, projectId: number, ref: string, path: string) =>
  queryOptions({
    queryKey: ['tree', projectId, ref, path],
    ...cacheFor(ref),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/tree', {
          signal,
          params: { path: { id: projectId }, query: { ref, ...(path === '' ? {} : { path }) } },
        }),
      ),
  });

export const blobQuery = (api: ApiClient, projectId: number, ref: string, path: string) =>
  queryOptions({
    queryKey: ['blob', projectId, ref, path],
    ...cacheFor(ref),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/blob', {
          signal,
          params: { path: { id: projectId }, query: { ref, path } },
        }),
      ),
  });

export const blameQuery = (api: ApiClient, projectId: number, ref: string, path: string) =>
  queryOptions({
    queryKey: ['blame', projectId, ref, path],
    ...cacheFor(ref),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/blame', {
          signal,
          params: { path: { id: projectId }, query: { ref, path } },
        }),
      ),
  });

/** History pages by keyset cursor: the next page continues from the commit
 * the first page was read at, so a push in between does not shift it. */
export const commitsQuery = (api: ApiClient, projectId: number, ref: string, path: string) =>
  infiniteQueryOptions({
    queryKey: ['commits', projectId, ref, path],
    ...cacheFor(ref),
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

export const commitQuery = (api: ApiClient, projectId: number, oid: string) =>
  queryOptions({
    queryKey: ['commit', projectId, oid],
    ...cacheFor(oid),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/commits/{oid}', {
          signal,
          params: { path: { id: projectId, oid } },
        }),
      ),
  });

/** The newest commit of a ref (the project page's commit strip). */
export const latestCommitQuery = (api: ApiClient, projectId: number, ref: string) =>
  queryOptions({
    queryKey: ['commits-latest', projectId, ref],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/commits', {
          signal,
          params: { path: { id: projectId }, query: { ref, limit: 1 } },
        }),
      ),
  });

export function useRefs(projectId: number) {
  return useQuery(refsQuery(useApi(), projectId));
}

/** `main/src/lib.rs` → the revision and the path. An empty spec is the
 * default branch's root (a repository with no branch has no such spec: the
 * query then ends in `empty`). */
export function useSpec(projectId: number, spec: string) {
  const api = useApi();
  const refs = useRefs(projectId);
  const explicit = spec !== '';
  const resolved = useQuery({ ...resolveSpecQuery(api, projectId, spec), enabled: explicit });
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
  const data = defaultSpec(refs.data);
  return {
    status: refs.status,
    error: refs.error,
    refetch: refs.refetch,
    data,
    empty: refs.isSuccess && data === undefined,
  } as const;
}

/** The default branch's root, if the repository has one. */
export function defaultSpec(refs: RefsBody | undefined): Spec | undefined {
  const branch = refs?.default_branch ?? null;
  const entry = branch === null ? undefined : refs?.refs.find((r) => r.full_name === `refs/heads/${branch}`);
  return entry !== undefined && entry.commit !== null
    ? {
        rev: { type: 'branch', name: entry.name, full_name: entry.full_name, commit: entry.commit },
        path: '',
      }
    : undefined;
}

export function useTree(projectId: number, ref: string, path: string, enabled = true) {
  return useQuery({ ...treeQuery(useApi(), projectId, ref, path), enabled });
}

export function useBlob(projectId: number, ref: string, path: string, enabled = true) {
  return useQuery({ ...blobQuery(useApi(), projectId, ref, path), enabled });
}

export function useBlame(projectId: number, ref: string, path: string) {
  return useQuery(blameQuery(useApi(), projectId, ref, path));
}

export function useCommits(projectId: number, ref: string, path: string) {
  return useInfiniteQuery(commitsQuery(useApi(), projectId, ref, path));
}

export function useCommit(projectId: number, oid: string, enabled = true) {
  return useQuery({ ...commitQuery(useApi(), projectId, oid), enabled });
}

/** Never immutable, even between two hex strings: the screen's `from` and
 * `to` are typed names, and a branch may be named like a commit id. */
export const compareQuery = (api: ApiClient, projectId: number, from: string, to: string) =>
  queryOptions({
    queryKey: ['compare', projectId, from, to],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/compare', {
          signal,
          params: { path: { id: projectId }, query: { from, to } },
        }),
      ),
  });

export function useCompare(projectId: number, from: string | null, to: string | null) {
  return useQuery({
    ...compareQuery(useApi(), projectId, from ?? '', to ?? ''),
    enabled: from !== null && to !== null,
  });
}
