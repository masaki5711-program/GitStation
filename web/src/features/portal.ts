// The document portal's own vocabulary (plan 10 §4.7, 08): what the
// screens call a "version", and how the address names one. Pure functions;
// the screens are in pages/portal.
import type { Chapter } from '../api/portal';
import type { DiffBody, DiffFile, RefEntry, RefsBody } from '../api/types';
import { encodePath } from './repoUrls';

/** What the address says: `?version=` a confirmed version (absent: the latest
 * work in progress), `?compare=` a confirmed version to compare it with,
 * `?chapter=` the chapter's path. The names are the project's own (they may
 * contain `/`), so they live in the query, not in the path. */
export interface PortalView {
  version: string | null;
  compare: string | null;
  chapter: string | null;
}

export function parseView(params: URLSearchParams): PortalView {
  const take = (key: string) => {
    const v = params.get(key);
    return v === null || v === '' ? null : v;
  };
  return { version: take('version'), compare: take('compare'), chapter: take('chapter') };
}

export function portalUrl(project: string, view: Partial<PortalView> = {}): string {
  const q = new URLSearchParams();
  if (view.version != null) q.set('version', view.version);
  if (view.compare != null) q.set('compare', view.compare);
  if (view.chapter != null) q.set('chapter', view.chapter);
  const qs = q.toString();
  return `/${encodePath(project)}/-/docs${qs === '' ? '' : `?${qs}`}`;
}

/** The most confirmed versions the portal lists (and looks the date up for). */
export const MAX_VERSIONS = 50;

const collator = new Intl.Collator('en', { numeric: true, sensitivity: 'base' });

export interface Confirmed {
  /** The name the person gave the version. */
  name: string;
  commit: string;
}

/** The confirmed versions (tags that point at a commit), newest name first
 * by natural order (`rev-10` after `rev-9`), at most MAX_VERSIONS. The screen
 * re-sorts by date once the dates are known. */
export function confirmedVersions(refs: RefsBody): { items: Confirmed[]; cut: boolean } {
  const tags = refs.refs
    .filter((r): r is RefEntry & { commit: string } => r.type === 'tag' && r.commit !== null)
    .map((r) => ({ name: r.name, commit: r.commit }))
    .sort((a, b) => collator.compare(b.name, a.name));
  return { items: tags.slice(0, MAX_VERSIONS), cut: tags.length > MAX_VERSIONS };
}

/** The tip of the default branch: the latest work in progress. */
export function latestCommit(refs: RefsBody): string | null {
  if (refs.default_branch === null) return null;
  const entry = refs.refs.find((r) => r.full_name === `refs/heads/${refs.default_branch}`);
  return entry?.commit ?? null;
}

/** Newest first by date; versions whose date is not known yet keep the name
 * order and come last. */
export function sortByDate<T extends { name: string; at: string | null }>(items: T[]): T[] {
  return [...items].sort((a, b) => {
    if (a.at !== null && b.at !== null) {
      const d = new Date(b.at).getTime() - new Date(a.at).getTime();
      if (d !== 0 && !Number.isNaN(d)) return d;
    } else if (a.at !== null) {
      return -1;
    } else if (b.at !== null) {
      return 1;
    }
    return collator.compare(b.name, a.name);
  });
}

export type ChangeKind = 'added' | 'removed' | 'changed' | 'same';

export interface ChapterChange {
  chapter: Chapter;
  kind: ChangeKind;
  file: DiffFile | null;
}

const chapterOfFile = (f: DiffFile) => f.new_path ?? f.old_path ?? '';

/** The chapters of both versions, each with what happened to it. A chapter
 * that only the older version has was removed; one only the newer has is
 * new. Files of the comparison that are not chapters are only counted. */
export function changesPerChapter(
  older: Chapter[],
  newer: Chapter[],
  diff: DiffBody,
): { chapters: ChapterChange[]; others: number } {
  const byPath = new Map<string, DiffFile>();
  for (const f of diff.files) {
    byPath.set(chapterOfFile(f), f);
    if (f.old_path !== null) byPath.set(f.old_path, f);
  }
  const seen = new Set<string>();
  const out: ChapterChange[] = [];
  const known = new Set([...older, ...newer].map((c) => c.path));
  for (const c of [...newer, ...older]) {
    if (seen.has(c.path)) continue;
    seen.add(c.path);
    const file = byPath.get(c.path) ?? null;
    let kind: ChangeKind = 'same';
    if (file !== null) {
      kind =
        file.status === 'added' ? 'added' : file.status === 'deleted' ? 'removed' : 'changed';
    }
    out.push({ chapter: c, kind, file });
  }
  out.sort((a, b) => a.chapter.path.localeCompare(b.chapter.path, 'en', { numeric: true }));
  const others = new Set(
    diff.files.filter((f) => !known.has(chapterOfFile(f)) && !(f.old_path !== null && known.has(f.old_path))),
  ).size;
  return { chapters: out, others };
}
