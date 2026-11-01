// Queries of the document portal (plan 10 §4.7): the chapters of one
// version of a specification, and the date and message of a version. Both are
// read from the same browse API as the file screens; the portal only asks
// differently.
import { useQueries, useQuery } from '@tanstack/react-query';

import { ApiFailure, call, type ApiClient } from './client';
import { useApi } from './context';
import type { TreeBody } from './types';
import { isMarkdownPath } from '../features/repoUrls';

export interface Chapter {
  /** Path in the project (`docs/specs/01-overview.md`). */
  path: string;
  /** The file name without its extension. */
  title: string;
}

export interface Chapters {
  /** Where the chapters were found: `docs/specs`, or '' (the top level). */
  dir: string;
  chapters: Chapter[];
  /** More files than the server lists. */
  truncated: boolean;
}

export const SPECS_DIR = 'docs/specs';

const collator = new Intl.Collator('en', { numeric: true, sensitivity: 'base' });

/** `01-overview.md` → `01-overview`. */
export function chapterTitle(path: string): string {
  const name = path.slice(path.lastIndexOf('/') + 1);
  const dot = name.lastIndexOf('.');
  return dot <= 0 ? name : name.slice(0, dot);
}

/** The Markdown files of one level of a tree, in natural name order. */
export function chaptersOf(tree: TreeBody): Chapter[] {
  return tree.entries
    .filter((e) => e.type === 'blob' && isMarkdownPath(e.name))
    .sort((a, b) => collator.compare(a.name, b.name))
    .map((e) => ({ path: e.path, title: chapterTitle(e.path) }));
}

function isNotFound(e: unknown): boolean {
  return e instanceof ApiFailure && e.failure.kind === 'api' && e.failure.status === 404;
}

async function tree(
  api: ApiClient,
  id: number,
  ref: string,
  path: string,
  signal: AbortSignal,
): Promise<TreeBody> {
  return call(() =>
    api.GET('/api/v1/projects/{id}/repository/tree', {
      signal,
      params: { path: { id }, query: { ref, ...(path === '' ? {} : { path }) } },
    }),
  );
}

/** `docs/specs/*.md`; when there is none, the top-level `*.md`. A missing
 * `docs/specs` is not an error (it is the usual case for a small
 * specification); any other failure is. */
export function useChapters(projectId: number, commit: string | null) {
  const api = useApi();
  return useQuery({
    queryKey: ['portal-chapters', projectId, commit],
    enabled: commit !== null,
    queryFn: async ({ signal }): Promise<Chapters> => {
      const ref = commit ?? '';
      let specs: TreeBody | null = null;
      try {
        specs = await tree(api, projectId, ref, SPECS_DIR, signal);
      } catch (e) {
        if (!isNotFound(e)) throw e;
      }
      if (specs !== null) {
        const found = chaptersOf(specs);
        if (found.length > 0) return { dir: SPECS_DIR, chapters: found, truncated: specs.truncated };
      }
      const root = await tree(api, projectId, ref, '', signal);
      return { dir: '', chapters: chaptersOf(root), truncated: root.truncated };
    },
  });
}

export interface Moment {
  /** ISO 8601. */
  at: string;
  /** What the person wrote when they confirmed the version. */
  message: string;
}

/** When a version was made and the message that goes with it: the newest
 * entry of its history (`limit=1`; the whole detail of a change would bring
 * its differences along). Keyed by the commit id, so it never goes stale. */
export function useMoments(projectId: number, commits: string[]) {
  const api = useApi();
  return useQueries({
    queries: commits.map((commit) => ({
      queryKey: ['portal-moment', projectId, commit],
      staleTime: Infinity,
      queryFn: async ({ signal }: { signal: AbortSignal }): Promise<Moment | null> => {
        const page = await call(() =>
          api.GET('/api/v1/projects/{id}/repository/commits', {
            signal,
            params: { path: { id: projectId }, query: { ref: commit, limit: 1 } },
          }),
        );
        const first = page.commits[0];
        return first === undefined ? null : { at: first.committed_at, message: first.subject };
      },
    })),
  });
}
