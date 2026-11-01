// Syntax colouring and diff parsing: pure functions.
import { describe, expect, it } from 'vitest';

import { parsePatch } from '../src/features/DiffView';
import {
  HIGHLIGHT_LIMIT,
  highlightLines,
  languageFor,
  plainLines,
  splitLines,
} from '../src/features/highlight';
import { archiveUrl, blobUrl, compareUrl, encodePath, rawUrl, treeUrl } from '../src/features/repoUrls';

function text(line: Awaited<ReturnType<typeof highlightLines>>, i: number): string {
  const walk = (n: unknown): string => {
    const node = n as { type: string; value?: string; children?: unknown[] };
    if (node.type === 'text') return node.value ?? '';
    return (node.children ?? []).map(walk).join('');
  };
  return (line?.[i] ?? []).map(walk).join('');
}

describe('highlight', () => {
  it('maps file names to grammars', () => {
    expect(languageFor('src/lib.rs')).toBe('rust');
    expect(languageFor('a/B.TS')).toBe('typescript');
    expect(languageFor('Dockerfile')).toBe('dockerfile');
    expect(languageFor('x/Makefile')).toBe('makefile');
    expect(languageFor('Cargo.toml')).toBe('ini');
    expect(languageFor('notes.unknownext')).toBeNull();
    expect(languageFor('LICENSE')).toBeNull();
  });

  it('keeps the text and the line count; a multi-line comment is closed on every line', async () => {
    const source = 'fn a() {}\n/* one\n two\n three */\nlet x = 1;\n';
    const lines = await highlightLines('x.rs', source);
    expect(lines).not.toBeNull();
    expect(lines?.length).toBe(5);
    expect((lines ?? []).map((_, i) => text(lines, i))).toEqual(source.replace(/\n$/, '').split('\n'));
    // Each comment line is its own element with the comment class.
    const comment = (lines?.[2] ?? [])[0] as { tagName: string; properties: { className: string[] } };
    expect(comment.tagName).toBe('span');
    expect(comment.properties.className).toContain('hljs-comment');
  });

  it('colours with classes only (no style properties in the tree)', async () => {
    const lines = await highlightLines('a.json', '{"a": [1, true, "s"]}');
    const seen: unknown[] = [];
    const walk = (n: { type: string; properties?: Record<string, unknown>; children?: unknown[] }) => {
      if (n.type === 'element') seen.push(n.properties);
      for (const c of n.children ?? []) walk(c as never);
    };
    for (const line of lines ?? []) for (const n of line) walk(n);
    expect(seen.length).toBeGreaterThan(0);
    for (const p of seen as Record<string, unknown>[]) {
      expect(Object.keys(p)).toEqual(['className']);
    }
  });

  it('no grammar or too long: no colours (the caller shows plain lines)', async () => {
    expect(await highlightLines('x.unknownext', 'a')).toBeNull();
    expect(await highlightLines('x.rs', 'a'.repeat(HIGHLIGHT_LIMIT + 1))).toBeNull();
  });

  it('splitLines and plainLines agree on trailing newlines and empty lines', () => {
    expect(plainLines('a\nb\n')).toEqual(['a', 'b']);
    expect(plainLines('a\n\nb')).toEqual(['a', '', 'b']);
    expect(plainLines('')).toEqual(['']);
    expect(splitLines([{ type: 'text', value: 'a\n\nb' }]).length).toBe(3);
  });
});

describe('parsePatch', () => {
  it('numbers old and new lines across hunks and keeps the "no newline" note', () => {
    const rows = parsePatch(
      ['@@ -1,2 +1,2 @@', ' keep', '-old', '+new', '\\ No newline at end of file', '@@ -10 +10 @@', '-x', '+y'].join(
        '\n',
      ),
    );
    expect(rows).toEqual([
      { kind: 'hunk', text: '@@ -1,2 +1,2 @@' },
      { kind: 'ctx', oldNo: 1, newNo: 1, text: 'keep' },
      { kind: 'del', oldNo: 2, newNo: null, text: 'old' },
      { kind: 'add', oldNo: null, newNo: 2, text: 'new' },
      { kind: 'note', text: '\\ No newline at end of file' },
      { kind: 'hunk', text: '@@ -10 +10 @@' },
      { kind: 'del', oldNo: 10, newNo: null, text: 'x' },
      { kind: 'add', oldNo: null, newNo: 10, text: 'y' },
    ]);
  });

  it('an empty patch has no rows; markup in a line stays text', () => {
    expect(parsePatch('')).toEqual([]);
    const rows = parsePatch('@@ -1 +1 @@\n+<script>alert(1)</script>');
    expect(rows[1]).toEqual({ kind: 'add', oldNo: null, newNo: 1, text: '<script>alert(1)</script>' });
  });
});

describe('repo URLs', () => {
  it('encode each segment once and keep refs with slashes', () => {
    expect(encodePath('a b/c')).toBe('a%20b/c');
    expect(treeUrl('acme/web', 'feature/x', 'src/設計.md')).toBe(
      `/acme/web/-/tree/feature/x/src/${encodeURIComponent('設計.md')}`,
    );
    expect(treeUrl('acme/web', 'main')).toBe('/acme/web/-/tree/main');
    expect(blobUrl('acme/web', 'main', 'a#b?c.md')).toBe('/acme/web/-/blob/main/a%23b%3Fc.md');
    expect(rawUrl('acme/web', 'v1', 'img/a b.png')).toBe('/acme/web/-/raw/v1/img/a%20b.png');
    expect(archiveUrl('acme/web', 'release/1.0', 'tar.gz')).toBe('/acme/web/-/archive/release/1.0.tar.gz');
    expect(compareUrl('acme/web', 'a', 'b/c')).toBe('/acme/web/-/compare/a...b/c');
    expect(compareUrl('acme/web')).toBe('/acme/web/-/compare');
  });
});
