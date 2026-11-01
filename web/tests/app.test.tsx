import { fireEvent, screen, waitFor } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import {
  PROJECT,
  REFS_FILLED,
  RESOLVE_PROJECT,
  apiError,
  mockServer,
  page,
  signedIn,
} from './mock';
import { json, renderApp } from './render';

describe('error display (CLAUDE.md contract 3)', () => {
  it('a failure shows user_message and request_id only (never error_code or status)', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/projects': () =>
        apiError(503, 'いまは処理できません。', 'req-abc123', 'FG-DB-0001'),
      'GET /api/v1/groups': () => page([]),
    });
    const { container } = await renderApp({ fetch: server.fetch });
    const alert = (await screen.findAllByRole('alert'))[0];
    expect(alert?.textContent).toContain('いまは処理できません。');
    expect(screen.getAllByTestId('request-id')[0]?.textContent).toBe('req-abc123');
    expect(container.textContent).not.toContain('FG-DB-0001');
    expect(container.textContent).not.toContain('503');
  });

  it('a body that is not the error contract is shown as unexpected, never raw', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/projects': () =>
        new Response('<html>proxy error: upstream 10.0.0.1</html>', {
          status: 502,
          headers: { 'x-request-id': 'req-proxy' },
        }),
      'GET /api/v1/groups': () => page([]),
    });
    const { container } = await renderApp({ fetch: server.fetch });
    await screen.findAllByRole('alert');
    expect(container.textContent).toContain('思いがけない応答がありました');
    expect(container.textContent).not.toContain('proxy error');
    expect(screen.getAllByTestId('request-id')[0]?.textContent).toBe('req-proxy');
  });

  it('a network failure shows the network message without a request id', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/projects': () => Promise.reject(new TypeError('Failed to fetch')),
      'GET /api/v1/groups': () => page([]),
    });
    await renderApp({ fetch: server.fetch });
    await screen.findAllByRole('alert');
    expect(screen.getAllByText(/サーバーにつながりませんでした/).length).toBeGreaterThan(0);
    expect(screen.queryByTestId('request-id')).toBeNull();
  });

  it('a session check that fails (not 401) shows the error, not the sign-in screen', async () => {
    const server = mockServer({
      'GET /api/v1/session': () => apiError(500, 'サーバーの内側で問題が起きました。', 'req-s1'),
    });
    await renderApp({ fetch: server.fetch });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('サーバーの内側で問題が起きました。');
    expect(screen.queryByRole('button', { name: 'IoT ポータルでログイン' })).toBeNull();
  });
});

describe('not found', () => {
  it('an unknown path (404 from the resolver) shows the not-found page', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': () => apiError(404, 'ありません。', 'req-404'),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/unknown', lang: 'id' });
    expect(await screen.findByText('Halaman tidak ditemukan')).toBeTruthy();
  });

  it('an instance route the SPA has no screen for is not found', async () => {
    const server = mockServer(signedIn());
    await renderApp({ fetch: server.fetch, path: '/-/nothing' });
    expect(await screen.findByText('ページが見つかりません')).toBeTruthy();
    expect(server.unmatched).toEqual([]);
  });
});

describe('CSP: no injected <style> (ADR-0022 §3a)', () => {
  it('rendering pages adds no <style> element and no style attribute', async () => {
    const before = document.querySelectorAll('style').length;
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/projects': (c) =>
        c.query.get('kind') === 'spec' ? page([]) : page([PROJECT]),
      'GET /api/v1/groups': () => page([]),
    });
    await renderApp({ fetch: server.fetch });
    await screen.findByText('Forge Web');
    await waitFor(() => {
      expect(document.querySelectorAll('style').length).toBe(before);
    });
    expect(document.querySelectorAll('[style]').length).toBe(0);
  });

  it('a project page adds none either', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_PROJECT),
      'GET /api/v1/projects/7': () => json(200, PROJECT),
      'GET /api/v1/projects/7/repository/refs': () => json(200, REFS_FILLED),
    });
    const before = document.querySelectorAll('style').length;
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    await screen.findByText('Forge Web');
    expect(document.querySelectorAll('style').length).toBe(before);
    expect(document.querySelectorAll('[style]').length).toBe(0);
  });

  it('opening and closing the create dialog adds none (native <dialog>)', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/projects': () => page([]),
      'GET /api/v1/groups': () => page([]),
      'GET /api/v1/namespaces/resolve': () => apiError(404, 'x', 'r'),
    });
    const before = document.querySelectorAll('style').length;
    await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('button', { name: 'グループを作る' }));
    expect(screen.getByRole('dialog')).toBeTruthy();
    expect(document.querySelectorAll('style').length).toBe(before);
    fireEvent.click(screen.getByRole('button', { name: '閉じる' }));
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).toBeNull();
    });
    expect(document.querySelectorAll('style').length).toBe(before);
    expect(document.querySelectorAll('[style]').length).toBe(0);
  });
});
