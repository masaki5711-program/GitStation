// The wiki page's pure parts (plan 20 W4): the form reads and writes the
// front matter so the person never sees YAML. Synthetic data only.
import { describe, expect, it } from 'vitest';

import {
  cleanFolder,
  composePage,
  nextWikiId,
  ownerOf,
  pageTree,
  parsePage,
} from '../src/features/wikiPage';

const SEED = [
  '---',
  'doctype: wiki.page',
  'id: WIKI-0001',
  'title: "Web \\"Wiki\\""',
  'owner: "acme"',
  'classification: internal',
  '---',
  '',
  '## About',
  '',
].join('\n');

describe('wiki page front matter', () => {
  it('reads the seed and writes it back unchanged', () => {
    const page = parsePage(SEED);
    expect(page?.front).toEqual({
      id: 'WIKI-0001',
      title: 'Web "Wiki"',
      owner: 'acme',
      classification: 'internal',
      extra: [],
    });
    expect(page?.body).toBe('## About\n');
    expect(page === null ? null : composePage(page)).toBe(SEED);
  });

  it('keeps other fields (with their continuation lines) as written', () => {
    const text = SEED.replace('---\n\n', 'tags:\n  - a\n  - b\n---\n\n');
    const page = parsePage(text);
    expect(page?.front.extra).toEqual(['tags:', '  - a', '  - b']);
    expect(page === null ? null : composePage(page)).toBe(text);
  });

  it('reads plain and single-quoted values; a comment is not part of one', () => {
    const text = SEED.replace('title: "Web \\"Wiki\\""', "title: 'It''s here'").replace(
      'owner: "acme"',
      'owner: acme # the group',
    );
    expect(parsePage(text)?.front.title).toBe("It's here");
    expect(parsePage(text)?.front.owner).toBe('acme');
  });

  it('refuses what the form cannot give back the same', () => {
    expect(parsePage('# no front matter\n')).toBeNull();
    expect(parsePage(SEED.replace('doctype: wiki.page', 'doctype: spec'))).toBeNull();
    expect(parsePage(SEED.replace('title: "Web \\"Wiki\\""', 'title: |\n  long'))).toBeNull();
    expect(parsePage(SEED.replace('title: "Web \\"Wiki\\""', 'title: "broken'))).toBeNull();
    expect(parsePage(SEED.replace('classification: internal', 'classification: secret'))).toBeNull();
    expect(parsePage(SEED.replace('id: WIKI-0001', 'id: WIKI-0001\nid: WIKI-0002'))).toBeNull();
  });

  it('writes a title on one line, quoted, line separators escaped', () => {
    const page = parsePage(SEED);
    if (page === null) throw new Error('seed');
    const ls = String.fromCharCode(0x2028);
    const out = composePage({ ...page, front: { ...page.front, title: `a\nb${ls}c: #d` } });
    expect(out).toContain('title: "a b\\u2028c: #d"\n');
  });
});

describe('wiki ids, folders and the tree', () => {
  it('numbers the next page after the highest id', () => {
    expect(nextWikiId([])).toBe('WIKI-0001');
    expect(nextWikiId(['WIKI-0001', 'WIKI-0012', 'SPEC-0099'])).toBe('WIKI-0013');
    expect(nextWikiId(['WIKI-9999'])).toBe('WIKI-10000');
  });

  it('owner is the wiki namespace', () => {
    expect(ownerOf('acme/plant/web-wiki')).toBe('acme/plant');
  });

  it('cleans a folder and refuses unsafe ones', () => {
    expect(cleanFolder(' /手順/設備/ ')).toBe('手順/設備');
    expect(cleanFolder('')).toBe('');
    expect(cleanFolder('a/../b')).toBeNull();
    expect(cleanFolder('.forge')).toBeNull();
    expect(cleanFolder('a\\b')).toBeNull();
    expect(cleanFolder('a//b')).toBeNull();
  });

  it('builds folders first, pages by title', () => {
    const tree = pageTree([
      { id: 'WIKI-0003', title: 'b', path: 'WIKI-0003.md' },
      { id: 'WIKI-0002', title: 'x', path: 'z/WIKI-0002.md' },
      { id: 'WIKI-0001', title: 'a', path: 'WIKI-0001.md' },
      { id: 'WIKI-0004', title: null, path: 'a/b/WIKI-0004.md' },
    ]);
    expect(tree.folders.map((f) => f.name)).toEqual(['a', 'z']);
    expect(tree.pages.map((p) => p.id)).toEqual(['WIKI-0001', 'WIKI-0003']);
    expect(tree.folders[0]?.tree.folders[0]?.tree.pages[0]?.id).toBe('WIKI-0004');
  });
});
