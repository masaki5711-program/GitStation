// A wiki page's pure parts (plan 20 W4, ADR-0040 §5): the page is a plan 13
// document of type `wiki.page`, but the person never sees its front matter.
// The form reads the four fields it shows (title, classification; id and
// owner are kept as they are) out of the file and writes the file back from
// the form, keeping every other front matter line exactly as written.
//
// Only what the form can carry back unchanged is offered: a front matter
// the reader below does not understand (a known field written over several
// lines, a quoted value that does not read) is `null`, and the screen sends
// the person to the file editor instead of guessing.

/** The doctype of every wiki page (`.forge/doctypes/wiki.page.yml`). */
export const WIKI_DOCTYPE = 'wiki.page';

export const CLASSIFICATIONS = ['public', 'internal', 'confidential'] as const;
export type Classification = (typeof CLASSIFICATIONS)[number];

export interface WikiFront {
  id: string;
  title: string;
  owner: string;
  classification: Classification;
  /** The other front matter lines, as written, in order. */
  extra: string[];
}

export interface WikiPage {
  front: WikiFront;
  /** The Markdown after the front matter (no leading blank line). */
  body: string;
}

const KNOWN = ['doctype', 'id', 'title', 'owner', 'classification'] as const;
type Known = (typeof KNOWN)[number];
const KEY_LINE = /^([A-Za-z_][A-Za-z0-9_.-]*)\s*:(?:\s+(.*))?$/;

/** A one-line YAML scalar as the form shows it, or `null` when it is not one
 * the writer below would give back the same (a block, a flow collection). */
function scalar(raw: string): string | null {
  const value = raw.trim();
  if (value.startsWith('"')) {
    try {
      const parsed: unknown = JSON.parse(value);
      return typeof parsed === 'string' ? parsed : null;
    } catch {
      return null;
    }
  }
  if (value.startsWith("'")) {
    if (value.length < 2 || !value.endsWith("'")) return null;
    const inner = value.slice(1, -1);
    if (/(^|[^'])'([^']|$)/.test(inner)) return null;
    return inner.replace(/''/g, "'");
  }
  if (value === '' || /^[[{|>&*!%@`]/.test(value)) return null;
  // A plain scalar ends before a comment.
  const comment = value.search(/\s#/);
  return (comment < 0 ? value : value.slice(0, comment)).trim();
}

/** The page's form fields and body, or `null` when the form cannot carry it. */
export function parsePage(text: string): WikiPage | null {
  const lines = text.replace(/\r\n/g, '\n').split('\n');
  if (lines[0] !== '---') return null;
  const close = lines.indexOf('---', 1);
  if (close < 0) return null;
  const known: Partial<Record<Known, string>> = {};
  const extra: string[] = [];
  let inExtra = false;
  for (const line of lines.slice(1, close)) {
    const m = KEY_LINE.exec(line);
    if (m === null) {
      // A continuation (indented, a list item) belongs to the line above:
      // fine under a field the form does not touch, not under one it does.
      if (!inExtra && line.trim() !== '' && !line.trimStart().startsWith('#')) return null;
      extra.push(line);
      continue;
    }
    const key = m[1] ?? '';
    if ((KNOWN as readonly string[]).includes(key)) {
      const value = scalar(m[2] ?? '');
      if (value === null || known[key as Known] !== undefined) return null;
      known[key as Known] = value;
      inExtra = false;
    } else {
      extra.push(line);
      inExtra = true;
    }
  }
  const { doctype, id, title, owner, classification } = known;
  if (doctype !== WIKI_DOCTYPE || id === undefined || title === undefined || owner === undefined) {
    return null;
  }
  const cls = classification ?? 'internal';
  if (!(CLASSIFICATIONS as readonly string[]).includes(cls)) return null;
  let body = lines.slice(close + 1).join('\n');
  if (body.startsWith('\n')) body = body.slice(1);
  return {
    front: { id, title, owner, classification: cls as Classification, extra },
    body,
  };
}

/** A YAML double-quoted scalar (a JSON string): a title never changes what
 * the front matter means. */
function quoted(value: string): string {
  // U+2028 / U+2029 end a line in YAML: written as escapes.
  const ls = String.fromCharCode(0x2028);
  const ps = String.fromCharCode(0x2029);
  return JSON.stringify(value).split(ls).join('\\u2028').split(ps).join('\\u2029');
}

/** The file of a page: the front matter from the form, then the body (one
 * blank line between, a final newline). */
export function composePage(page: WikiPage): string {
  const { front } = page;
  const body = page.body.replace(/\r\n/g, '\n');
  return [
    '---',
    `doctype: ${WIKI_DOCTYPE}`,
    `id: ${front.id}`,
    `title: ${quoted(front.title.replace(/[\r\n]+/g, ' ').trim())}`,
    `owner: ${quoted(front.owner)}`,
    `classification: ${front.classification}`,
    ...front.extra,
    '---',
    '',
    body.endsWith('\n') || body === '' ? body : `${body}\n`,
  ].join('\n');
}

const WIKI_ID = /^WIKI-(\d{4,6})$/;

/** The next free id (`WIKI-0001` onwards, Q6) after the ones in use. */
export function nextWikiId(ids: readonly string[]): string {
  let max = 0;
  for (const id of ids) {
    const m = WIKI_ID.exec(id);
    if (m !== null) max = Math.max(max, Number(m[1]));
  }
  return `WIKI-${String(max + 1).padStart(4, '0')}`;
}

/** The namespace that owns a wiki (its path without the last segment): the
 * owner written into a new page, as the first page has it. */
export function ownerOf(wikiPath: string): string {
  const slash = wikiPath.lastIndexOf('/');
  return slash < 0 ? wikiPath : wikiPath.slice(0, slash);
}

/** A folder typed for a new page, cleaned (`/a/b/` → `a/b`), or `null`
 * when it cannot be one (`..`, `.`, a backslash, a control character). */
export function cleanFolder(raw: string): string | null {
  const folder = raw.trim().replace(/^\/+|\/+$/g, '');
  if (folder === '') return '';
  const parts = folder.split('/');
  // eslint-disable-next-line no-control-regex -- a control character is what this refuses
  const bad = /[\u0000-\u001f\u007f\\:]/;
  if (parts.some((p) => p === '' || p === '.' || p === '..' || p.startsWith('.') || bad.test(p))) {
    return null;
  }
  return folder;
}

export interface PageEntry {
  id: string;
  title: string | null;
  path: string;
}

export interface PageTree {
  folders: { name: string; tree: PageTree }[];
  pages: PageEntry[];
}

/** The pages as a folder tree: folders first (by name), then pages (by
 * title, then id). */
export function pageTree(pages: readonly PageEntry[]): PageTree {
  type Node = { folders: Map<string, Node>; pages: PageEntry[] };
  const root: Node = { folders: new Map(), pages: [] };
  for (const page of pages) {
    const parts = page.path.split('/');
    parts.pop();
    let node = root;
    for (const part of parts) {
      let next = node.folders.get(part);
      if (next === undefined) {
        next = { folders: new Map(), pages: [] };
        node.folders.set(part, next);
      }
      node = next;
    }
    node.pages.push(page);
  }
  const collator = new Intl.Collator(undefined, { numeric: true });
  const freeze = (node: Node): PageTree => ({
    folders: [...node.folders.entries()]
      .sort(([a], [b]) => collator.compare(a, b))
      .map(([name, tree]) => ({ name, tree: freeze(tree) })),
    pages: [...node.pages].sort(
      (a, b) => collator.compare(a.title ?? a.id, b.title ?? b.id) || collator.compare(a.id, b.id),
    ),
  });
  return freeze(root);
}
