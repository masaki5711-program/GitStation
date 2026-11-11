// Plan 15 MR 1: the "My work" sidebar and its two new pages.
import axe from 'axe-core';
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { BrowserRouter } from 'react-router';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { createApiClient } from '../src/api/client';
import { AppProviders, AppRoutes, createQueryClient } from '../src/App';
import { createI18n } from '../src/i18n';
import { LISTED_GROUP, PROJECT, SESSION, apiError, mockServer, page, signedIn } from './mock';
import { json, renderApp } from './render';

const routes = (over: Record<string, () => Response> = {}) => ({
  ...signedIn(),
  'GET /api/v1/projects': () => page([PROJECT]),
  'GET /api/v1/groups': () => page([LISTED_GROUP]),
  ...over,
});

const nav = () => screen.getByRole('navigation', { name: 'マイワーク' });

/** A matchMedia that says "narrow" (< 768px), with a switch for change events. */
function stubNarrow(matches: boolean) {
  const listeners = new Set<() => void>();
  const mql = {
    get matches() {
      return matches;
    },
    media: '(max-width: 767.98px)',
    addEventListener: (_: string, l: () => void) => listeners.add(l),
    removeEventListener: (_: string, l: () => void) => listeners.delete(l),
  };
  vi.stubGlobal('matchMedia', () => mql);
  return {
    set(next: boolean) {
      matches = next;
      for (const l of listeners) l();
    },
  };
}

beforeEach(() => {
  window.localStorage.clear();
});
afterEach(() => {
  vi.unstubAllGlobals();
});

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help}`);
}

describe('sidebar', () => {
  it('lists Home, Projects, Groups, Merge requests, Activity under "My work" (no placeholder items)', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム', level: 1 });
    expect(within(nav()).getByRole('heading', { name: 'マイワーク' })).toBeTruthy();
    const links = within(nav()).getAllByRole('link');
    expect(links.map((l) => [l.textContent, l.getAttribute('href')])).toEqual([
      ['ホーム', '/'],
      ['プロジェクト', '/-/projects'],
      ['グループ', '/-/groups'],
      ['マージリクエスト', '/-/merge-requests'],
      ['アクティビティ', '/-/activity'],
    ]);
  });

  it('marks the current page with aria-current, and only it', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/-/groups' });
    await screen.findByRole('heading', { name: 'グループ', level: 1 });
    const current = within(nav())
      .getAllByRole('link')
      .filter((l) => l.getAttribute('aria-current') === 'page');
    expect(current.map((l) => l.textContent)).toEqual(['グループ']);
    // Moving on moves the mark.
    fireEvent.click(within(nav()).getByRole('link', { name: 'プロジェクト' }));
    await screen.findByRole('heading', { name: 'プロジェクト', level: 1 });
    expect(within(nav()).getByRole('link', { name: 'プロジェクト' }).getAttribute('aria-current')).toBe('page');
    expect(within(nav()).getByRole('link', { name: 'グループ' }).getAttribute('aria-current')).toBeNull();
  });

  it('collapses to icons with the names kept for screen readers, and remembers it', async () => {
    const server = mockServer(routes());
    const first = await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム', level: 1 });
    fireEvent.click(within(nav()).getByRole('button', { name: 'サイドバーを畳む' }));
    expect(window.localStorage.getItem('gitstation.sidebar.collapsed')).toBe('1');
    // The accessible names stay; the label text is visually hidden.
    const home = within(nav()).getByRole('link', { name: 'ホーム' });
    expect(home.querySelector('span')?.className).toContain('srOnly');
    expect(within(nav()).getByRole('button', { name: 'サイドバーを広げる' })).toBeTruthy();
    first.unmount();

    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム', level: 1 });
    expect(within(nav()).getByRole('button', { name: 'サイドバーを広げる' })).toBeTruthy();
    fireEvent.click(within(nav()).getByRole('button', { name: 'サイドバーを広げる' }));
    expect(window.localStorage.getItem('gitstation.sidebar.collapsed')).toBe('0');
    expect(within(nav()).getByRole('link', { name: 'ホーム' }).querySelector('span')?.className).toContain(
      'label',
    );
  });

  it('works when storage throws (expanded, still collapses for this view)', async () => {
    const throwing = {
      getItem: () => {
        throw new Error('blocked');
      },
      setItem: () => {
        throw new Error('blocked');
      },
    };
    vi.stubGlobal('localStorage', throwing);
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム', level: 1 });
    fireEvent.click(within(nav()).getByRole('button', { name: 'サイドバーを畳む' }));
    expect(within(nav()).getByRole('button', { name: 'サイドバーを広げる' })).toBeTruthy();
  });

  it('keyboard only: every item is a focusable link or button, in reading order', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム', level: 1 });
    const focusable = [...nav().querySelectorAll<HTMLElement>('a[href], button')];
    expect(focusable.map((e) => e.textContent)).toEqual(['サイドバーを畳む', 'ホーム', 'プロジェクト', 'グループ', 'マージリクエスト', 'アクティビティ']);
    for (const el of focusable) {
      expect(el.tabIndex).toBe(0);
      el.focus();
      expect(document.activeElement).toBe(el);
    }
    // Enter on a focused link follows it (native link behaviour; click is what it fires).
    const groups = within(nav()).getByRole('link', { name: 'グループ' });
    groups.focus();
    fireEvent.click(groups);
    expect(await screen.findByRole('heading', { name: 'グループ', level: 1 })).toBeTruthy();
    expect(screen.getByTestId('location').textContent).toBe('/-/groups');
  });

  it('no sidebar before sign-in, and none in the bare act-as popup', async () => {
    const server = mockServer({ 'GET /api/v1/session': () => apiError(401, 'x', 'r') });
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ログインしてください' });
    expect(screen.queryByRole('navigation', { name: 'マイワーク' })).toBeNull();
  });

  it('axe: no violations on Home with the sidebar, expanded and collapsed', async () => {
    const server = mockServer(routes());
    const { container } = await renderApp({ fetch: server.fetch });
    await screen.findByText('承認を頼まれた MR はここに出ます。');
    expect(await violations(container)).toEqual([]);
    fireEvent.click(within(nav()).getByRole('button', { name: 'サイドバーを畳む' }));
    expect(await violations(container)).toEqual([]);
  });
});

describe('sidebar on a narrow screen', () => {
  it('is closed; the header menu button opens it (aria-expanded), Esc closes and returns focus', async () => {
    stubNarrow(true);
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム', level: 1 });
    const button = screen.getByRole('button', { name: 'メニュー' });
    expect(button.getAttribute('aria-expanded')).toBe('false');
    expect(screen.queryByRole('navigation', { name: 'マイワーク' })).toBeNull();
    const controls = button.getAttribute('aria-controls');
    expect(controls).not.toBeNull();

    fireEvent.click(button);
    expect(button.getAttribute('aria-expanded')).toBe('true');
    expect(nav().id).toBe(controls);
    // Opening moves the focus to the first item.
    expect(document.activeElement).toBe(within(nav()).getByRole('link', { name: 'ホーム' }));
    // No collapse button on a narrow screen: the names always show.
    expect(within(nav()).queryByRole('button')).toBeNull();

    within(nav()).getByRole('link', { name: 'プロジェクト' }).focus();
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(button.getAttribute('aria-expanded')).toBe('false');
    expect(screen.queryByRole('navigation', { name: 'マイワーク' })).toBeNull();
    expect(document.activeElement).toBe(button);
  });

  it('Esc is left alone when already handled or when the focus is elsewhere', async () => {
    stubNarrow(true);
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム', level: 1 });
    const button = screen.getByRole('button', { name: 'メニュー' });
    fireEvent.click(button);
    // Something else (a dialog) handled the key first.
    const handled = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true });
    handled.preventDefault();
    act(() => {
      document.dispatchEvent(handled);
    });
    expect(button.getAttribute('aria-expanded')).toBe('true');
    // The focus is outside the sidebar and its button (e.g. in the page).
    const card = await screen.findByRole('link', { name: /承認を待っている MR/ });
    card.focus();
    fireEvent.keyDown(card, { key: 'Escape' });
    expect(button.getAttribute('aria-expanded')).toBe('true');
    // On the button itself, Esc closes.
    button.focus();
    fireEvent.keyDown(button, { key: 'Escape' });
    expect(button.getAttribute('aria-expanded')).toBe('false');
  });

  it('the link of the page I am on closes the panel and gives the focus to the menu button', async () => {
    stubNarrow(true);
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム', level: 1 });
    const button = screen.getByRole('button', { name: 'メニュー' });
    fireEvent.click(button);
    fireEvent.click(within(nav()).getByRole('link', { name: 'ホーム' }));
    expect(button.getAttribute('aria-expanded')).toBe('false');
    expect(document.activeElement).toBe(button);
  });

  it('following a link closes the panel', async () => {
    stubNarrow(true);
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム', level: 1 });
    const button = screen.getByRole('button', { name: 'メニュー' });
    fireEvent.click(button);
    fireEvent.click(within(nav()).getByRole('link', { name: 'グループ' }));
    expect(await screen.findByRole('heading', { name: 'グループ', level: 1 })).toBeTruthy();
    expect(button.getAttribute('aria-expanded')).toBe('false');
  });

  it('a wide screen has no menu button; turning narrow brings it', async () => {
    const media = stubNarrow(false);
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム', level: 1 });
    expect(screen.queryByRole('button', { name: 'メニュー' })).toBeNull();
    expect(nav()).toBeTruthy();
    act(() => {
      media.set(true);
    });
    expect(screen.getByRole('button', { name: 'メニュー' })).toBeTruthy();
    expect(screen.queryByRole('navigation', { name: 'マイワーク' })).toBeNull();
  });
});

describe('Home with the sidebar pages', () => {
  it('links to /-/projects, where the projects I take part in moved (plan 15 MR 7)', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch });
    expect((await screen.findByRole('link', { name: 'プロジェクトをすべて見る' })).getAttribute('href')).toBe(
      '/-/projects',
    );
    // Home no longer reads the project or group lists.
    expect(server.to('GET', '/api/v1/projects')).toHaveLength(0);
    expect(server.to('GET', '/api/v1/groups')).toHaveLength(0);
  });
});

describe('/-/projects', () => {
  // Plan 15 MR 5: two views as tabs — "created by me" (default,
  // created_by=me) and "member" (membership=true, `?tab=member`).
  const tabs = () => screen.getByRole('navigation', { name: 'プロジェクトの表示' });
  const projectQueries = (server: ReturnType<typeof mockServer>) =>
    server.to('GET', '/api/v1/projects').map((r) => [r.query.get('created_by'), r.query.get('membership')]);

  it('opens on "created by me" (created_by=me only) with the filter', async () => {
    const other = { ...PROJECT, id: 9, path: 'acme/press-line', display_name: 'Press Line' };
    const server = mockServer(routes({ 'GET /api/v1/projects': () => page([PROJECT, other]) }));
    await renderApp({ fetch: server.fetch, path: '/-/projects' });
    expect(await screen.findByRole('heading', { name: 'プロジェクト', level: 1 })).toBeTruthy();
    await screen.findByText('Press Line');
    const created = within(tabs()).getByRole('link', { name: '自分が作成' });
    expect(created.getAttribute('aria-current')).toBe('page');
    expect(created.getAttribute('href')).toBe('/-/projects');
    const member = within(tabs()).getByRole('link', { name: '参加中' });
    expect(member.getAttribute('aria-current')).toBeNull();
    expect(member.getAttribute('href')).toBe('/-/projects?tab=member');
    // Only `me`, never a user id, and not mixed with the membership filter.
    expect(projectQueries(server)).toEqual([['me', null]]);
    expect(screen.getByText(/作った人の記録がない古いプロジェクト/)).toBeTruthy();
    fireEvent.change(screen.getByLabelText('名前やパスで絞り込む'), { target: { value: 'press' } });
    expect(screen.queryByText('Forge Web')).toBeNull();
    expect(document.title).toBe('プロジェクト · GitStation');
    expect(server.unmatched).toEqual([]);
  });

  it('the "member" tab lists the projects I am a member of (membership=true)', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/-/projects' });
    await screen.findByText('Forge Web');
    fireEvent.click(within(tabs()).getByRole('link', { name: '参加中' }));
    await vi.waitFor(() => {
      expect(projectQueries(server)).toEqual([
        ['me', null],
        [null, 'true'],
      ]);
    });
    expect(within(tabs()).getByRole('link', { name: '参加中' }).getAttribute('aria-current')).toBe('page');
    expect(within(tabs()).getByRole('link', { name: '自分が作成' }).getAttribute('aria-current')).toBeNull();
    expect(screen.queryByText(/作った人の記録がない古いプロジェクト/)).toBeNull();
    expect(await screen.findByText('Forge Web')).toBeTruthy();
    expect(document.title).toBe('参加中 · プロジェクト · GitStation');
    expect(server.unmatched).toEqual([]);
  });

  it('a link to ?tab=member opens that tab directly', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/-/projects?tab=member' });
    await screen.findByText('Forge Web');
    expect(projectQueries(server)).toEqual([[null, 'true']]);
    expect(within(tabs()).getByRole('link', { name: '参加中' }).getAttribute('aria-current')).toBe('page');
  });

  it('loading, then the empty state with the next step (each tab its own words)', async () => {
    const server = mockServer(routes({ 'GET /api/v1/projects': () => page([]) }));
    await renderApp({ fetch: server.fetch, path: '/-/projects' });
    expect(screen.getAllByText('読み込んでいます…').length).toBeGreaterThan(0);
    expect(await screen.findByText('あなたが作ったプロジェクトはまだありません。')).toBeTruthy();
    expect(screen.getByText(/参加しているプロジェクトは「参加中」のタブにあります/)).toBeTruthy();
    // The "no recorded creator" hint belongs to a list with rows only.
    expect(screen.queryByText(/作った人の記録がない古いプロジェクト/)).toBeNull();
    fireEvent.click(screen.getAllByRole('button', { name: 'プロジェクトを作る' })[1] as HTMLElement);
    expect(screen.getByRole('dialog')).toBeTruthy();
  });

  it('an unknown ?tab= falls back to "created by me"', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/-/projects?tab=foo' });
    await screen.findByText('Forge Web');
    expect(projectQueries(server)).toEqual([['me', null]]);
    expect(within(tabs()).getByRole('link', { name: '自分が作成' }).getAttribute('aria-current')).toBe('page');
    expect(within(tabs()).getByRole('link', { name: '参加中' }).getAttribute('aria-current')).toBeNull();
  });

  it('browser Back from "参加中" returns to the created view and its query', async () => {
    const server = mockServer(routes());
    window.history.replaceState(null, '', '/-/projects');
    const i18n = await createI18n('ja');
    const api = createApiClient({ language: () => i18n.language, fetch: server.fetch });
    const queryClient = createQueryClient();
    queryClient.setDefaultOptions({ queries: { retry: false } });
    render(
      <AppProviders i18n={i18n} api={api} queryClient={queryClient}>
        <BrowserRouter>
          <AppRoutes />
        </BrowserRouter>
      </AppProviders>,
    );
    await screen.findByText('Forge Web');
    fireEvent.click(within(tabs()).getByRole('link', { name: '参加中' }));
    await waitFor(() => {
      expect(window.location.search).toBe('?tab=member');
    });
    await waitFor(() => {
      expect(projectQueries(server)).toEqual([
        ['me', null],
        [null, 'true'],
      ]);
    });
    window.history.back();
    await waitFor(() => {
      expect(window.location.search).toBe('');
    });
    await waitFor(() => {
      expect(within(tabs()).getByRole('link', { name: '自分が作成' }).getAttribute('aria-current')).toBe('page');
    });
    expect(await screen.findByText(/作った人の記録がない古いプロジェクト/)).toBeTruthy();
    // The created view is the `created_by=me` query again (served from the
    // cache or refetched — never the membership one).
    expect(projectQueries(server).every(([c, m]) => (c === 'me' && m === null) || (c === null && m === 'true'))).toBe(true);
    expect(document.title).toBe('プロジェクト · GitStation');
    window.history.replaceState(null, '', '/');
  });

  it('an error on ?tab=member shows user_message + request_id only', async () => {
    const server = mockServer(
      routes({
        'GET /api/v1/projects': () => apiError(500, '一覧を読めませんでした。', 'req-m-1', 'FG-DB-0001'),
      }),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: '/-/projects?tab=member' });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('一覧を読めませんでした。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-m-1');
    expect(container.textContent).not.toContain('FG-DB-0001');
    expect(projectQueries(server)[0]).toEqual([null, 'true']);
  });

  it('the member tab keeps its own empty state', async () => {
    const server = mockServer(routes({ 'GET /api/v1/projects': () => page([]) }));
    await renderApp({ fetch: server.fetch, path: '/-/projects?tab=member' });
    expect(await screen.findByText('あなたがメンバーのプロジェクトはまだありません。')).toBeTruthy();
  });

  it('an error shows user_message + request_id only, and retries', async () => {
    let fail = true;
    const server = mockServer(
      routes({
        'GET /api/v1/projects': () =>
          fail ? apiError(500, '一覧を読めませんでした。', 'req-p-1', 'FG-DB-0001') : page([PROJECT]),
      }),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: '/-/projects' });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('一覧を読めませんでした。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-p-1');
    expect(container.textContent).not.toContain('FG-DB-0001');
    fail = false;
    fireEvent.click(screen.getByRole('button', { name: 'もう一度試す' }));
    expect(await screen.findByText('Forge Web')).toBeTruthy();
  });

  it('axe: no violations', async () => {
    const server = mockServer(routes());
    const { container } = await renderApp({ fetch: server.fetch, path: '/-/projects' });
    await screen.findByText('Forge Web');
    expect(await violations(container)).toEqual([]);
  });
});

describe('/-/groups', () => {
  it('lists every group I may see (no membership filter); no create button for a non-admin', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/-/groups' });
    expect(await screen.findByRole('heading', { name: 'グループ', level: 1 })).toBeTruthy();
    expect(await screen.findByRole('link', { name: 'Acme Plant' })).toBeTruthy();
    expect(server.to('GET', '/api/v1/groups')[0]?.query.get('membership')).toBeNull();
    expect(screen.queryByRole('button', { name: 'グループを作る' })).toBeNull();
    expect(server.unmatched).toEqual([]);
  });

  it('an administrator gets the create-group button', async () => {
    const admin = { ...SESSION, user: { ...SESSION.user, is_admin: true } };
    const server = mockServer(routes({ 'GET /api/v1/session': () => json(200, admin) }));
    await renderApp({ fetch: server.fetch, path: '/-/groups' });
    fireEvent.click(await screen.findByRole('button', { name: 'グループを作る' }));
    expect(screen.getByRole('dialog')).toBeTruthy();
  });

  it('loading, then the empty state says what to do (non-admin / admin)', async () => {
    const server = mockServer(routes({ 'GET /api/v1/groups': () => page([]) }));
    const first = await renderApp({ fetch: server.fetch, path: '/-/groups' });
    expect(await screen.findByText('見られるグループはまだありません。')).toBeTruthy();
    expect(screen.getByText('参加しているグループがここに出ます。')).toBeTruthy();
    expect(screen.getByText('管理者に作成を依頼してください。')).toBeTruthy();
    first.unmount();

    const admin = { ...SESSION, user: { ...SESSION.user, is_admin: true } };
    const server2 = mockServer(
      routes({ 'GET /api/v1/session': () => json(200, admin), 'GET /api/v1/groups': () => page([]) }),
    );
    await renderApp({ fetch: server2.fetch, path: '/-/groups' });
    expect(screen.getAllByText('読み込んでいます…').length).toBeGreaterThan(0);
    expect(await screen.findByText(/最初のグループを作ってください/)).toBeTruthy();
  });

  it('an error shows user_message + request_id only', async () => {
    const server = mockServer(
      routes({ 'GET /api/v1/groups': () => apiError(503, 'グループを読めませんでした。', 'req-g-1', 'FG-DB-0002') }),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: '/-/groups' });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('グループを読めませんでした。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-g-1');
    expect(container.textContent).not.toContain('FG-DB-0002');
    expect(screen.getByRole('button', { name: 'もう一度試す' })).toBeTruthy();
  });

  it('axe: no violations', async () => {
    const server = mockServer(routes());
    const { container } = await renderApp({ fetch: server.fetch, path: '/-/groups' });
    await screen.findByText('Acme Plant');
    expect(await violations(container)).toEqual([]);
  });
});
