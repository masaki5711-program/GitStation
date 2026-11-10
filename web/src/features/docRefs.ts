// `[[ID]]` / `[[ID#BLOCK]]` / `[[repo:ID]]` in a document's Markdown, as the
// screen draws them (plan 13 §4.3, Q14). The server resolved every reference
// (`refs`); this only rewrites the text the Markdown renderer gets:
//
// - found: a link `ID title` to the target's document screen (the href is a
//   path under `/-/doc-ref/`, which the screen's `linkTo` maps — never a URL
//   taken from the document);
// - hidden (more confidential than this document): the id and a note — the
//   server never sent the title, so none can be shown;
// - broken: the id and a note; external: `repo:ID` as text.
//
// Code fences are left alone. Every piece of text that came from the
// document is escaped so it cannot become Markdown of its own.
import type { DocumentRef } from '../api/types';

export const DOC_REF_PREFIX = '-/doc-ref/';

export interface RefWords {
  hidden: string;
  broken: string;
}

/** Backslash-escape what Markdown (and a table cell) would read. */
export function escapeMarkdown(text: string): string {
  return text.replace(/[\\`*_[\]<>|#!()~]/g, (c) => `\\${c}`);
}

function keyOf(repo: string | null, doc: string, block: string | null): string {
  return `${repo ?? ''}\u0000${doc}\u0000${block ?? ''}`;
}

/** The inner `[repo:]ID[#BLOCK]` split as the server's parser does. */
function parse(inner: string): { repo: string | null; doc: string; block: string | null } {
  const hash = inner.indexOf('#');
  const target = hash < 0 ? inner : inner.slice(0, hash);
  const block = hash < 0 ? null : inner.slice(hash + 1);
  const colon = target.indexOf(':');
  return colon < 0
    ? { repo: null, doc: target, block }
    : { repo: target.slice(0, colon), doc: target.slice(colon + 1), block };
}

function rewrite(inner: string, refs: Map<string, DocumentRef>, words: RefWords): string {
  const p = parse(inner);
  const label = p.block === null ? p.doc : `${p.doc}#${p.block}`;
  const ref = refs.get(keyOf(p.repo, p.doc, p.block));
  if (ref === undefined) return escapeMarkdown(`[[${inner}]]`);
  switch (ref.target) {
    case 'found': {
      const title = ref.title ?? '';
      const href = `/${DOC_REF_PREFIX}${encodeURIComponent(p.doc)}${
        p.block === null ? '' : `/${encodeURIComponent(p.block)}`
      }`;
      const text = title === '' ? label : `${label} ${title}`;
      return `[${escapeMarkdown(text)}](${href})`;
    }
    case 'hidden':
      return escapeMarkdown(`${label} (${words.hidden})`);
    case 'external':
      return escapeMarkdown(`${p.repo ?? ''}:${label}`);
    default:
      return escapeMarkdown(`${label} (${words.broken})`);
  }
}

/** `markdown` with each reference drawn as its resolution says. */
export function linkRefs(markdown: string, refs: DocumentRef[], words: RefWords): string {
  const byKey = new Map(refs.map((r) => [keyOf(r.repo, r.doc, r.block), r]));
  let fence: string | null = null;
  return markdown
    .split('\n')
    .map((line) => {
      const trimmed = line.trimStart();
      if (fence !== null) {
        if (trimmed.startsWith(fence)) fence = null;
        return line;
      }
      const opens = ['```', '~~~'].find((m) => trimmed.startsWith(m));
      if (opens !== undefined) {
        fence = opens;
        return line;
      }
      return line.replace(/\[\[([^\]]+)\]\]/g, (_, inner: string) => rewrite(inner, byKey, words));
    })
    .join('\n');
}

/** The target of a `/-/doc-ref/ID[/BLOCK]` link (as the Markdown renderer
 * resolves it: no leading slash), or `null` for any other path. */
export function docRefTarget(path: string): { doc: string; block?: string } | null {
  if (!path.startsWith(DOC_REF_PREFIX)) return null;
  const [doc, block, ...rest] = path.slice(DOC_REF_PREFIX.length).split('/');
  if (doc === undefined || doc === '' || rest.length > 0) return null;
  return block === undefined ? { doc } : { doc, block };
}
