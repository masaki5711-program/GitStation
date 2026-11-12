// The simple editor's pure parts (no React): counts, the generated branch
// name, the hint whether a direct commit is refused by a protected-branch
// rule, and the path check before sending. The server decides everything
// again; these only spare a round trip and choose the words.
import { roleRank } from '../api/fileCommits';
import type { ProtectedRef } from '../api/types';

export interface TextCounts {
  lines: number;
  chars: number;
  bytes: number;
}

const encoder = new TextEncoder();

/** Lines as an editor numbers them (a final newline starts no new line),
 * characters as Unicode scalar values (what the server counts), UTF-8 bytes. */
export function countText(text: string): TextCounts {
  if (text === '') return { lines: 0, chars: 0, bytes: 0 };
  const breaks = text.split('\n').length - 1;
  return {
    lines: text.endsWith('\n') ? breaks : breaks + 1,
    chars: Array.from(text).length,
    bytes: encoder.encode(text).length,
  };
}

/** A git-safe slug of the file name (`Press Line.md` → `press-line`); `file`
 * when nothing ASCII is left (a Japanese name). */
export function slugOf(path: string): string {
  const name = path.slice(path.lastIndexOf('/') + 1);
  const stem = name.includes('.') && !name.startsWith('.') ? name.slice(0, name.lastIndexOf('.')) : name;
  const slug = stem
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 40)
    .replace(/-+$/, '');
  return slug === '' ? 'file' : slug;
}

/** `edit/<slug>-<stamp>`: unlikely to exist, readable in the branch list. */
export function branchNameFor(path: string, now: Date): string {
  const pad = (n: number) => String(n).padStart(2, '0');
  const stamp = `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}-${pad(now.getHours())}${pad(now.getMinutes())}${pad(now.getSeconds())}`;
  return `edit/${slugOf(path)}-${stamp}`;
}

/** Matching of a protected ref pattern (forge-core `RefPattern::matches`):
 * case-insensitive, one optional `*` that matches any run. */
export function patternMatches(pattern: string, ref: string): boolean {
  const key = pattern.toLowerCase();
  const name = ref.toLowerCase();
  const star = key.indexOf('*');
  if (star < 0) return name === key;
  const head = key.slice(0, star);
  const tail = key.slice(star + 1);
  return name.length >= head.length + tail.length && name.startsWith(head) && name.endsWith(tail);
}

/** Why a direct commit to `branch` would be refused, as far as the rules
 * say: `forbidden` (direct pushes are banned), `role` (the person's role is
 * below the rule's push role), or `null` (allowed, or not known). */
export function directCommitBlock(
  rules: ProtectedRef[] | undefined,
  branch: string,
  role: string | null | undefined,
  isAdmin: boolean,
): 'forbidden' | 'role' | null {
  if (rules === undefined) return null;
  const ref = branch.startsWith('refs/heads/') ? branch : `refs/heads/${branch}`;
  const matching = rules.filter((r) => patternMatches(r.pattern, ref));
  if (matching.some((r) => r.merge_rule?.direct_push_forbidden === true)) return 'forbidden';
  if (!isAdmin && matching.some((r) => roleRank(role) < roleRank(r.push_min_role))) return 'role';
  return null;
}

/** The path as typed, without surrounding spaces and leading slashes. */
export function cleanPath(raw: string): string {
  return raw.trim().replace(/^\/+/, '');
}

/** What is wrong with a new file's path before sending (the server checks
 * the rest, FG-GIT-0028): `empty`, `folder` (ends in `/`: no file name). */
export function pathProblem(path: string): 'empty' | 'folder' | null {
  if (path === '') return 'empty';
  if (path.endsWith('/')) return 'folder';
  return null;
}
