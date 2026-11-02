import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import {
  GROUP,
  LISTED_GROUP,
  PROJECT,
  REFS_EMPTY,
  SESSION,
  apiError,
  mockServer,
  page,
  signedIn,
} from './mock';
import { json, renderApp } from './render';

const lists = (mine: unknown[], groups: unknown[] = []) => ({
  ...signedIn(),
  'GET /api/v1/projects': () => page(mine),
  'GET /api/v1/groups': () => page(groups),
});

describe('home', () => {
  it('shows my projects and groups in the UI language', async () => {
    const server = mockServer(lists([PROJECT], [LISTED_GROUP]));
    await renderApp({ fetch: server.fetch, lang: 'en' });
    expect(await screen.findByRole('heading', { name: 'Your projects' })).toBeTruthy();
    expect(await screen.findByText('Forge Web')).toBeTruthy();
    expect(await screen.findByText('Acme Plant')).toBeTruthy();
    // The main list asks for the projects I hold a role in.
    const mine = server.to('GET', '/api/v1/projects')[0];
    // ADR-0023: no repository-kind filter is sent.
    expect(mine?.query.get('kind')).toBeNull();
    expect(mine?.query.get('membership')).toBe('true');
    expect(mine?.headers.get('accept-language')).toBe('en');
    expect(server.unmatched).toEqual([]);
  });

  it('links a project to its address', async () => {
    const server = mockServer(lists([PROJECT]));
    await renderApp({ fetch: server.fetch });
    const link = await screen.findByRole('link', { name: 'Forge Web' });
    expect(link.getAttribute('href')).toBe('/acme/forge-web');
  });

  it('loading state, then the empty state with the way forward (no empty frames for the rest)', async () => {
    const server = mockServer(lists([]));
    await renderApp({ fetch: server.fetch });
    expect(screen.getByText('読み込んでいます…')).toBeTruthy();
    expect(await screen.findByText('あなたがメンバーのプロジェクトはまだありません。')).toBeTruthy();
    expect(screen.queryByRole('heading', { name: '自分のグループ' })).toBeNull();
    expect(screen.queryByRole('heading', { name: '最近見たもの' })).toBeNull();
  });

  it('filters the list by name or path', async () => {
    const other = { ...PROJECT, id: 9, path: 'acme/press-line', display_name: 'Press Line' };
    const server = mockServer(lists([PROJECT, other]));
    await renderApp({ fetch: server.fetch });
    await screen.findByText('Press Line');
    fireEvent.change(screen.getByLabelText('名前やパスで絞り込む'), { target: { value: 'press' } });
    expect(screen.queryByText('Forge Web')).toBeNull();
    expect(screen.getByText('Press Line')).toBeTruthy();
    fireEvent.change(screen.getByLabelText('名前やパスで絞り込む'), { target: { value: 'zzz' } });
    expect(screen.getByText('条件に合うプロジェクトはありません。')).toBeTruthy();
  });

  it('reads every page of a list and says when the cap cut it', async () => {
    let n = 0;
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/projects': () => {
        n += 1;
        // A page can be empty while more follow; the last says so.
        return page(n === 1 ? [] : [{ ...PROJECT, id: n, path: `acme/p${n}`, display_name: `P${n}` }], {
          has_more: n < 3,
          next_cursor: n < 3 ? `cur${n}` : null,
        });
      },
      'GET /api/v1/groups': () => page([]),
    });
    await renderApp({ fetch: server.fetch });
    expect(await screen.findByText('P2')).toBeTruthy();
    expect(await screen.findByText('P3')).toBeTruthy();
    const mineCalls = server.to('GET', '/api/v1/projects');
    expect(mineCalls.map((c) => c.query.get('cursor'))).toEqual([null, 'cur1', 'cur2']);
    expect(screen.queryByText(/一部しか表示していません/)).toBeNull();
  });

  it('an error shows user_message + request_id and a retry that asks again', async () => {
    let fail = true;
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/projects': () =>
        fail
          ? apiError(500, '一覧を読めませんでした。', 'req-list-1', 'FG-DB-0001')
          : page([PROJECT]),
      'GET /api/v1/groups': () => page([]),
    });
    const { container } = await renderApp({ fetch: server.fetch });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('一覧を読めませんでした。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-list-1');
    expect(container.textContent).not.toContain('FG-DB-0001');
    fail = false;
    fireEvent.click(screen.getByRole('button', { name: 'もう一度試す' }));
    expect(await screen.findByText('Forge Web')).toBeTruthy();
  });
});

describe('create a project', () => {
  const open = async (server: ReturnType<typeof mockServer>, path = '/') => {
    await renderApp({ fetch: server.fetch, path });
    fireEvent.click(await screen.findByRole('button', { name: 'プロジェクトを作る' }));
    return screen.getByRole('dialog');
  };
  const routes = () => ({
    ...lists([]),
    'GET /api/v1/namespaces/resolve': () =>
      json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
  });

  it('sends the form with the CSRF token of the session and goes to the new project', async () => {
    const created = { ...PROJECT, id: 21, path: 'acme/new-one', display_name: 'New One' };
    const server = mockServer({
      ...routes(),
      'POST /api/v1/projects': () => json(201, created),
      'GET /api/v1/namespaces/resolve': (c) =>
        c.query.get('path') === 'taro'
          ? json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 })
          : json(200, { canonical_path: 'acme/new-one', kind: 'project', moved: false, namespace_id: 99, project_id: 21 }),
      'GET /api/v1/projects/21': () => json(200, created),
      'GET /api/v1/projects/21/repository/refs': () => json(200, REFS_EMPTY),
    });
    const dialog = await open(server);
    const d = within(dialog);
    // The personal namespace is the first choice once it is known.
    await waitFor(() => {
      expect(d.getByRole('option', { name: /taro/ })).toBeTruthy();
    });
    fireEvent.change(d.getByLabelText('作る場所'), { target: { value: '55' } });
    fireEvent.change(d.getByLabelText('名前'), { target: { value: 'New One' } });
    // The path is suggested from the name until the person types one.
    expect((d.getByLabelText<HTMLInputElement>(/^パス/)).value).toBe('new-one');
    fireEvent.change(d.getByLabelText('説明（省略できます）'), { target: { value: 'hello' } });
    fireEvent.click(d.getByLabelText('社内に公開'));
    // ADR-0023: the form has no kind choice any more.
    expect(d.queryByLabelText('仕様書')).toBeNull();
    expect(d.queryByLabelText('システム')).toBeNull();
    fireEvent.click(d.getByRole('button', { name: 'プロジェクトを作る' }));

    expect(await screen.findByRole('heading', { name: 'New One' })).toBeTruthy();
    const post = server.to('POST', '/api/v1/projects')[0];
    expect(post?.body).toEqual({
      namespace_id: 55,
      display_name: 'New One',
      path: 'new-one',
      description: 'hello',
      visibility: 'internal',
    });
    expect(post?.headers.get('x-gitstation-csrf')).toBe(SESSION.csrf_token);
    expect(screen.getByTestId('location').textContent).toBe('/acme/new-one');
    // Safe requests carry no CSRF header.
    expect(server.to('GET', '/api/v1/session')[0]?.headers.get('x-gitstation-csrf')).toBeNull();
  });

  it('checks the obvious mistakes before asking the server', async () => {
    const server = mockServer(routes());
    const dialog = await open(server);
    fireEvent.click(within(dialog).getByRole('button', { name: 'プロジェクトを作る' }));
    expect(within(dialog).getAllByText('名前を入力してください。')).toHaveLength(2);
    expect(within(dialog).getAllByText('作る場所を選んでください。')).toHaveLength(2);
    expect(within(dialog).getAllByText(/先頭と末尾は英数字/)).toHaveLength(2);
    expect(server.to('POST', '/api/v1/projects')).toHaveLength(0);
    // A Japanese name gives no ASCII suggestion: the person types the path.
    fireEvent.change(within(dialog).getByLabelText('名前'), { target: { value: '圧造ライン' } });
    expect((within(dialog).getByLabelText<HTMLInputElement>(/^パス/)).value).toBe('');
  });

  it('a server refusal is shown in the dialog with its request id; the form keeps its input', async () => {
    const server = mockServer({
      ...routes(),
      'POST /api/v1/projects': () =>
        apiError(409, 'このパスはすでに使われています。', 'req-dup-1', 'FG-REPO-0003'),
    });
    const dialog = await open(server);
    const d = within(dialog);
    await waitFor(() => {
      expect(d.getByRole('option', { name: /taro/ })).toBeTruthy();
    });
    fireEvent.change(d.getByLabelText('作る場所'), { target: { value: '55' } });
    fireEvent.change(d.getByLabelText('名前'), { target: { value: 'Dup' } });
    fireEvent.click(d.getByRole('button', { name: 'プロジェクトを作る' }));
    const alert = await d.findByRole('alert');
    expect(alert.textContent).toContain('このパスはすでに使われています。');
    expect(d.getByTestId('request-id').textContent).toBe('req-dup-1');
    expect(dialog.textContent).not.toContain('FG-REPO-0003');
    expect((d.getByLabelText<HTMLInputElement>('名前')).value).toBe('Dup');
  });
});

describe('create a group', () => {
  it('posts to /groups with the CSRF token and opens the group', async () => {
    const created = { ...GROUP, id: 40, namespace_id: 400, path: 'press', name: 'press', display_name: 'Press' };
    const server = mockServer({
      ...lists([]),
      'POST /api/v1/groups': () => json(201, created),
      'GET /api/v1/namespaces/resolve': () =>
        json(200, { canonical_path: 'press', kind: 'group', moved: false, namespace_id: 400, group_id: 40 }),
      'GET /api/v1/groups/40': () => json(200, created),
    });
    await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('button', { name: 'グループを作る' }));
    const d = within(screen.getByRole('dialog'));
    fireEvent.change(d.getByLabelText('名前'), { target: { value: 'Press' } });
    fireEvent.click(d.getByRole('button', { name: 'グループを作る' }));
    expect(await screen.findByRole('heading', { name: 'Press', level: 1 })).toBeTruthy();
    const post = server.to('POST', '/api/v1/groups')[0];
    expect(post?.body).toEqual({ display_name: 'Press', path: 'press', description: null });
    expect(post?.headers.get('x-gitstation-csrf')).toBe(SESSION.csrf_token);
  });
});
