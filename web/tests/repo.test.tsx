// The repository screens: tree, file, commits, commit, compare, blame,
// branches and tags, with their truncated / binary / large / error states.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { REFS_FILLED, apiError, mockServer } from './mock';
import {
  BLAME,
  C1,
  C2,
  COMMIT_1,
  COMMIT_2,
  COMMIT_DETAIL,
  COMPARE,
  DIFF,
  LIB_RS,
  MAIN,
  TREE_ROOT,
  blob,
  repoRoutes,
} from './repo-fixtures';
import { json, renderApp } from './render';

const BASE = '/acme/forge-web/-';

describe('tree', () => {
  it('lists folders first, links files and folders, marks a submodule, and shows the README', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree`, lang: 'en' });
    expect(await screen.findByRole('heading', { level: 1, name: 'Files' })).toBeTruthy();
    const table = await screen.findByRole('table', { name: 'Contents of the folder' });
    const rows = within(table).getAllByRole('row').slice(1);
    expect(rows.map((r) => within(r).getAllByRole('rowheader')[0]?.textContent)).toEqual([
      'src/',
      'vendor Submodule',
      'README.md',
      'data.bin',
      'logo.png',
    ]);
    expect(within(table).getByRole('link', { name: 'src/' }).getAttribute('href')).toBe(
      `${BASE}/tree/main/src`,
    );
    expect(within(table).getByRole('link', { name: 'README.md' }).getAttribute('href')).toBe(
      `${BASE}/blob/main/README.md`,
    );
    expect(within(table).queryByRole('link', { name: 'vendor' })).toBeNull();
    // The README is drawn under the list, below the page's own headings.
    expect(await screen.findByRole('heading', { level: 3, name: 'Press Line Spec' })).toBeTruthy();
    // Default branch: the tree was asked for the full ref name.
    const call = server.to('GET', '/api/v1/projects/7/repository/tree')[0];
    expect(call?.query.get('ref')).toBe('refs/heads/main');
    expect(server.unmatched).toEqual([]);
  });

  it('a sub-folder has a parent link and path crumbs; the ref and path come from resolve', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree/main/src`, lang: 'en' });
    expect(await screen.findByRole('heading', { level: 1, name: 'src' })).toBeTruthy();
    const resolve = server.to('GET', '/api/v1/projects/7/repository/resolve')[0];
    expect(resolve?.query.get('spec')).toBe('main/src');
    const tree = server.to('GET', '/api/v1/projects/7/repository/tree')[0];
    expect(tree?.query.get('path')).toBe('src');
    expect(screen.getByRole('link', { name: 'Up one folder' }).getAttribute('href')).toBe(
      `${BASE}/tree/main`,
    );
    const crumbs = screen.getByRole('navigation', { name: 'File location' });
    expect(within(crumbs).getByRole('link', { name: 'main' }).getAttribute('href')).toBe(`${BASE}/tree/main`);
    expect(within(crumbs).getByText('src').getAttribute('aria-current')).toBe('page');
  });

  it('offers archive downloads of the ref and the clone URL', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree/main`, lang: 'en' });
    expect(await screen.findByRole('link', { name: 'Download zip' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Download zip' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/archive/main.zip',
    );
    expect(screen.getByRole('link', { name: 'Download tar.gz' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/archive/main.tar.gz',
    );
    expect(screen.getByText(`${window.location.origin}/acme/forge-web.git`)).toBeTruthy();
  });

  it('the ref switcher goes to the same screen on another ref', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree/main`, lang: 'en' });
    const select = await screen.findByLabelText('Branch or tag');
    fireEvent.change(select, { target: { value: 'v1' } });
    // Choosing does not navigate (arrow keys in a select would): the button does.
    expect(screen.getByTestId('location').textContent).toBe(`${BASE}/tree/main`);
    fireEvent.click(screen.getByRole('button', { name: 'Go' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe(`${BASE}/tree/v1`);
    });
  });

  it('says so when the server cut the listing', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/tree': () => json(200, { ...TREE_ROOT, truncated: true }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree/main`, lang: 'en' });
    const note = await screen.findByTestId('tree-truncated');
    expect(note.textContent).toContain('too many entries');
    expect(note.textContent).toContain('5');
  });

  it('an empty folder says it is empty', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/tree': () => json(200, { ...TREE_ROOT, entries: [] }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree/main`, lang: 'en' });
    expect(await screen.findByText('This folder is empty.')).toBeTruthy();
  });

  it('an unknown path is a 404 with the server message and request id, not a crash', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree/main/nope` });
    // Tree for "nope" is answered 404 by the fixture.
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('指定された場所が見つかりません。');
    expect(alert.textContent).toContain('req-404');
    expect(alert.textContent).not.toContain('FG-REPO');
  });

  it('an unknown ref is a 404 with the server message', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree/nobranch/x`, lang: 'en' });
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByRole('heading', { name: 'Not found' })).toBeTruthy();
    expect(alert.textContent).toContain('req-404');
  });

  it('a server error offers a retry', async () => {
    let fail = true;
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/tree': () =>
          fail ? apiError(500, 'サーバーで問題が起きました。', 'req-500') : json(200, TREE_ROOT),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree/main` });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('req-500');
    fail = false;
    fireEvent.click(within(alert).getByRole('button', { name: 'もう一度試す' }));
    expect(await screen.findByRole('table')).toBeTruthy();
  });

  it('a repository with no branch says it is empty', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/refs': () =>
          json(200, { default_branch: null, refs: [], truncated: false }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree`, lang: 'en' });
    expect(await screen.findByRole('heading', { name: 'The repository is empty' })).toBeTruthy();
  });

  it('the project page shows the files of the default branch and the screen links', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web', lang: 'en' });
    expect(await screen.findByRole('link', { name: 'src/' })).toBeTruthy();
    const nav = screen.getByRole('navigation', { name: 'Repository screens' });
    expect(within(nav).getByRole('link', { name: 'Commits' }).getAttribute('href')).toBe(`${BASE}/commits`);
  });
});

describe('file', () => {
  it('shows text with numbered lines and colours it (classes, no style attributes)', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/src/lib.rs`, lang: 'en' });
    expect(await screen.findByRole('heading', { level: 1, name: 'lib.rs' })).toBeTruthy();
    const code = await screen.findByTestId('code');
    expect(code.getAttribute('data-lines')).toBe('4');
    await waitFor(() => {
      expect(code.querySelector('.hljs-keyword')).not.toBeNull();
    });
    expect(code.querySelector('.hljs-comment')?.textContent).toBe('// sum');
    expect(code.querySelectorAll('[style]')).toHaveLength(0);
    expect(code.querySelector('#L3 td')?.textContent).toBe('    a + b');
    expect(code.querySelector('#L3 th a')?.getAttribute('aria-label')).toBe('Line 3');
    expect(code.querySelector('#L3 th a')?.getAttribute('href')).toBe('#L3');
    // The text is intact after colouring.
    expect([...code.querySelectorAll('td')].map((td) => td.textContent).join('\n')).toBe(
      LIB_RS.replace(/\n$/, ''),
    );
    const calls = server.to('GET', '/api/v1/projects/7/repository/blob');
    expect(calls[0]?.query.get('ref')).toBe('refs/heads/main');
    expect(calls[0]?.query.get('path')).toBe('src/lib.rs');
  });

  it('links raw, download, blame and history', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/src/lib.rs`, lang: 'en' });
    await screen.findByTestId('code');
    expect(screen.getByRole('link', { name: 'Raw' }).getAttribute('href')).toBe(
      `/acme/forge-web/-/raw/${C1}/src/lib.rs`,
    );
    expect(screen.getByRole('link', { name: 'Download' }).getAttribute('download')).toBe('lib.rs');
    expect(screen.getByRole('link', { name: 'Blame' }).getAttribute('href')).toBe(
      `${BASE}/blame/main/src/lib.rs`,
    );
    expect(screen.getByRole('link', { name: 'History' }).getAttribute('href')).toBe(
      `${BASE}/commits/main/src/lib.rs`,
    );
  });

  it('draws a Markdown file, and switches to its source and back', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/README.md`, lang: 'en' });
    expect(await screen.findByRole('heading', { level: 2, name: 'Press Line Spec' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'guide' }).getAttribute('href')).toBe(
      `${BASE}/blob/main/docs/guide.md`,
    );
    expect(screen.getByRole('link', { name: 'outside link' }).getAttribute('rel')).toBe('noopener noreferrer');
    expect(screen.getByAltText('logo').getAttribute('src')).toBe(`/acme/forge-web/-/raw/${C1}/logo.png`);
    fireEvent.click(screen.getByRole('button', { name: 'Source' }));
    expect((await screen.findByTestId('code')).textContent).toContain('# Press Line Spec');
    expect(screen.queryByTestId('markdown')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Rendered' }));
    expect(await screen.findByTestId('markdown')).toBeTruthy();
  });

  it('a Markdown file with an XSS payload shows no active content on the screen', async () => {
    const evil = '<script>window.pwned=1</script>\n\n[x](javascript:alert(1))\n\n![i](https://evil.example/t.png)';
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/blob': () => json(200, blob('README.md', evil)),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/README.md`, lang: 'en' });
    const md = await screen.findByTestId('markdown');
    expect(md.querySelector('script')).toBeNull();
    expect(md.querySelector('img')).toBeNull();
    expect(md.innerHTML).not.toMatch(/javascript:/i);
    expect(document.querySelector('script:not([src])')).toBeNull();
  });

  it('a file cut at 1 MiB says so and keeps the link to the whole file', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/blob': () =>
          json(200, blob('src/lib.rs', 'a\nb\n', { truncated: true, size: 5 * 1024 * 1024 })),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/src/lib.rs`, lang: 'en' });
    const note = await screen.findByTestId('blob-truncated');
    expect(note.textContent).toContain('first 1 MiB');
    expect(note.textContent).toContain('5 MiB');
    expect(screen.getByRole('link', { name: 'Raw' })).toBeTruthy();
  });

  it('a binary file is not shown and offers the download', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/data.bin`, lang: 'en' });
    const note = await screen.findByTestId('binary');
    expect(note.textContent).toContain('not text');
    expect(screen.queryByTestId('code')).toBeNull();
    expect(screen.queryByRole('link', { name: 'Blame' })).toBeNull();
    expect(screen.getByRole('link', { name: 'Download' })).toBeTruthy();
  });

  it('a png is shown through the raw route', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/logo.png`, lang: 'en' });
    const img = await screen.findByAltText('logo.png');
    expect(img.getAttribute('src')).toBe(`/acme/forge-web/-/raw/${C1}/logo.png`);
    expect(screen.queryByTestId('binary')).toBeNull();
  });

  it('svg / html / pdf are never drawn: a notice, the source only as text', async () => {
    const svg = '<svg onload="alert(1)"><script>alert(1)</script></svg>';
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/blob': () => json(200, blob('a/pic.svg', svg)),
      }),
    );
    const view = await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/a/pic.svg`, lang: 'en' });
    const note = await screen.findByTestId('download-only');
    expect(note.textContent).toContain('download');
    const code = await screen.findByTestId('code');
    expect(code.textContent).toContain('<svg onload="alert(1)">');
    expect(view.container.querySelector('svg')).toBeNull();
    expect(view.container.querySelector('script')).toBeNull();
    expect(view.container.querySelector('main img')).toBeNull();
  });

  it('a symlink shows its target', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/blob': () =>
          json(200, blob('link', '../target', { type: 'symlink', mode: '120000' })),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/link`, lang: 'en' });
    expect((await screen.findByTestId('symlink')).textContent).toContain('../target');
  });

  it('an empty file says so', async () => {
    const server = mockServer(
      repoRoutes({ 'GET /api/v1/projects/7/repository/blob': () => json(200, blob('e.txt', '')) }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/e.txt`, lang: 'en' });
    expect(await screen.findByText('This file is empty.')).toBeTruthy();
  });

  it('a very large text file (20 000 lines) is shown without colours and stays usable', async () => {
    const big = Array.from({ length: 20_000 }, (_, i) => `line ${i}`).join('\n');
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/blob': () =>
          json(200, blob('big.rs', big, { truncated: false })),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/big.rs`, lang: 'en' });
    const code = await screen.findByTestId('code');
    expect(code.getAttribute('data-lines')).toBe('20000');
    expect(code.querySelector('.hljs-keyword')).toBeNull();
    // Only a window is in the page; "Show more" widens it.
    expect(code.querySelectorAll('tr')).toHaveLength(3000);
    fireEvent.click(screen.getByRole('button', { name: /Show more \(17000 lines left\)/ }));
    await waitFor(() => {
      expect(code.querySelectorAll('tr')).toHaveLength(6000);
    });
  });

  it('a path that is a folder goes to the tree screen', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main`, lang: 'en' });
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe(`${BASE}/tree/main`);
    });
  });

  it('a 404 file is the not-found notice', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/missing.rs`, lang: 'en' });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('req-404');
  });
});

describe('commits', () => {
  it('lists commits with author and date, and links to each', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/commits`, lang: 'en' });
    const list = await screen.findByTestId('commit-list');
    const items = within(list).getAllByRole('listitem');
    expect(items).toHaveLength(2);
    expect(within(items[0] as HTMLElement).getByRole('link', { name: 'Add the add function' }).getAttribute('href')).toBe(
      `${BASE}/commit/${C1}`,
    );
    expect(items[0]?.textContent).toContain('Taro Yamada');
    expect(items[0]?.textContent).toContain('aaaaaaaa');
    expect(screen.getByText('That is all of them.')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Load more' })).toBeNull();
  });

  it('pages by keyset cursor: "Load more" asks with the cursor and appends', async () => {
    const pageOne = {
      ref: MAIN,
      head: C1,
      path: '',
      commits: [COMMIT_1],
      next_cursor: 'v1.cursor-one',
      has_more: true,
    };
    const pageTwo = { ...pageOne, commits: [COMMIT_2], next_cursor: null, has_more: false };
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/commits': (c) =>
          json(200, c.query.get('cursor') === null ? pageOne : pageTwo),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commits`, lang: 'en' });
    await screen.findByTestId('commit-list');
    expect(screen.queryByText('Initial commit')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Load more' }));
    expect(await screen.findByText('Initial commit')).toBeTruthy();
    expect(screen.getByText('Add the add function')).toBeTruthy();
    const calls = server.to('GET', '/api/v1/projects/7/repository/commits');
    expect(calls.map((c) => c.query.get('cursor'))).toEqual([null, 'v1.cursor-one']);
    expect(screen.queryByRole('button', { name: 'Load more' })).toBeNull();
    expect(screen.getByText('That is all of them.')).toBeTruthy();
  });

  it('a failing next page keeps the list and offers a retry', async () => {
    const pageOne = { ref: MAIN, head: C1, path: '', commits: [COMMIT_1], next_cursor: 'c1', has_more: true };
    let fail = true;
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/commits': (c) =>
          c.query.get('cursor') === null
            ? json(200, pageOne)
            : fail
              ? apiError(500, 'サーバーで問題が起きました。', 'req-page2')
              : json(200, { ...pageOne, commits: [COMMIT_2], next_cursor: null, has_more: false }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commits`, lang: 'en' });
    await screen.findByTestId('commit-list');
    fireEvent.click(screen.getByRole('button', { name: 'Load more' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('req-page2');
    expect(screen.getByText('Add the add function')).toBeTruthy();
    fail = false;
    fireEvent.click(within(alert).getByRole('button', { name: 'Try again' }));
    expect(await screen.findByText('Initial commit')).toBeTruthy();
  });

  it('a path is a history of one file, with a way back to all commits', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/commits/main/src/lib.rs`, lang: 'en' });
    await screen.findByTestId('commit-list');
    expect(server.to('GET', '/api/v1/projects/7/repository/commits')[0]?.query.get('path')).toBe('src/lib.rs');
    expect(screen.getByRole('link', { name: 'Show all commits' }).getAttribute('href')).toBe(
      `${BASE}/commits/main`,
    );
  });

  it('a merge commit is marked', async () => {
    const merge = { ...COMMIT_1, parents: [C2, 'c'.repeat(40)] };
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/commits': () =>
          json(200, { ref: MAIN, head: C1, path: '', commits: [merge], next_cursor: null, has_more: false }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commits`, lang: 'en' });
    expect(await screen.findByText('Merge')).toBeTruthy();
  });

  it('no commits says so', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/commits': () =>
          json(200, { ref: MAIN, head: C1, path: 'x', commits: [], next_cursor: null, has_more: false }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commits/main/x`, lang: 'en' });
    expect(await screen.findByText('There are no commits.')).toBeTruthy();
  });
});

describe('commit and diff', () => {
  it('shows the message, facts, parents and the changed files', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/commit/${C1}`, lang: 'en' });
    expect(await screen.findByRole('heading', { level: 1, name: 'Add the add function' })).toBeTruthy();
    expect(screen.getByText(/Synthetic body line/)).toBeTruthy();
    expect(screen.getByRole('link', { name: 'bbbbbbbb' }).getAttribute('href')).toBe(`${BASE}/commit/${C2}`);
    expect(screen.getByText(C1)).toBeTruthy();
    const diff = screen.getByTestId('diff');
    expect(within(diff).getByText('Changed files: 3')).toBeTruthy();
    // The patch: two additions, one deletion, signs as text for assistive tech.
    const table = within(diff).getByRole('table');
    const rows = within(table).getAllByRole('row');
    expect(rows.some((r) => r.textContent.includes('a + b') && r.textContent.includes('added'))).toBe(true);
    expect(rows.some((r) => r.textContent.includes('a - b') && r.textContent.includes('removed'))).toBe(true);
    expect(within(diff).getByRole('link', { name: 'src/lib.rs' }).getAttribute('href')).toBe(
      `${BASE}/blob/${C1}/src/lib.rs`,
    );
  });

  it('says why a file has no patch: binary and too large', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/commit/${C1}`, lang: 'en' });
    const notes = await screen.findAllByTestId('diff-note');
    const texts = notes.map((n) => n.textContent);
    expect(texts.some((t) => t.includes('binary file changed'))).toBe(true);
    expect(texts.some((t) => t.includes('too large'))).toBe(true);
  });

  it.each([
    ['total_cap', 'as a whole grew too large'],
    ['not_utf8', 'not valid text'],
    ['something_new', 'The diff is not shown.'],
  ])('omitted=%s is explained', async (omitted, words) => {
    const detail = {
      ...COMMIT_DETAIL,
      diff: { truncated: false, files: [{ ...DIFF.files[0], patch: null, omitted }] },
    };
    const server = mockServer(
      repoRoutes({ [`GET /api/v1/projects/7/repository/commits/${C1}`]: () => json(200, detail) }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commit/${C1}`, lang: 'en' });
    expect((await screen.findByTestId('diff-note')).textContent).toContain(words);
  });

  it('a diff the server cut says so', async () => {
    const detail = { ...COMMIT_DETAIL, diff: { ...DIFF, truncated: true } };
    const server = mockServer(
      repoRoutes({ [`GET /api/v1/projects/7/repository/commits/${C1}`]: () => json(200, detail) }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commit/${C1}`, lang: 'en' });
    expect((await screen.findByTestId('diff-truncated')).textContent).toContain('only part of the diff');
  });

  it('a long patch is folded until asked for', async () => {
    const lines = ['@@ -1,700 +1,700 @@', ...Array.from({ length: 700 }, (_, i) => ` row ${i}`)];
    const detail = {
      ...COMMIT_DETAIL,
      diff: { truncated: false, files: [{ ...DIFF.files[0], patch: lines.join('\n') }] },
    };
    const server = mockServer(
      repoRoutes({ [`GET /api/v1/projects/7/repository/commits/${C1}`]: () => json(200, detail) }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commit/${C1}`, lang: 'en' });
    const show = await screen.findByRole('button', { name: /Show the diff \(701 lines\)/ });
    expect(screen.queryByText('row 5')).toBeNull();
    fireEvent.click(show);
    expect(await screen.findByText('row 5')).toBeTruthy();
  });

  it('renames and mode-only changes are described', async () => {
    const detail = {
      ...COMMIT_DETAIL,
      diff: {
        truncated: false,
        files: [
          { ...DIFF.files[0], status: 'renamed', old_path: 'a.txt', new_path: 'b.txt', similarity: 100, patch: '' },
          { ...DIFF.files[0], old_path: 'run.sh', new_path: 'run.sh', patch: '' },
        ],
      },
    };
    const server = mockServer(
      repoRoutes({ [`GET /api/v1/projects/7/repository/commits/${C1}`]: () => json(200, detail) }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commit/${C1}`, lang: 'en' });
    const notes = await screen.findAllByTestId('diff-note');
    expect(notes.map((n) => n.textContent)).toEqual([
      'Only the name changed; the content is the same.',
      'Only the mode changed; the content is the same.',
    ]);
    expect(screen.getByText('a.txt → b.txt')).toBeTruthy();
  });

  it('a root commit has no parents', async () => {
    const detail = { ...COMMIT_DETAIL, commit: { ...COMMIT_1, parents: [] } };
    const server = mockServer(
      repoRoutes({ [`GET /api/v1/projects/7/repository/commits/${C1}`]: () => json(200, detail) }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commit/${C1}`, lang: 'en' });
    expect(await screen.findByText('(root commit)')).toBeTruthy();
  });

  it('a malformed commit id is not found, with no request', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/commit/not-an-oid`, lang: 'en' });
    expect(await screen.findByRole('heading', { name: 'Page not found' })).toBeTruthy();
    expect(server.calls.filter((c) => c.path.includes('/commits/'))).toHaveLength(0);
  });
});

describe('compare', () => {
  it('shows the commits only the target has, and the diff', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/compare/main...dev`, lang: 'en' });
    expect(await screen.findByRole('heading', { name: /Commits only in the compared revision \(1\)/ })).toBeTruthy();
    expect(screen.getByText('Add the add function')).toBeTruthy();
    const call = server.to('GET', '/api/v1/projects/7/repository/compare')[0];
    expect(call?.query.get('from')).toBe('main');
    expect(call?.query.get('to')).toBe('dev');
    expect(screen.getByTestId('diff')).toBeTruthy();
  });

  it('a ref with a slash is kept whole', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/compare/release/1.0...feature/x`, lang: 'en' });
    await screen.findByTestId('diff');
    const call = server.to('GET', '/api/v1/projects/7/repository/compare')[0];
    expect(call?.query.get('from')).toBe('release/1.0');
    expect(call?.query.get('to')).toBe('feature/x');
  });

  it('the form goes to the compare address', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/compare`, lang: 'en' });
    expect(await screen.findByText('Choose the two to compare.')).toBeTruthy();
    fireEvent.change(screen.getByLabelText('Base'), { target: { value: 'main' } });
    fireEvent.change(screen.getByLabelText('Compare', { selector: 'input' }), { target: { value: 'dev' } });
    fireEvent.click(screen.getByRole('button', { name: 'Compare' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe(`${BASE}/compare/main...dev`);
    });
  });

  it('says when the server listed only the newest commits', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/compare': () => json(200, { ...COMPARE, commits_truncated: true }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/compare/main...dev`, lang: 'en' });
    expect((await screen.findByTestId('compare-truncated')).textContent).toContain('only the newest 1');
  });

  it('an unknown ref shows the server message', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/compare': () =>
          apiError(404, '指定された場所が見つかりません。', 'req-cmp', 'FG-REPO-0023'),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/compare/main...zzz` });
    expect((await screen.findByRole('alert')).textContent).toContain('req-cmp');
  });

  it('no difference says so', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/compare': () =>
          json(200, { ...COMPARE, commits: [], diff: { files: [], truncated: false } }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/compare/main...dev`, lang: 'en' });
    expect(await screen.findByText('No commits exist only in the compared revision.')).toBeTruthy();
  });
});

describe('blame', () => {
  it('shows each hunk once with its commit, and the lines numbered', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/blame/main/src/lib.rs`, lang: 'en' });
    expect(await screen.findByRole('heading', { level: 1, name: /lib\.rs/ })).toBeTruthy();
    const table = await screen.findByRole('table');
    const rows = within(table).getAllByRole('row').slice(1);
    expect(rows).toHaveLength(4);
    expect(within(table).getByRole('link', { name: 'aaaaaaaa' }).getAttribute('href')).toBe(`${BASE}/commit/${C1}`);
    expect(within(table).getAllByRole('link', { name: 'bbbbbbbb' })).toHaveLength(2);
    expect(rows[1]?.textContent).toContain('2');
    expect(rows[1]?.textContent).toContain('// sum');
    expect(screen.getByRole('link', { name: 'Back to the file' }).getAttribute('href')).toBe(
      `${BASE}/blob/main/src/lib.rs`,
    );
    await waitFor(() => {
      expect(table.querySelector('.hljs-keyword')).not.toBeNull();
    });
  });

  it('a binary file and a truncated blame are said in words', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/blame': () =>
          json(200, { ...BLAME, binary: true, truncated: false, hunks: [], commits: [] }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/blame/main/data.bin`, lang: 'en' });
    expect(await screen.findByTestId('blame-binary')).toBeTruthy();
    expect(screen.queryByRole('table')).toBeNull();

    const server2 = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/blame': () => json(200, { ...BLAME, truncated: true }),
      }),
    );
    await renderApp({ fetch: server2.fetch, path: `${BASE}/blame/main/src/lib.rs`, lang: 'en' });
    expect((await screen.findByTestId('blame-truncated')).textContent).toContain('first 4');
  });
});

describe('branches and tags', () => {
  it('lists branches with the default marked, commit links and archive links', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/branches`, lang: 'en' });
    const table = await screen.findByRole('table');
    expect(within(table).getByText('Default')).toBeTruthy();
    expect(within(table).getByRole('link', { name: 'main' }).getAttribute('href')).toBe(`${BASE}/tree/main`);
    const zips = within(table).getAllByRole('link', { name: /^zip/ });
    expect(zips.map((a) => a.getAttribute('href'))).toEqual([
      '/acme/forge-web/-/archive/main.zip',
      '/acme/forge-web/-/archive/dev.zip',
    ]);
    expect(within(table).queryByText('v1')).toBeNull();
  });

  it('lists tags; a tag that is not a commit has no links', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/refs': () =>
          json(200, {
            ...REFS_FILLED,
            refs: [
              ...REFS_FILLED.refs,
              { name: 'tree-tag', full_name: 'refs/tags/tree-tag', type: 'tag', target: 'd'.repeat(40), commit: null },
            ],
          }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/tags`, lang: 'en' });
    const table = await screen.findByRole('table');
    expect(within(table).getByRole('link', { name: 'v1' })).toBeTruthy();
    expect(within(table).queryByRole('link', { name: 'tree-tag' })).toBeNull();
    expect(within(table).getByText('Not a commit')).toBeTruthy();
  });

  it('says so when the ref list is cut, and when there is none', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/refs': () =>
          json(200, { default_branch: 'main', truncated: true, refs: [] }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/tags`, lang: 'en' });
    expect(await screen.findByTestId('refs-truncated')).toBeTruthy();
    expect(screen.getByText('There are no tags.')).toBeTruthy();
  });
});

describe('words and addresses', () => {
  it('settings and members are screens, any other word is 404', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/settings`, lang: 'en' });
    expect(await screen.findByRole('heading', { level: 1, name: 'Settings' })).toBeTruthy();
    const other = mockServer(repoRoutes());
    await renderApp({ fetch: other.fetch, path: `${BASE}/pull`, lang: 'en' });
    expect(await screen.findByRole('heading', { name: 'Page not found' })).toBeTruthy();
  });

  it('refs with spaces and non-ASCII characters in paths are encoded once', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/tree': () =>
          json(200, {
            ...TREE_ROOT,
            entries: [
              { name: '設計 メモ.md', path: '設計 メモ.md', type: 'blob', mode: '100644', oid: 'e'.repeat(40), size: 3 },
            ],
          }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree/main`, lang: 'en' });
    const link = await screen.findByRole('link', { name: '設計 メモ.md' });
    expect(link.getAttribute('href')).toBe(`${BASE}/blob/main/${encodeURIComponent('設計 メモ.md')}`);
  });

  it('opening the screens never adds a <style> element', async () => {
    const before = document.querySelectorAll('style').length;
    for (const path of [`${BASE}/tree/main`, `${BASE}/blob/main/src/lib.rs`, `${BASE}/commit/${C1}`]) {
      const server = mockServer(repoRoutes());
      const view = await renderApp({ fetch: server.fetch, path });
      await screen.findAllByRole('heading', { level: 1 });
      view.unmount();
    }
    expect(document.querySelectorAll('style').length).toBe(before);
  });
});

describe('review fixes (MR10)', () => {
  it('a new text with the same path and length never shows the old colours', async () => {
    const { useHighlighted } = await import('../src/features/CodeView');
    const { renderHook, waitFor: wait } = await import('@testing-library/react');
    const view = renderHook(({ text }) => useHighlighted('x.rs', text), {
      initialProps: { text: 'fn aa() {}' },
    });
    await wait(() => {
      expect(view.result.current).not.toBeNull();
    });
    // Same path, same length, different content: the old lines must not be returned.
    view.rerender({ text: 'xx bb() {}' });
    expect(view.result.current).toBeNull();
    await wait(() => {
      expect(view.result.current).not.toBeNull();
    });
    const first = (view.result.current ?? [])[0] ?? [];
    expect(JSON.stringify(first)).toContain('xx ');
    expect(JSON.stringify(first)).not.toContain('"fn"');
  });

  it('a failed grammar load leaves the current text as plain lines and is logged', async () => {
    const { vi } = await import('vitest');
    const hl = await import('../src/features/highlight');
    const spy = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const boom = vi.spyOn(hl, 'highlightLines').mockRejectedValue(new Error('chunk failed'));
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/src/lib.rs`, lang: 'en' });
    const code = await screen.findByTestId('code');
    await waitFor(() => {
      expect(spy).toHaveBeenCalled();
    });
    expect(code.querySelector('#L3 td')?.textContent).toBe('    a + b');
    expect(code.querySelector('.hljs-keyword')).toBeNull();
    boom.mockRestore();
    spy.mockRestore();
  });

  it('raw, image and archive addresses name one thing when a branch and a tag share a name', async () => {
    const refs = {
      default_branch: 'main',
      truncated: false,
      refs: [
        ...REFS_FILLED.refs,
        { name: 'dev', full_name: 'refs/tags/dev', type: 'tag', target: C2, commit: C2 },
      ],
    };
    const server = mockServer(
      repoRoutes({ 'GET /api/v1/projects/7/repository/refs': () => json(200, refs) }),
    );
    const first = await renderApp({ fetch: server.fetch, path: `${BASE}/tree/dev`, lang: 'en' });
    expect((await screen.findByRole('link', { name: 'Download zip' })).getAttribute('href')).toBe(
      `/acme/forge-web/-/archive/${C2}.zip`,
    );
    first.unmount();
    const other = mockServer(repoRoutes());
    await renderApp({ fetch: other.fetch, path: `${BASE}/tree/dev`, lang: 'en' });
    expect((await screen.findByRole('link', { name: 'Download zip' })).getAttribute('href')).toBe(
      '/acme/forge-web/-/archive/dev.zip',
    );
  });

  it('a diff opens at most a total budget of rows; the rest is folded, with aria-expanded and focus on reveal', async () => {
    const lines = ['@@ -1,500 +1,500 @@', ...Array.from({ length: 499 }, (_, i) => ` row ${i}`)];
    const files = Array.from({ length: 6 }, (_, i) => ({
      ...DIFF.files[0],
      old_path: `f${i}.rs`,
      new_path: `f${i}.rs`,
      patch: lines.join('\n'),
    }));
    const server = mockServer(
      repoRoutes({
        [`GET /api/v1/projects/7/repository/commits/${C1}`]: () =>
          json(200, { ...COMMIT_DETAIL, diff: { truncated: false, files } }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commit/${C1}`, lang: 'en' });
    const folded = await screen.findAllByRole('button', { name: /Show the diff/ });
    // 2000 rows / 500 per file: four open, two folded.
    expect(folded).toHaveLength(2);
    expect(folded[0]?.getAttribute('aria-expanded')).toBe('false');
    expect(folded[0]?.getAttribute('aria-controls')).toBeTruthy();
    fireEvent.click(folded[0] as HTMLElement);
    await waitFor(() => {
      expect(document.activeElement?.getAttribute('role')).toBe('region');
    });
  });

  it('blame shows a window of lines and "Show more" widens it', async () => {
    const many = Array.from({ length: 1200 }, (_, i) => `l${i}`);
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/blame': () =>
          json(200, { ...BLAME, hunks: [{ commit: C1, start: 1, orig_start: 1, lines: many }] }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/blame/main/src/lib.rs`, lang: 'en' });
    const table = await screen.findByRole('table');
    expect(within(table).getAllByRole('row')).toHaveLength(501);
    expect(screen.getAllByRole('status').some((s) => s.textContent.includes('500 of 1200'))).toBe(true);
    fireEvent.click(screen.getByRole('button', { name: /Show more \(700 lines left\)/ }));
    await waitFor(() => {
      expect(within(table).getAllByRole('row')).toHaveLength(1001);
    });
  });

  it('after "Load more" the focus is on the first new commit and a live region says how many', async () => {
    const pageOne = { ref: MAIN, head: C1, path: '', commits: [COMMIT_1], next_cursor: 'c1', has_more: true };
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/commits': (c) =>
          json(
            200,
            c.query.get('cursor') === null
              ? pageOne
              : { ...pageOne, commits: [COMMIT_2], next_cursor: null, has_more: false },
          ),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/commits`, lang: 'en' });
    await screen.findByTestId('commit-list');
    fireEvent.click(screen.getByRole('button', { name: 'Load more' }));
    const second = await screen.findByRole('link', { name: 'Initial commit' });
    await waitFor(() => {
      expect(document.activeElement).toBe(second);
    });
    expect(screen.getAllByRole('status').some((s) => s.textContent === 'Loaded 1 more commits.')).toBe(true);
  });
});
