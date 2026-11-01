import { screen, waitFor } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { fakeFetch, json, renderApp } from './render';

const PROJECT = {
  id: 7,
  namespace_id: 11,
  path: 'acme/forge-web',
  name: 'forge-web',
  display_name: 'Forge Web',
  description: '',
  visibility: 'internal',
  kind: 'system',
  created_at: '2026-10-01T00:00:00Z',
  role: 'developer',
};

describe('home', () => {
  it('lists the first page of GET /api/v1/projects, in the UI language', async () => {
    const { fetch, calls } = fakeFetch(() =>
      json(200, { items: [PROJECT], has_more: false, next_cursor: null }),
    );
    await renderApp({ fetch, lang: 'en' });
    expect(await screen.findByText('Forge Web')).toBeTruthy();
    expect(screen.getByText('acme/forge-web')).toBeTruthy();
    expect(screen.getByText('Internal')).toBeTruthy();
    expect(calls).toHaveLength(1);
    expect(new URL(calls[0]?.url ?? '').pathname).toBe('/api/v1/projects');
    expect(calls[0]?.headers.get('accept-language')).toBe('en');
  });

  it('a 401 shows "not signed in", not an error', async () => {
    const { fetch } = fakeFetch(() =>
      json(401, {
        error_code: 'FG-AUTH-0001',
        user_message: 'ログインが必要です。',
        request_id: 'req-401',
      }),
    );
    await renderApp({ fetch });
    expect(await screen.findByText('ログインしていません。')).toBeTruthy();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('a failure shows user_message and request_id only (never error_code)', async () => {
    const { fetch } = fakeFetch(() =>
      json(503, {
        error_code: 'FG-DB-0001',
        user_message: 'いまは処理できません。',
        request_id: 'req-abc123',
      }),
    );
    const { container } = await renderApp({ fetch });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('いまは処理できません。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-abc123');
    expect(container.textContent).not.toContain('FG-DB-0001');
    expect(container.textContent).not.toContain('503');
  });

  it('a body that is not the error contract is shown as unexpected, never raw', async () => {
    const { fetch } = fakeFetch(
      () =>
        new Response('<html>proxy error: upstream 10.0.0.1</html>', {
          status: 502,
          headers: { 'x-request-id': 'req-proxy' },
        }),
    );
    const { container } = await renderApp({ fetch });
    await screen.findByRole('alert');
    expect(container.textContent).toContain('思いがけない応答がありました');
    expect(container.textContent).not.toContain('proxy error');
    expect(screen.getByTestId('request-id').textContent).toBe('req-proxy');
  });

  it('a network failure shows the network message without a request id', async () => {
    const { fetch } = fakeFetch(() => Promise.reject(new TypeError('Failed to fetch')));
    await renderApp({ fetch });
    await screen.findByRole('alert');
    expect(screen.getByText(/サーバーにつながりませんでした/)).toBeTruthy();
    expect(screen.queryByTestId('request-id')).toBeNull();
  });

  it('an unknown path shows the not-found page', async () => {
    const { fetch } = fakeFetch(() => json(200, { items: [], has_more: false, next_cursor: null }));
    await renderApp({ fetch, path: '/acme/unknown/-/nothing', lang: 'id' });
    expect(await screen.findByText('Halaman tidak ditemukan')).toBeTruthy();
  });
});

describe('CSP: no injected <style> (ADR-0022 §3a)', () => {
  it('rendering the app adds no <style> element', async () => {
    const before = document.querySelectorAll('style').length;
    const { fetch } = fakeFetch(() =>
      json(200, { items: [PROJECT], has_more: true, next_cursor: 'c' }),
    );
    await renderApp({ fetch });
    await screen.findByText('Forge Web');
    await waitFor(() => {
      expect(document.querySelectorAll('style').length).toBe(before);
    });
    expect(document.querySelectorAll('[style]').length).toBe(0);
  });
});
