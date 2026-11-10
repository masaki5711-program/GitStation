// Queries of the document API (plan 13 stage 3,
// `/api/v1/projects/{id}/documents[/{doc_id}]`): the document view as JSON.
// A reference more confidential than the document comes as its id only
// (plan 13 Q14): the page never has the target's title to show.
import { useInfiniteQuery, useQuery } from '@tanstack/react-query';

import { call } from './client';
import { useApi } from './context';

export function useDocuments(projectId: number, ref?: string, after?: string) {
  const api = useApi();
  return useQuery({
    queryKey: ['documents', projectId, ref ?? null, after ?? null],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/documents', {
          signal,
          params: {
            path: { id: projectId },
            query: {
              ...(ref !== undefined ? { ref } : {}),
              ...(after !== undefined ? { after } : {}),
            },
          },
        }),
      ),
  });
}

export function useDocument(projectId: number, docId: string, ref?: string, lang?: string) {
  const api = useApi();
  return useQuery({
    queryKey: ['document', projectId, docId, ref ?? null, lang ?? null],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/documents/{doc_id}', {
          signal,
          params: {
            path: { id: projectId, doc_id: docId },
            query: {
              ...(ref !== undefined ? { ref } : {}),
              ...(lang !== undefined ? { lang } : {}),
            },
          },
        }),
      ),
  });
}

/** The documents that refer to one document, a page at a time
 * (`/referenced-by`, keyset by `next_after`). A more confidential referrer
 * comes as its id only (plan 13 Q14). */
export function useReferrers(projectId: number, docId: string, ref?: string) {
  const api = useApi();
  return useInfiniteQuery({
    queryKey: ['referrers', projectId, docId, ref ?? null],
    initialPageParam: undefined as string | undefined,
    queryFn: ({ signal, pageParam }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/documents/{doc_id}/referenced-by', {
          signal,
          params: {
            path: { id: projectId, doc_id: docId },
            query: {
              ...(ref !== undefined ? { ref } : {}),
              ...(pageParam !== undefined ? { after: pageParam } : {}),
            },
          },
        }),
      ),
    getNextPageParam: (last) => last.next_after ?? undefined,
  });
}

/** A document id as the server accepts it (the screen says "not found" for
 * anything else before asking). */
export function isDocId(raw: string): boolean {
  return raw.length > 0 && raw.length <= 128 && /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(raw);
}

/** The id of a plan 13 document (front matter naming a `doctype` and an
 * `id`), read loosely from the text for the link to the document screen;
 * the server decides whether it is one. */
export function documentId(text: string | null): string | null {
  if (text === null) return null;
  const front = /^\uFEFF?---\r?\n([\s\S]*?)\r?\n(?:---|\.\.\.)\r?\n?/.exec(text.slice(0, 16 * 1024));
  if (front === null) return null;
  const lines = (front[1] ?? '').split(/\r?\n/);
  const value = (key: string) =>
    lines
      .map((l) => new RegExp(`^${key}:\\s*(.*?)\\s*$`).exec(l)?.[1])
      .find((v) => v !== undefined);
  const id = value('id');
  if (value('doctype') === undefined || id === undefined) return null;
  return isDocId(id) ? id : null;
}
