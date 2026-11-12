// The sidebar follows where I am (project / group / My work), and the project
// top page's latest-commit strip and info panel.
import axe from 'axe-core';
import { act, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { CHILD_CAP } from '../src/components/sidebar/GroupSection';
import { SidebarScopeProvider, useReportSidebarScope, useSidebarScope } from '../src/components/sidebarScope';
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
      ['パイプライン', '/acme/forge-web/-/pipelines'],
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

  it('wide screen: a project screen has no second bar of the same links, only the breadcrumb', async () => {
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/commits' });
    const nav = await projectNav();
    await screen.findByRole('heading', { level: 1 });
    expect(screen.queryByRole('navigation', { name: 'リポジトリの画面' })).toBeNull();
    // Each screen name is a link once: in the sidebar.
    expect(screen.getAllByRole('link', { name: 'ブランチ' })).toEqual([within(nav).getByRole('link', { name: 'ブランチ' })]);
    // The breadcrumb leads back to the group and to the project.
    const crumbs = screen.getByRole('navigation', { name: 'いまの場所' });
    expect(within(crumbs).getAllByRole('link').map((l) => l.getAttribute('href'))).toEqual([
      '/acme',
      '/acme/forge-web',
    ]);
  });

  it('narrow screen: the screens stay one tap away above the page (the sidebar is behind the menu)', async () => {
    const mql = {
      matches: true,
      media: '(max-width: 767.98px)',
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
    };
    vi.stubGlobal('matchMedia', () => mql);
    const server = mockServer(repoRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/commits' });
    const bar = await screen.findByRole('navigation', { name: 'リポジトリの画面' });
    expect(within(bar).getByRole('link', { name: 'コミット' }).getAttribute('aria-current')).toBe('page');
    expect(screen.queryByRole('navigation', { name: 'いまの場所' })).toBeNull();
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

  it('no settings for a member who is not the Owner; members page is marked; no list requests there', async () => {
    const server = mockServer(
      groupRoutes({
        'GET /api/v1/groups/3': () => json(200, { ...GROUP, role: 'developer' }),
        'GET /api/v1/groups/3/members': () => json(200, { members: [], next_after: null }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/-/members' });
    const nav = await groupNav();
    await within(nav).findByRole('link', { name: 'Acme Plant' });
    expect(within(nav).queryByRole('link', { name: '設定' })).toBeNull();
    expect(currentIn(nav)).toEqual(['メンバー']);
    // The sidebar does not read the whole lists for itself: it leads to the group page.
    expect(within(nav).getByRole('link', { name: '中身をすべて見る' }).getAttribute('href')).toBe('/acme');
    expect(server.to('GET', '/api/v1/groups')).toHaveLength(0);
    expect(server.to('GET', '/api/v1/projects')).toHaveLength(0);
  });

  it(`lists at most ${String(CHILD_CAP)} of each, then "see everything" leads to the group page`, async () => {
    const many = Array.from({ length: CHILD_CAP + 2 }, (_, i) => ({
      ...LISTED_GROUP,
      id: 100 + i,
      namespace_id: 1000 + i,
      path: `acme/line-${String(i + 10)}`,
      name: `line-${String(i + 10)}`,
      display_name: `Line ${String(i + 10)}`,
    }));
    const server = mockServer(groupRoutes({ 'GET /api/v1/groups': () => page([LISTED_GROUP, ...many]) }));
    await renderApp({ fetch: server.fetch, path: '/acme' });
    const nav = await groupNav();
    await within(nav).findByRole('link', { name: 'Line 10' });
    const listed = within(nav)
      .getAllByRole('link')
      .filter((l) => /^Line /.test(l.textContent));
    expect(listed).toHaveLength(CHILD_CAP);
    expect(within(nav).queryByRole('link', { name: `Line ${String(CHILD_CAP + 11)}` })).toBeNull();
    expect(within(nav).getByRole('link', { name: '中身をすべて見る' }).getAttribute('href')).toBe('/acme');
    // The group page has them all.
    expect(within(screen.getByRole('main')).getByText(`Line ${String(CHILD_CAP + 11)}`)).toBeTruthy();
  });

  it('a list cut short (truncated) also offers "see everything", even with few children', async () => {
    const server = mockServer(
      groupRoutes({
        // Every page says "more": the list stops at its page cap (truncated).
        'GET /api/v1/groups': (c) =>
          page(
            c.query.get('cursor') === null
              ? [
                  LISTED_GROUP,
                  { ...LISTED_GROUP, id: 4, namespace_id: 40, path: 'acme/line-1', name: 'line-1', display_name: 'Line 1' },
                ]
              : [],
            { has_more: true, next_cursor: 'next' },
          ),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme' });
    const nav = await groupNav();
    await within(nav).findByRole('link', { name: 'Line 1' });
    expect(await within(nav).findByRole('link', { name: '中身をすべて見る' })).toBeTruthy();
  });

  it('few children and complete lists: no "see everything"', async () => {
    const server = mockServer(groupRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme' });
    const nav = await groupNav();
    await within(nav).findByRole('link', { name: 'Line 1' });
    expect(within(nav).queryByRole('link', { name: '中身をすべて見る' })).toBeNull();
  });

  it('a failing list shows its request id and a retry in the sidebar; the group links stay', async () => {
    let fail = true;
    const server = mockServer(
      groupRoutes({
        'GET /api/v1/projects': () =>
          fail ? apiError(500, '一覧を読めませんでした。', 'req-l-1', 'FG-DB-0001') : page([PROJECT]),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme' });
    const nav = await groupNav();
    const inline = within(await within(nav).findByTestId('inline-error'));
    expect(inline.getByText('一覧を読めませんでした。')).toBeTruthy();
    expect(inline.getByTestId('inline-request-id').textContent).toBe('req-l-1');
    expect(nav.textContent).not.toContain('FG-DB-0001');
    expect(within(nav).getByRole('link', { name: 'メンバー' })).toBeTruthy();
    fail = false;
    fireEvent.click(inline.getByRole('button', { name: 'もう一度試す' }));
    expect(await within(nav).findByRole('link', { name: 'Forge Web' })).toBeTruthy();
    expect(within(nav).queryByTestId('inline-error')).toBeNull();
  });
});

describe('sidebar title: loading and failure', () => {
  it('a placeholder while the project loads; no role-only items until the role is known', async () => {
    const server = mockServer(
      repoRoutes({ 'GET /api/v1/projects/7': () => new Promise<Response>(() => undefined) }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/branches' });
    const nav = await projectNav();
    expect(within(nav).getByRole('status').textContent).toBe('読み込んでいます…');
    expect(within(nav).getByRole('link', { name: 'コミット' })).toBeTruthy();
    expect(within(nav).queryByRole('link', { name: '設定' })).toBeNull();
  });

  it('a failed project shows its request id and a retry; retrying brings the name back', async () => {
    let fail = true;
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7': () =>
          fail ? apiError(500, 'プロジェクトを読めませんでした。', 'req-p-9', 'FG-DB-0003') : json(200, PROJECT),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/branches' });
    const nav = await projectNav();
    const inline = within(await within(nav).findByTestId('inline-error'));
    expect(inline.getByText('プロジェクトを読めませんでした。')).toBeTruthy();
    expect(inline.getByTestId('inline-request-id').textContent).toBe('req-p-9');
    expect(nav.textContent).not.toContain('FG-DB-0003');
    // The project's screens are still there (their paths are known).
    expect(within(nav).getByRole('link', { name: 'ブランチ' })).toBeTruthy();
    fail = false;
    fireEvent.click(inline.getByRole('button', { name: 'もう一度試す' }));
    expect(await within(nav).findByRole('link', { name: 'Forge Web' })).toBeTruthy();
  });

  it('collapsed: the failure stays readable (visually hidden) and the retry reachable', async () => {
    window.localStorage.setItem('gitstation.sidebar.collapsed', '1');
    const server = mockServer(
      repoRoutes({ 'GET /api/v1/projects/7': () => apiError(500, 'プロジェクトを読めませんでした。', 'req-p-8') }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/branches' });
    const nav = await projectNav();
    const box = await within(nav).findByTestId('inline-error');
    expect(box.textContent).toContain('プロジェクトを読めませんでした。');
    expect(box.textContent).toContain('req-p-8');
    expect(box.querySelector('p')?.className).toContain('srOnly');
    const retry = within(box).getByRole('button', { name: 'もう一度試す' });
    retry.focus();
    expect(document.activeElement).toBe(retry);
    window.localStorage.clear();
  });

  it('403: no retry (asking again does not change it)', async () => {
    const server = mockServer(
      repoRoutes({ 'GET /api/v1/projects/7': () => apiError(403, '見る権限がありません。', 'req-403') }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/branches' });
    const nav = await projectNav();
    const box = within(await within(nav).findByTestId('inline-error'));
    expect(box.getByTestId('inline-request-id').textContent).toBe('req-403');
    expect(box.queryByRole('button')).toBeNull();
  });
});

describe('sidebar scope', () => {
  it('is cleared when the namespace page is left', () => {
    function Probe({ path }: { path: string }) {
      const found = useSidebarScope(path);
      return <output data-testid="scope">{found === null ? 'none' : found.scope.kind}</output>;
    }
    function Reporter() {
      useReportSidebarScope({ kind: 'project', id: 7, path: 'acme/forge-web' });
      return null;
    }
    const view = render(
      <SidebarScopeProvider>
        <Reporter />
        <Probe path="/acme/forge-web/-/tree" />
      </SidebarScopeProvider>,
    );
    expect(screen.getByTestId('scope').textContent).toBe('project');
    view.rerender(
      <SidebarScopeProvider>
        <Probe path="/acme/forge-web/-/tree" />
      </SidebarScopeProvider>,
    );
    expect(screen.getByTestId('scope').textContent).toBe('none');
  });

  it('a moved path shows the project section after the redirect', async () => {
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/namespaces/resolve': () =>
          json(200, { canonical_path: 'acme/forge-web', kind: 'project', moved: true, namespace_id: 11, project_id: 7 }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/old/forge-web/-/branches' });
    const nav = await projectNav();
    expect(screen.getByTestId('location').textContent).toBe('/acme/forge-web/-/branches');
    expect(currentIn(nav)).toEqual(['ブランチ']);
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

  it('the info panel says it is loading, then shows the refs failure small, with its request id and a retry', async () => {
    let answer: (r: Response) => void = () => undefined;
    const server = mockServer(
      repoRoutes({
        'GET /api/v1/projects/7/repository/refs': () =>
          new Promise<Response>((resolve) => {
            answer = resolve;
          }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    const panel = await screen.findByRole('complementary', { name: 'プロジェクト情報' });
    expect(within(panel).getByText('読み込んでいます…').getAttribute('role')).toBe('status');
    act(() => {
      answer(apiError(500, '参照を読めませんでした。', 'req-refs-2', 'FG-REPO-0016'));
    });
    const box = within(await within(panel).findByTestId('inline-error'));
    expect(box.getByTestId('inline-request-id').textContent).toBe('req-refs-2');
    expect(panel.textContent).not.toContain('FG-REPO-0016');
    expect(box.getByRole('button', { name: 'もう一度試す' })).toBeTruthy();
    // The clone URL is in the panel and stays.
    expect(within(panel).getByTestId('clone-url')).toBeTruthy();
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
