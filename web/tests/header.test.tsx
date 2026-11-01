import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { SESSION, mockServer, page, signedIn } from './mock';
import { renderApp } from './render';

const home = () => ({
  ...signedIn(),
  'GET /api/v1/projects': () => page([]),
  'GET /api/v1/groups': () => page([]),
});

describe('header', () => {
  it('shows the logo, and no user menu while signed out', async () => {
    const server = mockServer({
      'GET /api/v1/session': () =>
        new Response('{"error_code":"FG-AUTH-0001","user_message":"x","request_id":"r"}', {
          status: 401,
          headers: { 'content-type': 'application/json' },
        }),
    });
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ログインしてください' });
    expect(screen.getByAltText('GitStation のホーム')).toBeTruthy();
    expect(screen.queryByRole('button', { name: /山田/ })).toBeNull();
  });

  it('the user menu names the person and signs out with a form POST carrying the CSRF token', async () => {
    const server = mockServer(home());
    const { container } = await renderApp({ fetch: server.fetch });
    const trigger = await screen.findByRole('button', { name: /山田 太郎/ });
    expect(trigger.getAttribute('aria-expanded')).toBe('false');
    fireEvent.click(trigger);
    expect(trigger.getAttribute('aria-expanded')).toBe('true');
    const form = container.querySelector('form[action="/-/auth/logout"]');
    expect(form?.getAttribute('method')).toBe('post');
    const csrf = form?.querySelector('input[name="csrf"]');
    expect(csrf?.getAttribute('type')).toBe('hidden');
    expect((csrf as HTMLInputElement).value).toBe(SESSION.csrf_token);
    expect(within(form as HTMLElement).getByRole('button', { name: 'ログアウト' })).toBeTruthy();
    expect(screen.getByText('taro')).toBeTruthy();
  });

  it('Escape closes the menu', async () => {
    const server = mockServer(home());
    await renderApp({ fetch: server.fetch });
    const trigger = await screen.findByRole('button', { name: /山田 太郎/ });
    fireEvent.click(trigger);
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(trigger.getAttribute('aria-expanded')).toBe('false');
  });

  it('changing the language re-renders and asks the API again in that language', async () => {
    window.localStorage.clear();
    const server = mockServer(home());
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム' });
    fireEvent.change(screen.getByLabelText('表示する言語'), { target: { value: 'en' } });
    expect(await screen.findByRole('heading', { name: 'Home' })).toBeTruthy();
    await waitFor(() => {
      const last = server.calls.at(-1);
      expect(last?.headers.get('accept-language')).toBe('en');
    });
    expect(window.localStorage.getItem('gitstation.lang')).toBe('en');
  });
});
