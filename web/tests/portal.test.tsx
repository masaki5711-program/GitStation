// MR12: the document portal (plan 10 §4.7). A specification is read in
// chapters, in a chosen version, with the differences between two versions
// chapter by chapter, and downloads. The words of the version control system
// never appear on its screens, in any language.
import { cleanup, fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import en from '../src/i18n/en.json';
import id from '../src/i18n/id.json';
import ja from '../src/i18n/ja.json';
import type { Language } from '../src/i18n';
import { apiError, mockServer, page, PROJECT, SPEC } from './mock';
import {
  A,
  B,
  C,
  CHAPTERS,
  COMPARE_B_C,
  REFS,
  RESOLVE_SPEC,
  defaultBlob,
  portalRoutes,
  type PortalOptions,
} from './portal-fixtures';
import { json, renderApp } from './render';

const BASE = '/acme/press-spec/-/docs';

async function open(path: string, options: PortalOptions = {}, lang: Language = 'en') {
  const server = mockServer(portalRoutes(options));
  await renderApp({ fetch: server.fetch, path, lang });
  return server;
}

describe('the list of specifications', () => {
  it('Home lists each specification and opens it in the portal', async () => {
    const server = mockServer({
      ...portalRoutes(),
      'GET /api/v1/projects': (c) => (c.query.get('kind') === 'spec' ? page([SPEC]) : page([PROJECT])),
    });
    await renderApp({ fetch: server.fetch, path: '/', lang: 'en' });
    const link = await screen.findByRole('link', { name: 'Press Spec' });
    expect(link.getAttribute('href')).toBe(BASE);
  });

  it('a specification project page offers the portal', async () => {
    const server = mockServer({
      ...portalRoutes(),
      'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_SPEC),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/press-spec', lang: 'en' });
    const link = await screen.findByRole('link', { name: 'Open as a specification' });
    expect(link.getAttribute('href')).toBe(BASE);
  });

  it('a project that is not a specification has no portal', async () => {
    const server = mockServer({
      ...portalRoutes(),
      'GET /api/v1/projects/8': () => json(200, { ...SPEC, kind: 'system' }),
    });
    await renderApp({ fetch: server.fetch, path: BASE, lang: 'en' });
    expect(await screen.findByRole('heading', { name: 'Page not found' })).toBeTruthy();
  });
});

describe('reading a specification', () => {
  it('opens the latest work in progress: chapters on the left, the first chapter on the right', async () => {
    const server = await open(BASE);
    expect(await screen.findByRole('heading', { level: 1, name: /Press Spec/ })).toBeTruthy();
    const toc = await screen.findByRole('navigation', { name: 'Table of chapters' });
    expect(within(toc).getAllByRole('link').map((a) => a.textContent)).toEqual([
      '01-scope',
      '02-parts',
      '03-notes',
    ]);
    expect(within(toc).getByRole('link', { name: '01-scope' }).getAttribute('aria-current')).toBe('page');
    const body = await screen.findByTestId('markdown');
    expect(within(body).getByRole('heading', { name: 'Scope of the press line' })).toBeTruthy();
    // The latest work in progress is read at the tip of the default branch.
    expect(server.to('GET', '/api/v1/projects/8/repository/blob')[0]?.query.get('ref')).toBe(A);
    expect(server.unmatched).toEqual([]);
  });

  it('the version picker offers the latest and the confirmed versions with date and message', async () => {
    await open(BASE);
    const select = await screen.findByLabelText('Version to show');
    await waitFor(() => {
      expect(within(select).getAllByRole('option').map((o) => o.textContent)).toEqual([
        'Latest (work in progress) — Oct 31, 2026 — Working draft of the next issue',
        'rev-2 — Oct 20, 2026 — Second issue with the parts list',
        'rev-1 — Oct 1, 2026 — First issue for approval',
      ]);
    });
    // Choosing only picks; "Show" goes.
    fireEvent.change(select, { target: { value: 'rev-1' } });
    expect(screen.getByTestId('location').textContent).toBe(BASE);
    fireEvent.click(screen.getByRole('button', { name: 'Show' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe(`${BASE}?version=rev-1`);
    });
    const toc = await screen.findByRole('navigation', { name: 'Table of chapters' });
    expect(within(toc).getAllByRole('link')).toHaveLength(1);
  });

  it('lists the confirmed versions newest first with the newest marked', async () => {
    await open(BASE);
    const table = await screen.findByRole('table');
    await waitFor(() => {
      expect(within(table).getAllByRole('row')[1]?.textContent).toContain('Oct 20, 2026');
    });
    const rows = within(table).getAllByRole('row');
    expect(rows[1]?.textContent).toContain('rev-2');
    expect(rows[1]?.textContent).toContain('Newest confirmed version');
    expect(rows[2]?.textContent).toContain('rev-1');
    // The oldest has no earlier version to compare with.
    expect(within(rows[2] as HTMLElement).queryByRole('link', { name: /Changes from the previous version/ })).toBeNull();
    expect(
      within(rows[1] as HTMLElement).getByRole('link', { name: 'Changes from the previous version (rev-2, compared with rev-1)' }).getAttribute('href'),
    ).toBe(`${BASE}?version=rev-2&compare=rev-1`);
  });

  it('opens a confirmed version, a chosen chapter, and keeps chapter links inside the portal', async () => {
    const server = await open(`${BASE}?version=rev-2`);
    const body = await screen.findByTestId('markdown');
    expect(server.to('GET', '/api/v1/projects/8/repository/blob')[0]?.query.get('ref')).toBe(C);
    const inner = within(body).getByRole('link', { name: 'parts list' });
    expect(inner.getAttribute('href')).toBe(`${BASE}?version=rev-2&chapter=docs%2Fspecs%2F02-parts.md`);
    expect(within(body).getByRole('link', { name: 'outside note' }).getAttribute('href')).toBe('https://example.com/x');
    fireEvent.click(inner);
    expect(await screen.findByRole('heading', { level: 3, name: '02-parts' })).toBeTruthy();
    await waitFor(() => {
      expect(document.activeElement?.id).toBe('portal-chapter');
    });
  });

  it('falls back to the top-level files when there is no docs/specs', async () => {
    await open(BASE, { chapters: {}, topLevel: { [A]: ['overview.md', 'notes.md', 'photo.png'] } });
    const toc = await screen.findByRole('navigation', { name: 'Table of chapters' });
    expect(within(toc).getAllByRole('link').map((a) => a.textContent)).toEqual(['notes', 'overview']);
  });

  it('says so when a version has no chapters', async () => {
    await open(BASE, { chapters: {}, topLevel: { [A]: ['logo.png'] } });
    expect(await screen.findByText('This version has no chapters to read.')).toBeTruthy();
  });

  it('says so when the specification has no content yet', async () => {
    await open(BASE, { refs: { default_branch: null, refs: [], truncated: false } });
    expect(await screen.findByRole('heading', { name: 'This specification has no content yet' })).toBeTruthy();
  });

  it('with no confirmed version only the work in progress can be shown', async () => {
    await open(BASE, { refs: { ...REFS, refs: REFS.refs.filter((r) => r.type === 'branch') } });
    expect(await screen.findByText(/No version has been confirmed yet/)).toBeTruthy();
    const compare = screen.getByLabelText('Version to compare with');
    expect(within(compare).getAllByRole('option')).toHaveLength(1);
  });

  it('an unknown version is said, with a way back to the latest', async () => {
    await open(`${BASE}?version=rev-9`);
    expect(await screen.findByRole('heading', { name: 'That version was not found' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Open the latest (work in progress)' }).getAttribute('href')).toBe(BASE);
  });

  it('a chapter the version lacks is said', async () => {
    await open(`${BASE}?version=rev-1&chapter=docs%2Fspecs%2F02-parts.md`);
    expect(await screen.findByText('This version does not have that chapter.')).toBeTruthy();
  });

  it('says when the chapter list was cut', async () => {
    await open(BASE, { treeTruncated: true });
    expect(await screen.findByText(/too many chapters/)).toBeTruthy();
  });

  it('a chapter that is not text is offered as a download', async () => {
    await open(BASE, {
      blob: (commit, path) => ({ ...defaultBlob(commit, path), binary: true, content: null, size: 4096 }),
    });
    const note = await screen.findByTestId('chapter-binary');
    expect(note.textContent).toContain('cannot be shown as text (4 KiB)');
    expect(within(note).getByRole('link', { name: 'Download this chapter' })).toBeTruthy();
  });

  it('a chapter cut at the server limit says so', async () => {
    await open(BASE, { blob: (commit, path) => ({ ...defaultBlob(commit, path), truncated: true }) });
    expect(await screen.findByTestId('chapter-truncated')).toBeTruthy();
  });

  it('a failed read shows the reason and the request id only', async () => {
    const server = mockServer({
      ...portalRoutes(),
      'GET /api/v1/projects/8/repository/blob': () =>
        apiError(500, 'Something went wrong on the server.', 'req-blob-1', 'FG-GIT-0001'),
    });
    await renderApp({ fetch: server.fetch, path: BASE, lang: 'en' });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('req-blob-1');
    expect(alert.textContent).not.toContain('FG-GIT');
  });
});

describe('differences between two versions', () => {
  const CMP = `${BASE}?version=rev-2&compare=rev-1`;

  it('asks the server for the older and the newer version by their ids', async () => {
    const server = await open(CMP);
    await screen.findByRole('heading', { name: 'Differences by chapter' });
    const call = server.to('GET', '/api/v1/projects/8/repository/compare')[0];
    expect(call?.query.get('from')).toBe(B);
    expect(call?.query.get('to')).toBe(C);
  });

  it('marks each chapter as added or changed, in words, and opens the first changed one', async () => {
    await open(CMP);
    const toc = await screen.findByRole('navigation', { name: 'Table of chapters' });
    const items = within(toc).getAllByRole('listitem').map((li) => li.textContent);
    expect(items).toEqual(['01-scopeChanged', '02-partsAdded']);
    expect(within(toc).getByText(/Chapters with changes: 2/)).toBeTruthy();
    expect(within(toc).getByText(/1 other file\(s\) that are not chapters also changed/)).toBeTruthy();
    expect(within(toc).getByRole('link', { name: '01-scope' }).getAttribute('aria-current')).toBe('page');
  });

  it('shows added and removed lines by a sign and by words, not by colour alone', async () => {
    await open(CMP);
    const lines = await screen.findByRole('region', { name: 'Changed lines' });
    const rows = within(lines).getAllByRole('row');
    const removed = rows.find((r) => r.textContent.includes('30 t'));
    const added = rows.find((r) => r.textContent.includes('40 t'));
    expect(removed?.textContent).toContain('Removed line: ');
    expect(removed?.querySelector('[aria-hidden="true"]')?.textContent).toBe('−');
    expect(added?.textContent).toContain('Added line: ');
    expect(added?.querySelector('[aria-hidden="true"]')?.textContent).toBe('+');
    expect(screen.getByText(/Added lines are marked with \+ and underlined/)).toBeTruthy();
  });

  it('a chapter new in the newer version says so; its lines are all additions', async () => {
    await open(`${CMP}&chapter=docs%2Fspecs%2F02-parts.md`);
    expect(await screen.findByText('This chapter is new in this version.')).toBeTruthy();
    const lines = screen.getByRole('region', { name: 'Changed lines' });
    expect(within(lines).getAllByText('Added line:', { exact: false })).toHaveLength(2);
  });

  it('a chapter only the older version has is removed', async () => {
    const compare = {
      ...COMPARE_B_C,
      diff: {
        truncated: false,
        files: [
          {
            ...COMPARE_B_C.diff.files[0],
            status: 'deleted',
            new_path: null,
            patch: '@@ -1,2 +0,0 @@\n-# Old scope\n-text',
          },
        ],
      },
    };
    // Older has the chapter, the newer does not.
    await open(`${BASE}?version=rev-1&compare=rev-2`, {
      chapters: { ...CHAPTERS, [B]: ['01-scope.md'], [C]: [] },
      compare,
    });
    expect(await screen.findByText('This chapter was removed in this version.')).toBeTruthy();
    const toc = screen.getByRole('navigation', { name: 'Table of chapters' });
    expect(within(toc).getByText('Removed')).toBeTruthy();
  });

  it('a chapter without a difference says so', async () => {
    await open(`${CMP}&chapter=docs%2Fspecs%2F01-scope.md`, {
      compare: { ...COMPARE_B_C, diff: { truncated: false, files: [] } },
    });
    expect(await screen.findByText('There are no differences in this chapter.')).toBeTruthy();
    expect(screen.getByText(/Chapters with changes: 0/)).toBeTruthy();
  });

  it('says when a change cannot be shown as text, is too large, or the comparison was cut', async () => {
    const files = [
      { ...COMPARE_B_C.diff.files[0], binary: true, patch: null },
      { ...COMPARE_B_C.diff.files[1], patch: null, omitted: 'too_large' },
    ];
    await open(CMP, { compare: { ...COMPARE_B_C, diff: { truncated: true, files } } });
    expect(await screen.findByText('The changes in this chapter cannot be shown as text.')).toBeTruthy();
    expect(screen.getByText(/some changes are not shown/)).toBeTruthy();
    fireEvent.click(screen.getByRole('link', { name: '02-parts' }));
    expect(await screen.findByText('The change is too large to show.')).toBeTruthy();
  });

  it('an unknown version to compare with is said and nothing is compared', async () => {
    const server = await open(`${BASE}?version=rev-2&compare=rev-7`);
    expect(await screen.findByText(/version to compare with was not found/)).toBeTruthy();
    expect(server.to('GET', '/api/v1/projects/8/repository/compare')).toHaveLength(0);
  });

  it('"Stop comparing" and "Read this chapter in full" go back to the text', async () => {
    await open(CMP);
    const read = await screen.findByRole('link', { name: 'Read this chapter in full' });
    expect(read.getAttribute('href')).toBe(`${BASE}?version=rev-2&chapter=docs%2Fspecs%2F01-scope.md`);
    expect(screen.getByRole('link', { name: 'Stop comparing' }).getAttribute('href')).toBe(`${BASE}?version=rev-2`);
  });

  it('the picker compares the shown version with another, and never with itself', async () => {
    await open(`${BASE}?version=rev-2`);
    const compare = await screen.findByLabelText('Version to compare with');
    expect(within(compare).getAllByRole('option').map((o) => (o as HTMLOptionElement).value)).toContain('rev-1');
    expect(within(compare).getAllByRole('option').map((o) => (o as HTMLOptionElement).value)).not.toContain('rev-2');
    fireEvent.change(compare, { target: { value: 'rev-1' } });
    fireEvent.click(screen.getByRole('button', { name: 'Show' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe(`${BASE}?version=rev-2&compare=rev-1`);
    });
  });
});

describe('downloads', () => {
  it('one chapter downloads from the server by the version id, and the whole version as a zip', async () => {
    await open(`${BASE}?version=rev-2`);
    const chapter = await screen.findByRole('link', { name: 'Download this chapter' });
    expect(chapter.getAttribute('href')).toBe(`/acme/press-spec/-/raw/${C}/docs/specs/01-scope.md`);
    expect(chapter.hasAttribute('download')).toBe(true);
    const all = screen.getByRole('link', { name: 'Download this whole version (zip)' });
    expect(all.getAttribute('href')).toBe(`/acme/press-spec/-/archive/${C}.zip`);
    // The list offers the zip of every confirmed version.
    const row = screen.getByRole('link', { name: 'Download all (zip): rev-1' });
    expect(row.getAttribute('href')).toBe(`/acme/press-spec/-/archive/${B}.zip`);
  });

  it('a removed chapter has nothing to download', async () => {
    await open(`${BASE}?version=rev-1&compare=rev-2`, {
      chapters: { ...CHAPTERS, [C]: [] },
      compare: {
        ...COMPARE_B_C,
        diff: { truncated: false, files: [{ ...COMPARE_B_C.diff.files[0], status: 'deleted', new_path: null }] },
      },
    });
    await screen.findByText('This chapter was removed in this version.');
    expect(screen.queryByRole('link', { name: 'Download this chapter' })).toBeNull();
  });
});

// The words of the version control system must not reach a reader. The check
// reads both the dictionary of the portal and what is really drawn.
const FORBIDDEN: Record<Language, RegExp[]> = {
  ja: [/ブランチ/, /コミット/, /タグ/, /マージ/, /リポジトリ/, /リビジョン/, /ハッシュ/, /プッシュ/, /クローン/, /差分/, /\bgit\b/i, /\bSHA\b/i, /\bdiff\b/i, /\bref\b/i, /\bHEAD\b/],
  en: [/\bbranch/i, /\bcommit/i, /\btag(s|ged)?\b/i, /\bmerge/i, /\brepositor/i, /\brepo\b/i, /\brevision/i, /\bhash\b/i, /\bclone\b/i, /\bpush/i, /\bpull\b/i, /\bdiff\b/i, /\bgit\b/i, /\bSHA\b/i, /\bref(s)?\b/i, /\bHEAD\b/],
  id: [/\bcabang/i, /\bkomit/i, /\btag\b/i, /\bgabung/i, /\brepositori/i, /\brevisi\b/i, /\bhash\b/i, /\bklon\b/i, /\bdiff\b/i, /\bgit\b/i, /\bSHA\b/i, /\bref\b/i, /\bHEAD\b/, /\bpush\b/i],
};

type Dict = { [key: string]: string | Dict };
function strings(dict: Dict, prefix: string): string[] {
  return Object.entries(dict).flatMap(([k, v]) => {
    const key = `${prefix}.${k}`;
    return typeof v === 'string' ? [`${key}=${v}`] : strings(v, key);
  });
}

/** Everything a reader (or a screen reader) gets from the page's main area,
 * without the specification's own text. */
function readable(main: HTMLElement): string {
  const clone = main.cloneNode(true) as HTMLElement;
  for (const own of clone.querySelectorAll('[data-testid="markdown"]')) own.remove();
  const parts = [clone.textContent];
  for (const el of clone.querySelectorAll('[aria-label],[title],[alt],[placeholder]')) {
    for (const a of ['aria-label', 'title', 'alt', 'placeholder']) {
      const v = el.getAttribute(a);
      if (v !== null) parts.push(v);
    }
  }
  return parts.join('\n');
}

describe('plain words', () => {
  const dictionaries: [Language, Dict][] = [
    ['ja', ja],
    ['en', en],
    ['id', id],
  ];

  it.each(dictionaries)('the portal and plain-word keys in %s hold no version-control word', (lang, dict) => {
    const own = [
      ...strings((dict.portal ?? {}) as Dict, 'portal'),
      ...strings((dict.plain ?? {}) as Dict, 'plain'),
    ];
    expect(own.length).toBeGreaterThan(50);
    const hits = own.filter((line) => FORBIDDEN[lang].some((re) => re.test(line.slice(line.indexOf('=') + 1))));
    expect(hits).toEqual([]);
  });

  it('the plain words are the ones of the glossary in every language', () => {
    expect(ja.plain).toMatchObject({ spec: '仕様書', confirmed: '確定した版', working: '作業中の版' });
    for (const d of [ja, en, id]) {
      expect(Object.keys(d.plain).sort()).toEqual(['confirmed', 'latest', 'spec', 'version', 'working']);
    }
  });

  it.each(['ja', 'en', 'id'] as Language[])('the drawn screens in %s hold no version-control word', async (lang) => {
    const screens: [string, PortalOptions][] = [
      [BASE, {}],
      [`${BASE}?version=rev-2&compare=rev-1`, {}],
      [`${BASE}?version=rev-9`, {}],
      [BASE, { refs: { default_branch: null, refs: [], truncated: false } }],
      [BASE, { blob: (commit: string, path: string) => ({ ...defaultBlob(commit, path), binary: true, content: null }) }],
    ];
    for (const [path, options] of screens) {
      const server = mockServer(portalRoutes(options));
      await renderApp({ fetch: server.fetch, path, lang });
      // Settled: nothing is loading any more.
      await screen.findByRole('main');
      await waitFor(() => {
        const loading = screen.getByRole('main').querySelectorAll('[role="status"]');
        expect([...loading].some((n) => n.className.includes('block'))).toBe(false);
      });
      const text = readable(screen.getByRole('main'));
      // Guard against checking an empty page.
      expect(text).toContain((lang === 'ja' ? ja : lang === 'en' ? en : id).plain.spec);
      if (path === BASE && Object.keys(options).length === 0) expect(text).toContain('01-scope');
      const hits = FORBIDDEN[lang].filter((re) => re.test(text)).map(String);
      expect({ path, hits }).toEqual({ path, hits: [] });
      cleanup();
    }
  });
});
