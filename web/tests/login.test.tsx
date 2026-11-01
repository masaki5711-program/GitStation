import { fireEvent, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { loginUrl, safeNext } from '../src/features/signin';
import { apiError, mockServer } from './mock';
import { renderApp } from './render';

const signedOut = () => apiError(401, 'ログインが必要です。', 'req-401', 'FG-AUTH-0001');

/** `Response.type` cannot be set by a constructor: this is what a browser
 * gives for `fetch(url, { redirect: 'manual' })` answered by a 302. */
function opaqueRedirect(): Response {
  const response = new Response(null, { status: 200 });
  Object.defineProperty(response, 'type', { value: 'opaqueredirect' });
  return response;
}

describe('login screen', () => {
  it('without a session shows the one sign-in button in the page asked for', async () => {
    const server = mockServer({ 'GET /api/v1/session': signedOut });
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    expect(await screen.findByRole('heading', { name: 'ログインしてください' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'IoT ポータルでログイン' })).toBeTruthy();
    // The address stays the page asked for (`next` brings the person back).
    expect(screen.getByTestId('location').textContent).toBe('/acme/forge-web');
    // Nothing else is asked of the API while signed out.
    expect(server.calls.map((c) => c.path)).toEqual(['/api/v1/session']);
    expect(screen.queryByRole('button', { name: /ログアウト/ })).toBeNull();
  });

  it('starts the sign-in with next = the current page, then leaves for the server route', async () => {
    const assign = vi.fn();
    const server = mockServer({
      'GET /api/v1/session': signedOut,
      'GET /-/auth/login': opaqueRedirect,
    });
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web', assign });
    fireEvent.click(await screen.findByRole('button', { name: 'IoT ポータルでログイン' }));
    await waitFor(() => {
      expect(assign).toHaveBeenCalledTimes(1);
    });
    expect(assign).toHaveBeenCalledWith('/-/auth/login?next=%2Facme%2Fforge-web');
    const probe = server.to('GET', '/-/auth/login')[0];
    expect(probe?.query.get('next')).toBe('/acme/forge-web');
  });

  it('FG-AUTH-0018: says sign-in is not configured, with the request id, without the code', async () => {
    const assign = vi.fn();
    const server = mockServer({
      'GET /api/v1/session': signedOut,
      'GET /-/auth/login': () =>
        apiError(503, 'raw server text', 'req-nc-1', 'FG-AUTH-0018'),
    });
    const { container } = await renderApp({ fetch: server.fetch, assign });
    fireEvent.click(await screen.findByRole('button', { name: 'IoT ポータルでログイン' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('ログインがまだ設定されていません');
    expect(alert.textContent).toContain('管理者に連絡してください');
    expect(screen.getByTestId('request-id').textContent).toBe('req-nc-1');
    expect(container.textContent).not.toContain('FG-AUTH-0018');
    expect(container.textContent).not.toContain('raw server text');
    expect(assign).not.toHaveBeenCalled();
  });

  it('another failure shows user_message + request_id and can be retried', async () => {
    let n = 0;
    const server = mockServer({
      'GET /api/v1/session': signedOut,
      'GET /-/auth/login': () => {
        n += 1;
        return apiError(429, '少し待ってからお試しください。', `req-rl-${n}`, 'FG-WEB-0010');
      },
    });
    const { container } = await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('button', { name: 'IoT ポータルでログイン' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('少し待ってからお試しください。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-rl-1');
    expect(container.textContent).not.toContain('FG-WEB-0010');
    fireEvent.click(screen.getByRole('button', { name: 'もう一度試す' }));
    await waitFor(() => {
      expect(screen.getByTestId('request-id').textContent).toBe('req-rl-2');
    });
  });

  it('a network failure while starting is shown, not swallowed', async () => {
    const server = mockServer({
      'GET /api/v1/session': signedOut,
      'GET /-/auth/login': () => Promise.reject(new TypeError('Failed to fetch')),
    });
    await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('button', { name: 'IoT ポータルでログイン' }));
    expect((await screen.findByRole('alert')).textContent).toContain(
      'サーバーにつながりませんでした',
    );
  });

  it('is complete in en and id', async () => {
    const server = mockServer({ 'GET /api/v1/session': signedOut });
    const { unmount } = await renderApp({ fetch: server.fetch, lang: 'en' });
    expect(await screen.findByRole('button', { name: 'Sign in with the IoT portal' })).toBeTruthy();
    unmount();
    await renderApp({ fetch: server.fetch, lang: 'id' });
    expect(await screen.findByRole('button', { name: 'Masuk dengan portal IoT' })).toBeTruthy();
  });

  it('a session that ends in the middle of a page offers sign-in again', async () => {
    const server = mockServer({
      'GET /api/v1/session': () =>
        new Response(
          JSON.stringify({
            absolute_expires_at: 'x',
            idle_expires_at: 'x',
            public_id: 'p',
            csrf_token: 'c'.repeat(64),
            user: { id: 1, username: 'u', display_name: 'U', is_admin: false },
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        ),
      'GET /api/v1/projects': signedOut,
      'GET /api/v1/groups': signedOut,
    });
    await renderApp({ fetch: server.fetch });
    expect((await screen.findAllByText('ログインの有効期限が切れました')).length).toBeGreaterThan(0);
    expect(screen.getAllByRole('button', { name: 'IoT ポータルでログイン' }).length).toBeGreaterThan(0);
  });
});

describe('next', () => {
  it('only a path of this app is kept', () => {
    expect(safeNext('/acme/forge-web?x=1')).toBe('/acme/forge-web?x=1');
    expect(safeNext('https://evil.example/')).toBe('/');
    expect(safeNext('//evil.example/')).toBe('/');
    expect(safeNext('/\\evil')).toBe('/');
    expect(safeNext('/a\nb')).toBe('/');
    expect(loginUrl('/a b')).toBe('/-/auth/login?next=%2Fa%20b');
  });
});
