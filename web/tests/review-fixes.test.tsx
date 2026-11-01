// Fixes from the MR9 reviews: focus handling, a save that cannot be
// cancelled, the forbidden state, one alert per page, URL encoding.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import {
  GROUP,
  PROJECT,
  REFS_FILLED,
  RESOLVE_GROUP,
  SESSION,
  apiError,
  mockServer,
  page,
  signedIn,
} from './mock';
import { json, renderApp } from './render';

const lists = () => ({
  ...signedIn(),
  'GET /api/v1/projects': () => page([]),
  'GET /api/v1/groups': () => page([]),
  'GET /api/v1/namespaces/resolve': () =>
    json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
});

describe('forms: focus and cancel', () => {
  it('opening the dialog focuses its first field, not the Close button', async () => {
    const server = mockServer(lists());
    await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('button', { name: 'グループを作る' }));
    const dialog = screen.getByRole('dialog');
    expect(document.activeElement).toBe(within(dialog).getByLabelText('名前'));
  });

  it('an invalid submit moves focus to the error summary (an alert) that lists the problems', async () => {
    const server = mockServer(lists());
    await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('button', { name: 'グループを作る' }));
    const dialog = screen.getByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'グループを作る' }));
    const summary = within(dialog).getAllByRole('alert')[0] as HTMLElement;
    expect(document.activeElement).toBe(summary);
    expect(summary.textContent).toContain('入力を確かめてください。');
    expect(summary.textContent).toContain('名前を入力してください。');
    expect(within(dialog).getByLabelText('名前').getAttribute('aria-invalid')).toBe('true');
    expect(server.to('POST', '/api/v1/groups')).toHaveLength(0);
  });

  it('Close and Esc do nothing while the save is under way; the result still arrives', async () => {
    let release: (r: Response) => void = () => undefined;
    const created = { ...GROUP, id: 40, namespace_id: 400, path: 'press', name: 'press', display_name: 'Press' };
    const server = mockServer({
      ...lists(),
      'POST /api/v1/groups': () =>
        new Promise<Response>((resolve) => {
          release = resolve;
        }),
      'GET /api/v1/namespaces/resolve': () =>
        json(200, { canonical_path: 'press', kind: 'group', moved: false, namespace_id: 400, group_id: 40 }),
      'GET /api/v1/groups/40': () => json(200, created),
    });
    await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('button', { name: 'グループを作る' }));
    const dialog = screen.getByRole('dialog');
    fireEvent.change(within(dialog).getByLabelText('名前'), { target: { value: 'Press' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'グループを作る' }));
    await waitFor(() => {
      expect(within(dialog).getByRole('button', { name: '閉じる' }).hasAttribute('disabled')).toBe(true);
    });
    const cancel = new Event('cancel', { cancelable: true });
    dialog.dispatchEvent(cancel);
    expect(cancel.defaultPrevented).toBe(true);
    release(json(201, created));
    expect(await screen.findByRole('heading', { level: 1, name: 'Press' })).toBeTruthy();
  });

  it('a 403 on a save reads the session again (a stale CSRF token)', async () => {
    const server = mockServer({
      ...lists(),
      'POST /api/v1/groups': () => apiError(403, '要求を受け付けられませんでした。', 'req-403', 'FG-AUTH-0016'),
    });
    await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('button', { name: 'グループを作る' }));
    const dialog = screen.getByRole('dialog');
    fireEvent.change(within(dialog).getByLabelText('名前'), { target: { value: 'Press' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'グループを作る' }));
    await within(dialog).findByRole('alert');
    await waitFor(() => {
      expect(server.to('GET', '/api/v1/session').length).toBeGreaterThan(1);
    });
  });
});

describe('menu and page focus', () => {
  it('Escape closes the user menu and returns focus to its button', async () => {
    const server = mockServer(lists());
    await renderApp({ fetch: server.fetch });
    const trigger = await screen.findByRole('button', { name: /山田 太郎/ });
    fireEvent.click(trigger);
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(trigger.getAttribute('aria-expanded')).toBe('false');
    expect(document.activeElement).toBe(trigger);
  });

  it('moving focus out of the menu closes it', async () => {
    const server = mockServer(lists());
    await renderApp({ fetch: server.fetch });
    const trigger = await screen.findByRole('button', { name: /山田 太郎/ });
    fireEvent.click(trigger);
    const outside = screen.getByLabelText('表示する言語');
    fireEvent.blur(trigger, { relatedTarget: outside });
    expect(trigger.getAttribute('aria-expanded')).toBe('false');
  });

  it('after a navigation the new page heading takes the focus', async () => {
    const server = mockServer({
      ...lists(),
      'GET /api/v1/projects': (c) => (c.query.get('kind') === 'spec' ? page([]) : page([PROJECT])),
      'GET /api/v1/namespaces/resolve': () =>
        json(200, { canonical_path: 'acme/forge-web', kind: 'project', moved: false, namespace_id: 11, project_id: 7 }),
      'GET /api/v1/projects/7': () => json(200, PROJECT),
      'GET /api/v1/projects/7/repository/refs': () => json(200, REFS_FILLED),
    });
    await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('link', { name: 'Forge Web' }));
    const h1 = await screen.findByRole('heading', { level: 1, name: 'Forge Web' });
    await waitFor(() => {
      expect(document.activeElement).toBe(h1);
    });
  });
});

describe('states', () => {
  it('forbidden (403) has its own title and still shows user_message + request_id', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_GROUP),
      'GET /api/v1/groups/3': () => apiError(403, 'このグループを見る権限がありません。', 'req-403-g', 'FG-AUTH-0004'),
    });
    const { container } = await renderApp({ fetch: server.fetch, path: '/acme' });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('権限がありません');
    expect(alert.textContent).toContain('このグループを見る権限がありません。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-403-g');
    expect(container.textContent).not.toContain('FG-AUTH-0004');
    expect(screen.queryByRole('button', { name: 'もう一度試す' })).toBeNull();
  });

  it('when every list fails the home page shows one alert, not three', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/projects': () => apiError(500, '失敗', 'req-x'),
      'GET /api/v1/groups': () => apiError(500, '失敗', 'req-y'),
    });
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('alert');
    await waitFor(() => {
      expect(screen.getAllByRole('alert')).toHaveLength(1);
    });
  });

  it('loading is a block with a reserved height, announced as a status', async () => {
    const server = mockServer({ 'GET /api/v1/session': () => new Promise<Response>(() => undefined) });
    await renderApp({ fetch: server.fetch });
    const status = screen.getByText('読み込んでいます…').parentElement as HTMLElement;
    expect(status.getAttribute('role')).toBe('status');
    expect(status.className).toContain('block');
  });

  it('the empty home offers the primary action', async () => {
    const server = mockServer(lists());
    await renderApp({ fetch: server.fetch });
    const empty = (await screen.findByText('あなたがメンバーのプロジェクトはまだありません。')).parentElement as HTMLElement;
    fireEvent.click(within(empty).getByRole('button', { name: 'プロジェクトを作る' }));
    expect(screen.getByRole('dialog')).toBeTruthy();
  });
});

describe('permalink', () => {
  it('encodes each segment of the screen path it keeps', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/projects/7': () => json(200, PROJECT),
      'GET /api/v1/namespaces/resolve': () => apiError(404, 'x', 'r'),
    });
    await renderApp({ fetch: server.fetch, path: '/-/projects/7/blob/main/%E5%9B%B3%20%E9%9D%A2.md' });
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe(
        '/acme/forge-web/-/blob/main/%E5%9B%B3%20%E9%9D%A2.md',
      );
    });
  });

  it('an id of more than 15 digits is not found without asking', async () => {
    const server = mockServer(signedIn());
    await renderApp({ fetch: server.fetch, path: '/-/projects/1234567890123456' });
    expect(await screen.findByText('ページが見つかりません')).toBeTruthy();
    expect(server.calls.map((c) => c.path)).toEqual(['/api/v1/session']);
  });
});

describe('session', () => {
  it('the CSRF token of a re-read session is used for the next save', async () => {
    let token = 'a'.repeat(64);
    const server = mockServer({
      ...lists(),
      'GET /api/v1/session': () => json(200, { ...SESSION, csrf_token: token }),
      'POST /api/v1/groups': () => apiError(403, 'x', 'r', 'FG-AUTH-0016'),
    });
    const { api } = await renderApp({ fetch: server.fetch });
    await screen.findByRole('button', { name: 'グループを作る' });
    expect(api.csrf.token).toBe('a'.repeat(64));
    token = 'b'.repeat(64);
    fireEvent.click(screen.getByRole('button', { name: 'グループを作る' }));
    const dialog = screen.getByRole('dialog');
    fireEvent.change(within(dialog).getByLabelText('名前'), { target: { value: 'P' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'グループを作る' }));
    await waitFor(() => {
      expect(api.csrf.token).toBe('b'.repeat(64));
    });
  });
});
