// The sidebar follows where I am (project / group / My work), and the project
// top page's latest-commit strip and info panel.
import axe from 'axe-core';
import { fireEvent, screen, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { formatRelative } from '../src/features/labels';
import {
  GROUP,
  LISTED_GROUP,
  PROJECT,
  RESOLVE_GROUP,
  SESSION,
  apiError,
  mockServer,
  page,
  signedIn,
  type Handler,
} from './mock';
import { json, renderApp } from './render';
import { C1, repoRoutes } from './repo-fixtures';

afterEach(() => {
  vi.unstubAllGlobals();
});

const projectNav = () => screen.findByRole('navigation', { name: 'プロジェクトのメニュー' });
const groupNav = () => screen.findByRole('navigation', { name: 'グループのメニュー' });

const asRole = (role: string) => ({ 'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role }) });

function currentIn(nav: HTMLElement): string[] {
  return within(nav)
    .getAllByRole('link')
    .filter((l) => l.getAttribute('aria-current') === 'page')
    .map((l) => l.textContent);
}

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help}`);
}

const groupRoutes = (over: Record<string, Handler> = {}): Record<string, Handler> => ({
  ...signedIn(),
  'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_GROUP),
  'GET /api/v1/groups/3': () => json(200, GROUP),
  'GET /api/v1/groups': () =>
    page([
      LISTED_GROUP,
      { ...LISTED_GROUP, id: 4, namespace_id: 40, path: 'acme/line-1', name: 'line-1', display_name: 'Line 1' },
      { ...LISTED_GROUP, id: 5, namespace_id: 50, path: 'acme/line-1/cell', name: 'cell', display_name: 'Cell' },
    ]),
  'GET /api/v1/projects': () =>
    page([PROJECT, { ...PROJECT, id: 8, path: 'other/elsewhere', name: 'elsewhere', display_name: 'Elsewhere' }]),
  ...over,
});

describe('sidebar in a project', () => {
  it('shows the way back to My work, the project and its screens; no settings for a Developer', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    await screen.findByRole('heading', { level: 1, name: 'Forge Web' });
    const nav = await projectNav();
    // No "My work" section at the same time.
    expect(screen.queryByRole('navigation', { name: 'マイワーク' })).toBeNull();
    const links = within(nav).getAllByRole('link');
    expect(links.map((l) => [l.textContent, l.getAttribute('href')])).toEqual([
      ['マイワーク', '/'],
      ['Forge Web', '/acme/forge-web'],
      ['ファイル', '/acme/forge-web/-/tree'],
      ['コミット', '/acme/forge-web/-/commits'],
      ['ブランチ', '/acme/forge-web/-/branches'],
      ['タグ', '/acme/forge-web/-/tags'],
      ['比較', '/acme/forge-web/-/compare'],
      ['MR', '/acme/forge-web/-/pulls'],
      ['文書', '/acme/forge-web/-/documents'],
      ['メンバー', '/acme/forge-web/-/members'],
    ]);
    // The top page is the current one, and only it.
    expect(currentIn(nav)).toEqual(['Forge Web']);
    expect(server.unmatched).toEqual([]);
  });

  it('a Maintainer also gets webhooks and settings; the screen I am on is marked', async () => {
    const server = mockServer(repoRoutes(asRole('maintainer')));
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/commits/main' });
    const nav = await projectNav();
    expect(await within(nav).findByRole('link', { name: '設定' })).toBeTruthy();
    expect(within(nav).getByRole('link', { name: 'Webhook' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/webhooks',
    );
    expect(currentIn(nav)).toEqual(['コミット']);
    // Following a link moves the mark.
    fireEvent.click(within(nav).getByRole('link', { name: 'ブランチ' }));
    await screen.findByRole('heading', { level: 1, name: 'ブランチ' });
    expect(currentIn(await projectNav())).toEqual(['ブランチ']);
  });

  it('an administrator without a role sees settings too', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role: null }),
        'GET /api/v1/session': () => json(200, { ...SESSION, user: { ...SESSION.user, is_admin: true } }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    const nav = await projectNav();
    expect(await within(nav).findByRole('link', { name: '設定' })).toBeTruthy();
  });

  it('"My work" leads back: the sidebar turns to My work on Home', async () => {
    const server = mockServer({
      ...repoRoutes(),
      'GET /api/v1/projects': () => page([PROJECT]),
      'GET /api/v1/groups': () => page([LISTED_GROUP]),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    fireEvent.click(within(await projectNav()).getByRole('link', { name: 'マイワーク' }));
    await screen.findByRole('heading', { level: 1, name: 'ホーム' });
    const nav = screen.getByRole('navigation', { name: 'マイワーク' });
    expect(currentIn(nav)).toEqual(['ホーム']);
    expect(screen.queryByRole('navigation', { name: 'プロジェクトのメニュー' })).toBeNull();
  });

  it('narrow screen: the menu button opens the project section; a link closes it', async () => {
    const mql = {
      matches: true,
      media: '(max-width: 767.98px)',
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
    };
    vi.stubGlobal('matchMedia', () => mql);
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    await screen.findByRole('heading', { level: 1, name: 'Forge Web' });
    const button = screen.getByRole('button', { name: 'メニュー' });
    expect(screen.queryByRole('navigation', { name: 'プロジェクトのメニュー' })).toBeNull();
    fireEvent.click(button);
    const nav = await projectNav();
    expect(nav.id).toBe(button.getAttribute('aria-controls'));
    // The focus goes to the first item.
    expect(document.activeElement).toBe(within(nav).getByRole('link', { name: 'マイワーク' }));
    fireEvent.click(within(nav).getByRole('link', { name: 'コミット' }));
    expect(button.getAttribute('aria-expanded')).toBe('false');
  });

  it('axe: no violations on the project page with its sidebar', async () => {
    const server = mockServer(repoRoutes());
    const { container } = await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    await screen.findByRole('link', { name: 'src/' });
    await screen.findByText('Add the add function');
    expect(await violations(container)).toEqual([]);
  });
});

describe('sidebar in a group', () => {
  it('shows the group, members, settings for its Owner, and what is directly inside', async () => {
    const server = mockServer(groupRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme' });
    await screen.findByRole('heading', { level: 1, name: 'Acme Plant' });
    const nav = await groupNav();
    await within(nav).findByRole('link', { name: 'Line 1' });
    const links = within(nav).getAllByRole('link');
    expect(links.map((l) => [l.textContent, l.getAttribute('href')])).toEqual([
      ['マイワーク', '/'],
      ['Acme Plant', '/acme'],
      ['メンバー', '/acme/-/members'],
      ['設定', '/acme/-/settings'],
      ['Line 1', '/acme/line-1'],
      ['Forge Web', '/acme/forge-web'],
    ]);
    expect(within(nav).getByRole('heading', { name: 'サブグループ' })).toBeTruthy();
    expect(currentIn(nav)).toEqual(['Acme Plant']);
    expect(server.unmatched).toEqual([]);
  });

  it('no settings for a member who is not the Owner; members page is marked', async () => {
    const server = mockServer(
      groupRoutes({
        'GET /api/v1/groups/3': () => json(200, { ...GROUP, role: 'developer' }),
        'GET /api/v1/groups/3/members': () => json(200, { members: [], next_after: null }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/-/members' });
    const nav = await groupNav();
    await within(nav).findByRole('link', { name: 'Line 1' });
    expect(within(nav).queryByRole('link', { name: '設定' })).toBeNull();
    expect(currentIn(nav)).toEqual(['メンバー']);
  });

  it('a failing list says so in the sidebar; the group links stay', async () => {
    const server = mockServer(
      groupRoutes({ 'GET /api/v1/projects': () => apiError(500, '一覧を読めませんでした。', 'req-l-1') }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme' });
    const nav = await groupNav();
    expect(await within(nav).findByText('中のグループとプロジェクトを読めませんでした。')).toBeTruthy();
    expect(within(nav).getByRole('link', { name: 'メンバー' })).toBeTruthy();
  });
});

describe('project top page: latest commit and info panel', () => {
  it('shows the newest commit of the default branch, with its short id linking the commit', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    const strip = within(await screen.findByRole('region', { name: '最新のコミット' }));
    expect(strip.getByText('Add the add function')).toBeTruthy();
    expect(strip.getByText('Taro Yamada')).toBeTruthy();
    const time = strip.getByText((_, el) => el?.tagName === 'TIME');
    expect(time.getAttribute('datetime')).toBe('2026-10-30T09:00:00+09:00');
    const sha = strip.getByRole('link', { name: `コミット ${C1.slice(0, 8)} を開く` });
    expect(sha.getAttribute('href')).toBe(`/acme/forge-web/-/commit/${C1}`);
    // One commit, of the default branch by its full name.
    const asked = server.to('GET', '/api/v1/projects/7/repository/commits')[0];
    expect(asked?.query.get('ref')).toBe('refs/heads/main');
    expect(asked?.query.get('limit')).toBe('1');
  });

  it('the info panel counts branches and tags and links the README and merge requests', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    const info = within(await screen.findByRole('complementary', { name: 'プロジェクト情報' }));
    expect(info.getByText('main')).toBeTruthy();
    expect(info.getByRole('link', { name: '2' }).getAttribute('href')).toBe('/acme/forge-web/-/branches');
    expect(info.getByRole('link', { name: '1' }).getAttribute('href')).toBe('/acme/forge-web/-/tags');
    expect((await info.findByRole('link', { name: 'README' })).getAttribute('href')).toBe(
      '/acme/forge-web/-/blob/main/README.md',
    );
    expect(info.getByRole('link', { name: 'マージリクエスト' }).getAttribute('href')).toBe('/acme/forge-web/-/pulls');
    // The README link reuses the file list's tree request.
    expect(server.to('GET', '/api/v1/projects/7/repository/tree')).toHaveLength(1);
  });

  it('a failing commit call shows its request id there; the rest of the page stays', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/commits': () =>
          apiError(500, '履歴を読めませんでした。', 'req-c-1', 'FG-REPO-0001'),
      }),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('履歴を読めませんでした。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-c-1');
    expect(container.textContent).not.toContain('FG-REPO-0001');
    expect(screen.getByRole('heading', { level: 1, name: 'Forge Web' })).toBeTruthy();
    expect(screen.getByTestId('clone-url')).toBeTruthy();
    expect(await screen.findByRole('link', { name: 'src/' })).toBeTruthy();
    expect(screen.getByRole('complementary', { name: 'プロジェクト情報' })).toBeTruthy();
  });

  it('an empty repository has no commit strip and no README link', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/refs': () =>
          json(200, { default_branch: null, refs: [], truncated: false }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    expect(await screen.findByRole('heading', { name: 'リポジトリはまだ空です' })).toBeTruthy();
    expect(screen.queryByRole('region', { name: '最新のコミット' })).toBeNull();
    const info = within(screen.getByRole('complementary', { name: 'プロジェクト情報' }));
    expect(info.queryByRole('link', { name: 'README' })).toBeNull();
    expect(server.to('GET', '/api/v1/projects/7/repository/commits')).toHaveLength(0);
  });
});

describe('formatRelative', () => {
  const now = Date.parse('2026-11-11T00:00:00Z');
  it('says how long ago in the language given', () => {
    expect(formatRelative('2026-11-08T00:00:00Z', 'en', now)).toBe('3 days ago');
    expect(formatRelative('2026-11-10T22:00:00Z', 'en', now)).toBe('2 hours ago');
    expect(formatRelative('2026-11-10T23:59:30Z', 'en', now)).toBe('30 seconds ago');
    expect(formatRelative('2025-10-01T00:00:00Z', 'ja', now)).toBe('昨年');
  });
  it('an unreadable date is shown as it is', () => {
    expect(formatRelative('not a date', 'en', now)).toBe('not a date');
  });
});
