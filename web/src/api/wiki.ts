// A project's wiki (plan 20 W2/W4, ADR-0040 §5): `GET …/wiki` names the
// linked wiki (or the wiki itself, asked on it) when the caller may read
// it; 404 is "no wiki to show" (none, or not readable: the server answers
// both the same). `POST …/wiki` creates and links one. The wiki's pages are
// read through the document API on the wiki project.
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { ApiFailure, call } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

export type WikiLink = components['schemas']['WikiLinkBody'];

export const wikiKeys = {
  link: (project: number) => ['wiki-link', project] as const,
  pages: (wiki: number) => ['documents', wiki, 'wiki-pages'] as const,
  checks: (wiki: number, sha: string) => ['wiki-checks', wiki, sha] as const,
};

/** The wiki to show, `null` when there is none the caller may read. */
export function useWikiLink(projectId: number) {
  const api = useApi();
  return useQuery({
    queryKey: wikiKeys.link(projectId),
    queryFn: async ({ signal }): Promise<WikiLink | null> => {
      try {
        return await call(() =>
          api.GET('/api/v1/projects/{id}/wiki', { signal, params: { path: { id: projectId } } }),
        );
      } catch (e) {
        if (e instanceof ApiFailure && e.failure.kind === 'api' && e.failure.status === 404) {
          return null;
        }
        throw e;
      }
    },
  });
}

export function useCreateWiki(projectId: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () =>
      call(() =>
        api.POST('/api/v1/projects/{id}/wiki', {
          params: { path: { id: projectId } },
          body: {},
        }),
      ),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: wikiKeys.link(projectId) });
    },
  });
}

/** Every page of the wiki's default branch (the document list, page by page). */
export function useWikiPages(wikiId: number | null) {
  const api = useApi();
  return useInfiniteQuery({
    queryKey: wikiKeys.pages(wikiId ?? 0),
    enabled: wikiId !== null,
    initialPageParam: undefined as string | undefined,
    queryFn: ({ signal, pageParam }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/documents', {
          signal,
          params: {
            path: { id: wikiId ?? 0 },
            query: pageParam === undefined ? {} : { after: pageParam },
          },
        }),
      ),
    getNextPageParam: (last) => last.next_after ?? undefined,
  });
}

/** The check runs of the wiki's head (the lint after a direct save). */
export function useWikiChecks(wikiId: number, sha: string | null) {
  const api = useApi();
  return useQuery({
    queryKey: wikiKeys.checks(wikiId, sha ?? ''),
    enabled: sha !== null,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/commits/{sha}/checks', {
          signal,
          params: { path: { id: wikiId, sha: sha ?? '' } },
        }),
      ),
  });
}
