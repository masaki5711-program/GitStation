// The search box (plan 20 K5, ADR-0040): `GET /api/v1/search`, page by page
// with the server's opaque `next_cursor`. The server decides which hits the
// caller may read and never says how many matched; the screen shows only
// what it is given. A snippet's matches are character ranges (Unicode code
// points), never HTML: `snippetPieces` cuts the text and the screen draws
// `<mark>` itself.
import { useInfiniteQuery } from '@tanstack/react-query';

import { call } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type SearchHit = S['SearchHit'];
export type SearchSnippet = S['SearchSnippet'];

/** The kinds the server searches, in the order the filter shows them. */
export const SEARCH_KINDS = ['project', 'merge_request'] as const;
export type SearchKind = (typeof SEARCH_KINDS)[number];

export function isSearchKind(value: string | null): value is SearchKind {
  return value !== null && (SEARCH_KINDS as readonly string[]).includes(value);
}

/** The shortest query the server takes, in characters (plan 20 Q7). */
export const MIN_QUERY_CHARS = 2;
/** Hits asked for at a time (the API allows 1-50). */
export const SEARCH_PAGE = 20;

/** Characters as the server counts them (code points, not UTF-16 units). */
export function queryChars(q: string): number {
  return Array.from(q).length;
}

export function useSearch(q: string, kind: SearchKind | null, enabled: boolean) {
  const api = useApi();
  return useInfiniteQuery({
    queryKey: ['search', q, kind] as const,
    initialPageParam: undefined as string | undefined,
    queryFn: ({ signal, pageParam }) =>
      call(() =>
        api.GET('/api/v1/search', {
          signal,
          params: {
            query: {
              q,
              limit: SEARCH_PAGE,
              ...(kind === null ? {} : { scope: kind }),
              ...(pageParam === undefined ? {} : { cursor: pageParam }),
            },
          },
        }),
      ),
    getNextPageParam: (last) => (last.has_more ? (last.next_cursor ?? undefined) : undefined),
    enabled,
  });
}

export interface SnippetPiece {
  text: string;
  mark: boolean;
}

/** The snippet cut at its highlights. Ranges are `[start, end)` in code
 * points; one that is out of order, overlapping, empty or past the end is
 * clamped or skipped, so a bad range never breaks the text. */
export function snippetPieces(snippet: Pick<SearchSnippet, 'text' | 'highlights'>): SnippetPiece[] {
  const chars = Array.from(snippet.text);
  const out: SnippetPiece[] = [];
  let at = 0;
  for (const h of snippet.highlights) {
    if (!Number.isInteger(h.start) || !Number.isInteger(h.end)) continue;
    const start = Math.max(h.start, at);
    const end = Math.min(h.end, chars.length);
    if (start >= end) continue;
    if (start > at) out.push({ text: chars.slice(at, start).join(''), mark: false });
    out.push({ text: chars.slice(start, end).join(''), mark: true });
    at = end;
  }
  if (at < chars.length) out.push({ text: chars.slice(at).join(''), mark: false });
  return out;
}
