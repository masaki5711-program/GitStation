// Path segments as ADR-0015 accepts them at creation: 1..64 characters,
// `^[a-z0-9](?:[a-z0-9._-]{0,62}[a-z0-9])?$`. The server decides (reserved
// words, taken names, tombstones); this only spares a round trip for the
// obvious mistakes and suggests a path from the display name.
const SEGMENT = /^[a-z0-9](?:[a-z0-9._-]{0,62}[a-z0-9])?$/;

export function isValidSegment(value: string): boolean {
  return SEGMENT.test(value);
}

/** `Forge Web 2` → `forge-web-2`; names with no ASCII letters give ''. */
export function suggestSegment(displayName: string): string {
  return displayName
    .normalize('NFKD')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 64)
    .replace(/-+$/, '');
}

/** The path `/a/b` of a URL pathname → segments, each decoded once. A
 * segment that does not decode is `null` (the page is "not found"). */
export function pathSegments(pathname: string): string[] | null {
  const out: string[] = [];
  for (const raw of pathname.split('/')) {
    if (raw === '') continue;
    try {
      out.push(decodeURIComponent(raw));
    } catch {
      return null;
    }
  }
  return out;
}

/** The GitHub-shaped words (plan 10 §4.3) the URL rescue moves behind `/-/`. */
export const VIEW_WORDS = [
  'tree',
  'blob',
  'blame',
  'commits',
  'commit',
  'compare',
  'branches',
  'tags',
  'settings',
  'members',
] as const;

export function isViewWord(word: string | undefined): boolean {
  return word !== undefined && (VIEW_WORDS as readonly string[]).includes(word);
}
