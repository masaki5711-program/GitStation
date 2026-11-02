// MR11 review leftovers (MR12 step 0): a persistent success message for a
// protected-ref role change, the token leave guard on browser Back and its
// href handling, a repeat copy announced again, the "Token created" status
// outside the secret panel, the danger-zone heading colour.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { render } from '@testing-library/react';
import { BrowserRouter } from 'react-router';
import { afterEach, describe, expect, it } from 'vitest';

import { createApiClient } from '../src/api/client';
import { AppProviders, AppRoutes, createQueryClient } from '../src/App';
import { createI18n } from '../src/i18n';

import { mockServer, page, PROJECT, RESOLVE_PROJECT, signedIn } from './mock';
import { json, renderApp } from './render';

const user = (over: Record<string, unknown> = {}) => ({
  ...signedIn(),
  'GET /api/v1/namespaces/resolve': () =>
    json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
  ...over,
});

const tokenRoutes = () => ({
  'GET /api/v1/user/tokens': () => json(200, { items: [] }),
  'POST /api/v1/user/tokens': () =>
    json(201, {
      id: 1, public_id: 'abcd', name: 'ci', scopes: ['read_repository'], expires_at: '2027-01-30T00:00:00Z',
      is_agent: false, token: 'fgp_abcd_SYNTHETICSECRET',
    }),
});

async function showSecret() {
  const server = mockServer(user(tokenRoutes()));
  await renderApp({ fetch: server.fetch, path: '/taro/-/settings', lang: 'en' });
  await screen.findByText('You have no tokens.');
  fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'ci' } });
  fireEvent.click(screen.getByRole('button', { name: 'Create token' }));
  await screen.findByTestId('token-secret');
  return server;
}

afterEach(() => {
  Object.defineProperty(navigator, 'clipboard', { value: undefined, configurable: true });
});

const projectRoutes = () => ({
  ...signedIn(),
  'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_PROJECT),
  'GET /api/v1/projects/7': () => json(200, PROJECT),
  'GET /api/v1/projects/7/protected-refs': () => json(200, { rules: [] }),
  'GET /api/v1/groups': () => page([]),
  'GET /api/v1/projects': () => page([]),
});

describe('MR11 leftovers', () => {
  it('leaving through a link replaces the sentinel: no stray history entry, Back returns to the page', async () => {
    const server = mockServer(user(tokenRoutes()));
    window.history.replaceState(null, '', '/taro/-/settings');
    const i18n = await createI18n('en');
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
    await screen.findByText('You have no tokens.');
    const before = window.history.length;
    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'ci' } });
    fireEvent.click(screen.getByRole('button', { name: 'Create token' }));
    await screen.findByTestId('token-secret');
    // One sentinel entry on top while the secret is shown.
    expect(window.history.length).toBe(before + 1);
    fireEvent.click(screen.getByRole('link', { name: 'My sessions' }));
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Leave and lose the token' }));
    await screen.findByRole('heading', { name: 'My sessions' });
    expect(window.location.search).toBe('?tab=sessions');
    // [page, new address]: the sentinel became the new address.
    expect(window.history.length).toBe(before + 1);
    window.history.back();
    await waitFor(() => {
      expect(window.location.search).toBe('');
    });
    expect(window.location.pathname).toBe('/taro/-/settings');
  });

  it('a protected-ref role change leaves a persistent success status', async () => {
    const rule = {
      id: 1, pattern: 'refs/heads/main', push_min_role: 'maintainer',
      created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-01T00:00:00Z',
    };
    const server = mockServer({
      ...projectRoutes(),
      'GET /api/v1/projects/7/protected-refs': () => json(200, { rules: [rule] }),
      'PATCH /api/v1/projects/7/protected-refs/1': () => json(200, { ...rule, push_min_role: 'owner' }),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    const select = await screen.findByLabelText('Lowest role that may push to refs/heads/main');
    fireEvent.change(select, { target: { value: 'owner' } });
    fireEvent.click(screen.getByRole('button', { name: 'Change the role for refs/heads/main' }));
    await waitFor(() => {
      expect(server.to('PATCH', '/api/v1/projects/7/protected-refs/1')).toHaveLength(1);
    });
    await waitFor(() => {
      const status = screen.getAllByRole('status').find((s) => s.textContent.includes('was changed'));
      expect(status?.textContent).toBe('The role for refs/heads/main was changed.');
    });
  });

  it('browser Back with the secret shown asks first; staying keeps it, leaving loses it', async () => {
    await showSecret();
    window.history.back();
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText('Leave without storing the token?')).toBeTruthy();
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    expect(screen.getByTestId('token-secret')).toBeTruthy();
    // The guard is armed again: a second Back asks again.
    window.history.back();
    await waitFor(() => {
      expect(screen.getByRole('dialog')).toBeTruthy();
    });
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Leave and lose the token' }));
    await waitFor(() => {
      expect(screen.queryByTestId('token-secret')).toBeNull();
    });
    expect(document.body.textContent).not.toContain('SYNTHETICSECRET');
  });

  it('links that leave the site, open elsewhere, download or stay on the page are not intercepted', async () => {
    await showSecret();
    const make = (attrs: Record<string, string>) => {
      const a = document.createElement('a');
      for (const [k, v] of Object.entries(attrs)) a.setAttribute(k, v);
      a.textContent = 'x';
      document.body.append(a);
      // The test's own listener keeps jsdom from navigating.
      a.addEventListener('click', (e) => {
        e.preventDefault();
      });
      return a;
    };
    const click = (a: HTMLElement) => {
      a.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 }));
    };
    for (const attrs of [
      { href: 'https://other.example/x' },
      { href: '/elsewhere', target: '_blank' },
      { href: '/file.zip', download: '' },
      { href: '#top' },
    ]) {
      click(make(attrs));
      expect(screen.queryByRole('dialog')).toBeNull();
    }
    // A relative href is resolved against the page, and asks.
    click(make({ href: 'sessions?x=1' }));
    expect(await screen.findByRole('dialog')).toBeTruthy();
  });

  it('"Token created." is announced by a status outside the secret panel', async () => {
    await showSecret();
    const panel = screen.getByTestId('token-secret');
    expect(within(panel).queryAllByRole('status').filter((s) => s.textContent === 'Token created.')).toHaveLength(0);
    const outside = screen.getAllByRole('status').find((s) => s.textContent === 'Token created.');
    expect(outside).toBeTruthy();
    expect(panel.contains(outside ?? null)).toBe(false);
    fireEvent.click(screen.getByRole('button', { name: 'I have stored it' }));
    expect(outside?.textContent).toBe('');
  });

  it('copying twice announces again: the status is emptied before it is set again', async () => {
    const written: string[] = [];
    Object.defineProperty(navigator, 'clipboard', {
      value: {
        writeText: (text: string) => {
          written.push(text);
          return Promise.resolve();
        },
      },
      configurable: true,
    });
    const server = mockServer({
      ...projectRoutes(),
      'GET /api/v1/projects/7/repository/refs': () =>
        json(200, { default_branch: null, refs: [], truncated: false }),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web', lang: 'en' });
    const button = (await screen.findAllByRole('button', { name: /^Copy/ }))[0] as HTMLElement;
    const status = button.nextElementSibling as HTMLElement;
    const seen: string[] = [];
    new MutationObserver(() => {
      seen.push(status.textContent);
    }).observe(status, { childList: true, characterData: true, subtree: true });
    fireEvent.click(button);
    await waitFor(() => {
      expect(status.textContent).not.toBe('');
    });
    fireEvent.click(button);
    await waitFor(() => {
      expect(seen.filter((s) => s !== '')).toHaveLength(2);
    });
    expect(seen).toContain('');
    expect(written).toHaveLength(2);
  });

  it('the danger zone heading is in the danger text colour', async () => {
    const server = mockServer(projectRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    const heading = await screen.findByRole('heading', { name: 'Delete' });
    expect(heading.className).toMatch(/dangerTitle/);
  });
});
