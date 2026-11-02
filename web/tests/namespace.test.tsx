import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import {
  GROUP,
  LISTED_GROUP,
  PROJECT,
  REFS_EMPTY,
  REFS_FILLED,
  RESOLVE_GROUP,
  RESOLVE_PROJECT,
  apiError,
  mockServer,
  page,
  signedIn,
} from './mock';
import { json, renderApp } from './render';
import { repoRoutes } from './repo-fixtures';

const project = (refs: unknown = REFS_FILLED) =>
  repoRoutes({
    'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_PROJECT),
    'GET /api/v1/projects/7/repository/refs': () => json(200, refs),
  });

describe('project top page', () => {
  it('shows the summary, the clone URL and the repository state', async () => {
    const server = mockServer(project());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web', lang: 'en' });
    expect(await screen.findByRole('heading', { level: 1, name: 'Forge Web' })).toBeTruthy();
    expect(screen.getByText('The web UI')).toBeTruthy();
    expect(screen.getByTestId('clone-url').textContent).toBe(
      `${window.location.origin}/acme/forge-web.git`,
    );
    expect(await screen.findByText('Default branch')).toBeTruthy();
    const state = within(screen.getByRole('region', { name: 'Repository' }));
    expect(state.getByText('main')).toBeTruthy();
    // 2 branches, 1 tag.
    expect(state.getByText('2')).toBeTruthy();
    expect(state.getByText('1')).toBeTruthy();
    // The breadcrumb links the group; the current page is not a link.
    expect(screen.getByRole('link', { name: 'acme' }).getAttribute('href')).toBe('/acme');
    expect(document.title).toBe('Forge Web · GitStation');
    expect(server.unmatched).toEqual([]);
  });

  it('an empty repository says how to make the first push', async () => {
    const server = mockServer(project(REFS_EMPTY));
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    expect(await screen.findByRole('heading', { name: 'リポジトリはまだ空です' })).toBeTruthy();
    const commands = screen.getByText(/git push -u origin main/);
    expect(commands.textContent).toContain(`git clone ${window.location.origin}/acme/forge-web.git`);
    expect(commands.textContent).toContain('cd forge-web');
  });

  it('copies the clone URL; says so when the clipboard is missing', async () => {
    const server = mockServer(project());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    fireEvent.click(await screen.findByRole('button', { name: 'URL をコピー' }));
    expect(await screen.findByText(/コピーできませんでした/)).toBeTruthy();
  });

  it('a failing refs call is shown with its request id, the rest of the page stays', async () => {
    const server = mockServer({
      ...project(),
      'GET /api/v1/projects/7/repository/refs': () =>
        apiError(500, '参照を読めませんでした。', 'req-refs-1', 'FG-REPO-0016'),
    });
    const { container } = await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('参照を読めませんでした。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-refs-1');
    expect(container.textContent).not.toContain('FG-REPO-0016');
    expect(screen.getByRole('heading', { level: 1, name: 'Forge Web' })).toBeTruthy();
  });

  it('remembers the project for "recently viewed" in this browser', async () => {
    window.localStorage.clear();
    const server = mockServer(project());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    await screen.findByRole('heading', { level: 1, name: 'Forge Web' });
    expect(window.localStorage.getItem('gitstation.recent')).toContain('acme/forge-web');
  });
});

describe('URL rescue (ADR-0015, plan 10 §4.3)', () => {
  it('a GitHub-shaped URL moves behind /-/ without a new history entry', async () => {
    const server = mockServer({
      ...project(),
      'GET /api/v1/namespaces/resolve': (c) =>
        json(200, c.query.get('prefix') === 'true' ? { ...RESOLVE_PROJECT, remainder: 'settings' } : RESOLVE_PROJECT),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/settings' });
    // The rescued address is the project's settings screen.
    expect(await screen.findByRole('heading', { level: 1, name: '設定' })).toBeTruthy();
    expect(screen.getByTestId('location').textContent).toBe('/acme/forge-web/-/settings');
    const first = server.to('GET', '/api/v1/namespaces/resolve')[0];
    expect(first?.query.get('path')).toBe('acme/forge-web/settings');
    expect(first?.query.get('prefix')).toBe('true');
    expect(screen.getByRole('link', { name: 'Forge Web' }).getAttribute('href')).toBe(
      '/acme/forge-web',
    );
  });

  it('a remainder that is not a screen word is not found (no guessing)', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': () => json(200, { ...RESOLVE_PROJECT, remainder: 'pull/3' }),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/pull/3' });
    expect(await screen.findByText('ページが見つかりません')).toBeTruthy();
    expect(screen.getByTestId('location').textContent).toBe('/acme/forge-web/pull/3');
  });

  it('a moved path is replaced by the canonical one and says so', async () => {
    const server = mockServer({
      ...project(),
      'GET /api/v1/namespaces/resolve': (c) =>
        json(
          200,
          c.query.get('path') === 'old/forge-web'
            ? { ...RESOLVE_PROJECT, moved: true }
            : RESOLVE_PROJECT,
        ),
    });
    await renderApp({ fetch: server.fetch, path: '/old/forge-web' });
    expect(await screen.findByRole('heading', { level: 1, name: 'Forge Web' })).toBeTruthy();
    expect(screen.getByTestId('location').textContent).toBe('/acme/forge-web');
    expect(screen.getByText(/この場所は移動しました/).textContent).toContain('old/forge-web');
  });

  it('a different spelling is replaced by the canonical path', async () => {
    const server = mockServer({
      ...project(),
      'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_PROJECT),
    });
    await renderApp({ fetch: server.fetch, path: '/Acme/Forge-Web' });
    await screen.findByRole('heading', { level: 1, name: 'Forge Web' });
    expect(screen.getByTestId('location').textContent).toBe('/acme/forge-web');
  });

  it('a permalink /-/projects/{id}/… is replaced by the current path', async () => {
    const server = mockServer(project());
    await renderApp({ fetch: server.fetch, path: '/-/projects/7' });
    await screen.findByRole('heading', { level: 1, name: 'Forge Web' });
    expect(screen.getByTestId('location').textContent).toBe('/acme/forge-web');
  });

  it('a permalink with a screen keeps it after /-/', async () => {
    const server = mockServer(project());
    await renderApp({ fetch: server.fetch, path: '/-/projects/7/members' });
    await screen.findByRole('heading', { level: 1, name: 'メンバー' });
    expect(screen.getByTestId('location').textContent).toBe('/acme/forge-web/-/members');
  });

  it('a permalink whose id is not a positive number is not found without asking', async () => {
    const server = mockServer(signedIn());
    await renderApp({ fetch: server.fetch, path: '/-/projects/0' });
    expect(await screen.findByText('ページが見つかりません')).toBeTruthy();
    expect(server.calls.map((c) => c.path)).toEqual(['/api/v1/session']);
  });

  it('a path that does not decode is not found without asking', async () => {
    const server = mockServer(signedIn());
    await renderApp({ fetch: server.fetch, path: '/acme/%E0%A4%A' });
    expect(await screen.findByText('ページが見つかりません')).toBeTruthy();
    expect(server.calls.map((c) => c.path)).toEqual(['/api/v1/session']);
  });

  it('an error that is not a 404 is shown, not turned into not-found', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': () =>
        apiError(429, '少し待ってください。', 'req-rl-9', 'FG-WEB-0010'),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('少し待ってください。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-rl-9');
  });
});

describe('group page', () => {
  const group = () => ({
    ...signedIn(),
    'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_GROUP),
    'GET /api/v1/groups/3': () => json(200, GROUP),
    'GET /api/v1/groups': () =>
      page([
        LISTED_GROUP,
        { ...LISTED_GROUP, id: 4, path: 'acme/line-1', name: 'line-1', display_name: 'Line 1' },
        { ...LISTED_GROUP, id: 5, path: 'acme/line-1/cell', name: 'cell', display_name: 'Cell' },
        { ...LISTED_GROUP, id: 6, path: 'other', name: 'other', display_name: 'Other' },
      ]),
    'GET /api/v1/projects': () =>
      page([
        PROJECT,
        { ...PROJECT, id: 9, path: 'acme/line-1/deep', display_name: 'Deep' },
        { ...PROJECT, id: 10, path: 'other/x', display_name: 'Elsewhere' },
      ]),
  });

  it('lists only what is directly under the group', async () => {
    const server = mockServer(group());
    await renderApp({ fetch: server.fetch, path: '/acme' });
    expect(await screen.findByRole('heading', { level: 1, name: 'Acme Plant' })).toBeTruthy();
    expect(await screen.findByText('Line 1')).toBeTruthy();
    expect(screen.getByText('Forge Web')).toBeTruthy();
    expect(screen.queryByText('Cell')).toBeNull();
    expect(screen.queryByText('Deep')).toBeNull();
    expect(screen.queryByText('Other')).toBeNull();
    expect(screen.queryByText('Elsewhere')).toBeNull();
  });

  it('creating a subgroup posts to the group with the CSRF token', async () => {
    const created = { ...GROUP, id: 41, namespace_id: 410, parent_id: 3, path: 'acme/new', name: 'new', display_name: 'New' };
    const server = mockServer({
      ...group(),
      'POST /api/v1/groups/3/subgroups': () => json(201, created),
    });
    await renderApp({ fetch: server.fetch, path: '/acme' });
    fireEvent.click(await screen.findByRole('button', { name: 'サブグループを作る' }));
    const d = within(screen.getByRole('dialog'));
    expect(d.getByText('acme')).toBeTruthy();
    fireEvent.change(d.getByLabelText('名前'), { target: { value: 'New' } });
    fireEvent.click(d.getByRole('button', { name: 'グループを作る' }));
    await waitFor(() => {
      expect(server.to('POST', '/api/v1/groups/3/subgroups')).toHaveLength(1);
    });
    const post = server.to('POST', '/api/v1/groups/3/subgroups')[0];
    expect(post?.body).toEqual({ display_name: 'New', path: 'new', description: null });
    expect(post?.headers.get('x-gitstation-csrf')).toHaveLength(64);
  });

  it('an empty group says so', async () => {
    const server = mockServer({
      ...group(),
      'GET /api/v1/groups': () => page([LISTED_GROUP]),
      'GET /api/v1/projects': () => page([]),
    });
    await renderApp({ fetch: server.fetch, path: '/acme' });
    expect(await screen.findByText(/見られるグループやプロジェクトがありません/)).toBeTruthy();
  });

  it('without a role (and not an administrator) there are no create buttons', async () => {
    const server = mockServer({
      ...group(),
      'GET /api/v1/groups/3': () => json(200, { ...GROUP, role: null }),
    });
    await renderApp({ fetch: server.fetch, path: '/acme' });
    await screen.findByRole('heading', { level: 1, name: 'Acme Plant' });
    expect(screen.queryByRole('button', { name: 'サブグループを作る' })).toBeNull();
  });
});

describe('personal namespace', () => {
  it('lists the projects directly under the user', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': () =>
        json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
      'GET /api/v1/groups': () => page([]),
      'GET /api/v1/projects': () =>
        page([{ ...PROJECT, id: 12, path: 'taro/notes', display_name: 'Notes' }, PROJECT]),
    });
    await renderApp({ fetch: server.fetch, path: '/taro' });
    expect(await screen.findByText('Notes')).toBeTruthy();
    expect(screen.queryByText('Forge Web')).toBeNull();
    // It is my own namespace: I can create here.
    expect(screen.getByRole('button', { name: 'プロジェクトを作る' })).toBeTruthy();
  });
});
