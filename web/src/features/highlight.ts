// Syntax colouring (ADR-0022 §3): highlight.js through lowlight, which returns
// a syntax tree (no HTML string, no innerHTML). The result is `<span
// class="hljs-…">`: classes only, never style attributes (CSP). Languages are
// separate chunks, loaded when a file of that language is opened. No WASM, no
// eval.
import type { Element, ElementContent } from 'hast';
import type { LanguageFn } from 'highlight.js';
import { createLowlight } from 'lowlight';

import { baseName, extensionOf } from './repoUrls';

type Loader = () => Promise<{ default: LanguageFn }>;

const LOADERS: Record<string, Loader> = {
  bash: () => import('highlight.js/lib/languages/bash'),
  c: () => import('highlight.js/lib/languages/c'),
  cpp: () => import('highlight.js/lib/languages/cpp'),
  csharp: () => import('highlight.js/lib/languages/csharp'),
  css: () => import('highlight.js/lib/languages/css'),
  diff: () => import('highlight.js/lib/languages/diff'),
  dockerfile: () => import('highlight.js/lib/languages/dockerfile'),
  go: () => import('highlight.js/lib/languages/go'),
  ini: () => import('highlight.js/lib/languages/ini'),
  java: () => import('highlight.js/lib/languages/java'),
  javascript: () => import('highlight.js/lib/languages/javascript'),
  json: () => import('highlight.js/lib/languages/json'),
  kotlin: () => import('highlight.js/lib/languages/kotlin'),
  makefile: () => import('highlight.js/lib/languages/makefile'),
  markdown: () => import('highlight.js/lib/languages/markdown'),
  php: () => import('highlight.js/lib/languages/php'),
  python: () => import('highlight.js/lib/languages/python'),
  ruby: () => import('highlight.js/lib/languages/ruby'),
  rust: () => import('highlight.js/lib/languages/rust'),
  sql: () => import('highlight.js/lib/languages/sql'),
  typescript: () => import('highlight.js/lib/languages/typescript'),
  xml: () => import('highlight.js/lib/languages/xml'),
  yaml: () => import('highlight.js/lib/languages/yaml'),
};

const BY_EXTENSION: Record<string, string> = {
  sh: 'bash', bash: 'bash', zsh: 'bash',
  c: 'c', h: 'c',
  cc: 'cpp', cpp: 'cpp', cxx: 'cpp', hpp: 'cpp', hh: 'cpp',
  cs: 'csharp',
  css: 'css',
  diff: 'diff', patch: 'diff',
  go: 'go',
  ini: 'ini', toml: 'ini', cfg: 'ini', conf: 'ini', properties: 'ini',
  java: 'java',
  js: 'javascript', mjs: 'javascript', cjs: 'javascript', jsx: 'javascript',
  json: 'json', jsonc: 'json',
  kt: 'kotlin', kts: 'kotlin',
  md: 'markdown', markdown: 'markdown',
  php: 'php',
  py: 'python',
  rb: 'ruby',
  rs: 'rust',
  sql: 'sql',
  ts: 'typescript', tsx: 'typescript',
  html: 'xml', htm: 'xml', xml: 'xml', svg: 'xml', xsl: 'xml',
  yml: 'yaml', yaml: 'yaml',
};

const BY_NAME: Record<string, string> = {
  dockerfile: 'dockerfile',
  makefile: 'makefile',
  gnumakefile: 'makefile',
};

/** Above this many characters a file is shown without colours (the work is
 * done in the page, on the main thread). */
export const HIGHLIGHT_LIMIT = 200_000;

const lowlight = createLowlight();

export function languageFor(path: string): string | null {
  const byName = BY_NAME[baseName(path).toLowerCase()];
  if (byName !== undefined) return byName;
  const byExt = BY_EXTENSION[extensionOf(path)];
  return byExt ?? null;
}

/** A line is a list of tree nodes. */
export type Line = ElementContent[];

/** `\n` splits text nodes; an element that spans lines is closed and
 * re-opened on each (so every line is self-contained). */
function splitNode(node: ElementContent): Line[] {
  if (node.type === 'text') {
    return node.value.split('\n').map((value) => [{ type: 'text', value }]);
  }
  if (node.type !== 'element') return [[]];
  const lines: Line[] = [[]];
  for (const child of node.children) {
    const parts = splitNode(child);
    const last = lines[lines.length - 1];
    const first = parts[0];
    if (last !== undefined && first !== undefined) last.push(...first);
    lines.push(...parts.slice(1));
  }
  return lines.map((children) => {
    const copy: Element = { ...node, children };
    return children.length === 0 ? [] : [copy];
  });
}

export function splitLines(nodes: ElementContent[]): Line[] {
  const lines: Line[] = [[]];
  for (const node of nodes) {
    const parts = splitNode(node);
    const last = lines[lines.length - 1];
    const first = parts[0];
    if (last !== undefined && first !== undefined) last.push(...first);
    lines.push(...parts.slice(1));
  }
  return lines;
}

function lineText(line: Line): string {
  return line
    .map((node) => (node.type === 'text' ? node.value : node.type === 'element' ? lineText(node.children) : ''))
    .join('');
}

/** Plain lines: `a\nb\n` is two lines. */
export function plainLines(text: string): string[] {
  const lines = text.split('\n');
  if (lines.length > 1 && lines[lines.length - 1] === '') lines.pop();
  return lines;
}

/** Colour a file's text by language; `null` when there is no grammar for it
 * (or the text is too long): the caller shows plain lines. */
export async function highlightLines(path: string, text: string): Promise<Line[] | null> {
  if (text.length > HIGHLIGHT_LIMIT) return null;
  const language = languageFor(path);
  if (language === null) return null;
  const load = LOADERS[language];
  if (load === undefined) return null;
  if (!lowlight.registered(language)) {
    const module = await load();
    lowlight.register({ [language]: module.default });
  }
  const tree = lowlight.highlight(language, text);
  const lines = splitLines(tree.children as ElementContent[]);
  // `a\nb\n` is two lines: drop the empty one after the final newline.
  const last = lines[lines.length - 1];
  if (lines.length > 1 && last !== undefined && lineText(last) === '') lines.pop();
  return lines;
}
