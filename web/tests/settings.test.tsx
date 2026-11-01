// MR11: settings, members, tokens, sessions, deleted projects, administration.
// Denials show one line (user_message) + request_id; the secret of a new token
// is shown once; every unsafe request carries the CSRF header; every
// destructive request asks first.
import { cleanup, fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer, type Call, type Handler, page, PROJECT, RESOLVE_GROUP, RESOLVE_PROJECT, SESSION, signedIn, GROUP } from './mock';
import { json, renderApp } from './render';
import { repoRoutes } from './repo-fixtures';

const CSRF = SESSION.csrf_token;
const BASE = '/acme/forge-web/-';
const ME = { ...RESOLVE_PROJECT };

const project = (over: Record<string, unknown> = {}) => ({
  ...signedIn(),
  'GET /api/v1/namespaces/resolve': () => json(200, ME),
  'GET /api/v1/projects/7': () => json(200, { ...PROJECT, ...over }),
  'GET /api/v1/projects/7/protected-refs': () => json(200, { rules: [] }),
  'GET /api/v1/groups': () => page([]),
  'GET /api/v1/projects': () => page([]),
});

const user = (over: Record<string, unknown> = {}) => ({
  ...signedIn(),
  'GET /api/v1/namespaces/resolve': () =>
    json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
  ...over,
});

const click = (name: string | RegExp) => {
  fireEvent.click(screen.getByRole('button', { name }));
};

describe('project settings', () => {
  it('saves the name and description with the CSRF header, only what changed', async () => {
    const server = mockServer({
      ...project(),
      'PATCH /api/v1/projects/7': () => json(200, { ...PROJECT, display_name: 'Forge Web 2' }),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/settings`, lang: 'en' });
    const box = await screen.findByLabelText('Name');
    fireEvent.change(box, { target: { value: 'Forge Web 2' } });
    fireEvent.click(screen.getAllByRole('button', { name: 'Save' })[0] as HTMLElement);
    await waitFor(() => {
      expect(server.to('PATCH', '/api/v1/projects/7')).toHaveLength(1);
    });
    const call = server.to('PATCH', '/api/v1/projects/7')[0];
    expect(call?.body).toEqual({ display_name: 'Forge Web 2' });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByText('Saved.')).toBeTruthy();
  });

  it('an empty name is refused before sending, the summary takes the focus', async () => {
    const server = mockServer(project());
    await renderApp({ fetch: server.fetch, path: `${BASE}/settings`, lang: 'en' });
    fireEvent.change(await screen.findByLabelText('Name'), { target: { value: '  ' } });
    fireEvent.click(screen.getAllByRole('button', { name: 'Save' })[0] as HTMLElement);
    const summary = await screen.findByText('Please check your input.');
    await waitFor(() => {
      expect(document.activeElement).toBe(summary.closest('[role="alert"]'));
    });
    expect(server.to('PATCH', '/api/v1/projects/7')).toHaveLength(0);
    expect(screen.getByLabelText('Name').getAttribute('aria-invalid')).toBe('true');
  });

  it('a refused visibility change shows the one-line reason and the request id only', async () => {
    const server = mockServer({
      ...project(),
      'PUT /api/v1/projects/7/visibility': () =>
        apiError(403, 'You need the Owner role to change this.', 'req-vis-1', 'FG-AUTH-0003'),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/settings`, lang: 'en' });
    await screen.findByLabelText('Name');
    fireEvent.click(screen.getByLabelText(/^Private/));
    fireEvent.click(screen.getByLabelText(/^Internal/));
    fireEvent.click(screen.getByLabelText(/^Private/));
    const forms = screen.getAllByRole('button', { name: 'Save' });
    fireEvent.click(forms[1] as HTMLElement);
    expect(await screen.findByText('You need the Owner role to change this.')).toBeTruthy();
    expect(screen.getByText('req-vis-1')).toBeTruthy();
    expect(document.body.textContent).not.toContain('FG-AUTH-0003');
    expect(server.to('PUT', '/api/v1/projects/7/visibility')[0]?.body).toEqual({ visibility: 'private' });
  });

  it('a protected ref pattern is checked first; a valid one is created; removing asks', async () => {
    const rule = (id: number, pattern: string) => ({
      id, pattern, push_min_role: 'maintainer', created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-01T00:00:00Z',
    });
    let rules = [rule(1, 'refs/heads/main')];
    const server = mockServer({
      ...project(),
      'GET /api/v1/projects/7/protected-refs': () => json(200, { rules }),
      'POST /api/v1/projects/7/protected-refs': (c) => {
        const body = c.body as { pattern: string; push_min_role: string };
        const created = rule(2, body.pattern);
        rules = [...rules, created];
        return json(201, created);
      },
      'DELETE /api/v1/projects/7/protected-refs/1': () => new Response(null, { status: 204 }),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/settings`, lang: 'en' });
    await screen.findByText('refs/heads/main');
    const field = screen.getByLabelText('Pattern');
    fireEvent.change(field, { target: { value: 'main' } });
    click('Protect a ref');
    expect(await screen.findAllByText('Start with refs/heads/ or refs/tags/.')).not.toHaveLength(0);
    expect(server.to('POST', '/api/v1/projects/7/protected-refs')).toHaveLength(0);
    fireEvent.change(field, { target: { value: 'refs/tags/v*' } });
    click('Protect a ref');
    await screen.findByText('refs/tags/v*');
    const post = server.to('POST', '/api/v1/projects/7/protected-refs')[0];
    expect(post?.body).toEqual({ pattern: 'refs/tags/v*', push_min_role: 'maintainer' });
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(CSRF);

    // Removing asks first; Cancel sends nothing.
    fireEvent.click(screen.getAllByRole('button', { name: 'Remove protection' })[0] as HTMLElement);
    const dialog = screen.getByRole('dialog');
    expect(within(dialog).getByText('Remove this protection?')).toBeTruthy();
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    expect(server.to('DELETE', '/api/v1/projects/7/protected-refs/1')).toHaveLength(0);
    fireEvent.click(screen.getAllByRole('button', { name: 'Remove protection' })[0] as HTMLElement);
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Remove protection' }));
    await waitFor(() => {
      expect(server.to('DELETE', '/api/v1/projects/7/protected-refs/1')).toHaveLength(1);
    });
    expect(server.to('DELETE', '/api/v1/projects/7/protected-refs/1')[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
  });

  it('a rate-limited rename shows the reason (FG-REPO-0018) and keeps the input', async () => {
    const server = mockServer({
      ...project(),
      'POST /api/v1/projects/7/transfer': () =>
        apiError(429, 'This was renamed or moved too often recently. Try again later.', 'req-mv-1', 'FG-REPO-0018'),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/settings`, lang: 'en' });
    const segment = await screen.findByLabelText('Last part of the path');
    // Nothing chosen: refused before sending.
    click('Move or rename');
    expect(await screen.findAllByText('Choose a new location or change the last part of the path.')).not.toHaveLength(0);
    expect(server.to('POST', '/api/v1/projects/7/transfer')).toHaveLength(0);
    // The redirect is explained before the request, not only after.
    expect(screen.getByText(/The old address keeps leading to the new one/)).toBeTruthy();
    fireEvent.change(segment, { target: { value: 'forge-web-2' } });
    click('Move or rename');
    expect(await screen.findByText('This was renamed or moved too often recently. Try again later.')).toBeTruthy();
    expect(screen.getByText('req-mv-1')).toBeTruthy();
    expect(screen.getByLabelText<HTMLInputElement>('Last part of the path').value).toBe('forge-web-2');
    expect(server.to('POST', '/api/v1/projects/7/transfer')[0]?.body).toEqual({
      reclaim: false,
      path: 'forge-web-2',
    });
  });

  it('a successful rename goes to the new address and says where it was', async () => {
    const server = mockServer({
      ...project(),
      'POST /api/v1/projects/7/transfer': () =>
        json(200, {
          id: 7, kind: 'project', namespace_id: 11, old_path: 'acme/forge-web', path: 'acme/forge-web-2',
          projects: 1, reclaimed_tombstones: 0,
        }),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/settings`, lang: 'en' });
    fireEvent.change(await screen.findByLabelText('Last part of the path'), { target: { value: 'forge-web-2' } });
    click('Move or rename');
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe('/acme/forge-web-2/-/settings');
    });
  });

  it('deleting asks first, then goes to the deleted projects with the CSRF header', async () => {
    const server = mockServer({
      ...project(),
      ...user(),
      'GET /api/v1/namespaces/resolve': (c) =>
        c.query.get('path') === 'taro'
          ? json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 })
          : json(200, ME),
      'DELETE /api/v1/projects/7': () =>
        json(200, { id: 7, deleted_at: '2026-11-01T00:00:00Z', purge_after: '2026-11-08T00:00:00Z' }),
      'GET /api/v1/projects/7': () => json(200, PROJECT),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/settings`, lang: 'en' });
    await screen.findByLabelText('Name');
    click('Delete this project');
    const dialog = screen.getByRole('dialog');
    expect(within(dialog).getByText('Delete this project?')).toBeTruthy();
    expect(document.activeElement).toBe(within(dialog).getByRole('button', { name: 'Cancel' }));
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    expect(server.to('DELETE', '/api/v1/projects/7')).toHaveLength(0);
    click('Delete this project');
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Delete this project' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe('/taro/-/settings?tab=trash');
    });
    expect(server.to('DELETE', '/api/v1/projects/7')[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
  });
});

describe('members', () => {
  const member = (id: number, name: string, role: string) => ({
    user_id: id, username: name, display_name: name.toUpperCase(), role, active: true, created_at: '2026-10-01T00:00:00Z',
  });

  it('adds a found user with the chosen role, changes a role and removes after asking', async () => {
    let members = [member(5, 'taro', 'owner')];
    const server = mockServer({
      ...project(),
      'GET /api/v1/projects/7/members': () => json(200, { members, next_after: null }),
      'GET /api/v1/users': () => json(200, { items: [{ id: 9, username: 'hanako', display_name: 'HANAKO' }, { id: 5, username: 'taro', display_name: 'TARO' }] }),
      'POST /api/v1/projects/7/members': (c) => {
        const b = c.body as { user_id: number; role: string };
        members = [...members, member(b.user_id, 'hanako', b.role)];
        return json(201, members[1]);
      },
      'PATCH /api/v1/projects/7/members/9': (c) => {
        members = [members[0] as (typeof members)[0], { ...(members[1] as (typeof members)[0]), role: (c.body as { role: string }).role }];
        return json(200, members[1]);
      },
      'DELETE /api/v1/projects/7/members/9': () => {
        members = [members[0] as (typeof members)[0]];
        return new Response(null, { status: 204 });
      },
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/members`, lang: 'en' });
    await screen.findByText('TARO');
    // Too short a search is refused before sending.
    fireEvent.change(screen.getByLabelText('Find a user'), { target: { value: 'h' } });
    click('Search');
    expect(await screen.findAllByText('Enter 2 to 64 characters.')).not.toHaveLength(0);
    expect(server.to('GET', '/api/v1/users')).toHaveLength(0);
    fireEvent.change(screen.getByLabelText('Find a user'), { target: { value: 'ha' } });
    fireEvent.change(screen.getAllByLabelText('Role')[0] as HTMLElement, { target: { value: 'developer' } });
    click('Search');
    expect(await screen.findByText('Already a member')).toBeTruthy();
    click('Add HANAKO');
    expect(await screen.findByText('HANAKO was added.')).toBeTruthy();
    const post = server.to('POST', '/api/v1/projects/7/members')[0];
    expect(post?.body).toEqual({ user_id: 9, role: 'developer' });
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(CSRF);

    // Change the role.
    const select = await screen.findByLabelText('Role of HANAKO');
    fireEvent.change(select, { target: { value: 'maintainer' } });
    const row = select.closest('tr') as HTMLElement;
    fireEvent.click(within(row).getByRole('button', { name: 'Change role' }));
    await waitFor(() => {
      expect(server.to('PATCH', '/api/v1/projects/7/members/9')[0]?.body).toEqual({ role: 'maintainer' });
    });

    // Remove: asks, Cancel sends nothing, Confirm sends.
    const row2 = (await screen.findByLabelText('Role of HANAKO')).closest('tr') as HTMLElement;
    fireEvent.click(within(row2).getByRole('button', { name: 'Remove' }));
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Cancel' }));
    expect(server.to('DELETE', '/api/v1/projects/7/members/9')).toHaveLength(0);
    fireEvent.click(within(row2).getByRole('button', { name: 'Remove' }));
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Remove' }));
    await waitFor(() => {
      expect(server.to('DELETE', '/api/v1/projects/7/members/9')).toHaveLength(1);
    });
    expect(server.to('DELETE', '/api/v1/projects/7/members/9')[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    await waitFor(() => {
      expect(screen.queryByLabelText('Role of HANAKO')).toBeNull();
    });
  });

  it('a membership conflict (FG-REPO-0010) is shown as its one-line reason in the dialog', async () => {
    const server = mockServer({
      ...project(),
      'GET /api/v1/projects/7/members': () => json(200, { members: [member(5, 'taro', 'owner')], next_after: null }),
      'DELETE /api/v1/projects/7/members/5': () =>
        apiError(409, 'The membership change does not fit the current members. Refresh and check.', 'req-mem-1', 'FG-REPO-0010'),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/members`, lang: 'en' });
    const row = (await screen.findByLabelText('Role of TARO')).closest('tr') as HTMLElement;
    fireEvent.click(within(row).getByRole('button', { name: 'Remove' }));
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Remove' }));
    const dialog = screen.getByRole('dialog');
    expect(await within(dialog).findByText(/does not fit the current members/)).toBeTruthy();
    expect(within(dialog).getByText('req-mem-1')).toBeTruthy();
    expect(document.body.textContent).not.toContain('FG-REPO-0010');
  });

  it('group members and settings use the group endpoints', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_GROUP),
      'GET /api/v1/groups/3': () => json(200, GROUP),
      'GET /api/v1/groups/3/members': () => json(200, { members: [member(5, 'taro', 'owner')], next_after: null }),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/-/members', lang: 'en' });
    expect(await screen.findByLabelText('Role of TARO')).toBeTruthy();
    expect(server.unmatched).toEqual([]);
  });
});

describe('access tokens', () => {
  const tokenRoutes = () => {
    const items: unknown[] = [];
    return {
      items,
      routes: {
        'GET /api/v1/user/tokens': () => json(200, { items }),
        'POST /api/v1/user/tokens': () => {
          items.push({
            id: 1, public_id: 'abcd', name: 'ci', scopes: ['read_repository'], created_at: '2026-11-01T00:00:00Z',
            expires_at: '2027-01-30T00:00:00Z', revoked_at: null, last_used_at: null,
          });
          return json(201, {
            id: 1, public_id: 'abcd', name: 'ci', scopes: ['read_repository'], expires_at: '2027-01-30T00:00:00Z',
            is_agent: false, token: 'fgp_abcd_SYNTHETICSECRET',
          });
        },
      },
    };
  };

  it('creating shows the secret once; it is gone after "I have stored it", a tab change and a reload', async () => {
    const { routes } = tokenRoutes();
    const server = mockServer(user(routes));
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings', lang: 'en' });
    // Validation first: nothing sent.
    await screen.findByText('You have no tokens.');
    click('Create token');
    expect(await screen.findAllByText('Enter a name.')).not.toHaveLength(0);
    expect(server.to('POST', '/api/v1/user/tokens')).toHaveLength(0);
    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'ci' } });
    fireEvent.change(screen.getByLabelText('Valid for (days)'), { target: { value: '400' } });
    click('Create token');
    expect(await screen.findAllByText('Enter a whole number from 1 to 365.')).not.toHaveLength(0);
    fireEvent.change(screen.getByLabelText('Valid for (days)'), { target: { value: '30' } });
    click('Create token');

    const panel = await screen.findByTestId('token-secret');
    expect(within(panel).getByTestId('token-value').textContent).toBe('fgp_abcd_SYNTHETICSECRET');
    expect(within(panel).getByText(/only time the token is shown/)).toBeTruthy();
    expect(within(panel).getByText(/gstn auth login --with-token/)).toBeTruthy();
    expect(document.activeElement).toBe(panel);
    const post = server.to('POST', '/api/v1/user/tokens')[0];
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(post?.body).toEqual({ name: 'ci', scopes: ['read_repository'], expires_in_days: 30, is_agent: false });

    // The list never has the secret.
    expect(await screen.findByRole('row', { name: /ci/ })).toBeTruthy();
    click('I have stored it');
    expect(screen.queryByTestId('token-secret')).toBeNull();
    expect(document.body.textContent).not.toContain('SYNTHETICSECRET');
  });

  it('the secret is not there after navigating away and back, nor after a reload', async () => {
    const { routes } = tokenRoutes();
    const server = mockServer(user(routes));
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings', lang: 'en' });
    await screen.findByText('You have no tokens.');
    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'ci' } });
    click('Create token');
    await screen.findByTestId('token-secret');
    // Another tab of the account, then back.
    fireEvent.click(screen.getByRole('link', { name: 'My sessions' }));
    expect(screen.queryByTestId('token-secret')).toBeNull();
    expect(document.body.textContent).not.toContain('SYNTHETICSECRET');
    fireEvent.click(screen.getByRole('link', { name: 'Access tokens' }));
    await screen.findByRole('heading', { name: 'Access tokens' });
    expect(screen.queryByTestId('token-secret')).toBeNull();
    expect(document.body.textContent).not.toContain('SYNTHETICSECRET');
    // A reload is a new app.
    cleanup();
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings', lang: 'en' });
    await screen.findByRole('heading', { name: 'Access tokens' });
    expect(document.body.textContent).not.toContain('SYNTHETICSECRET');
    expect(screen.queryByTestId('token-secret')).toBeNull();
  });

  it('revoking asks first and sends with the CSRF header; a refusal is shown in the dialog', async () => {
    const items = [
      { id: 4, public_id: 'zz', name: 'deploy', scopes: ['api'], created_at: '2026-10-01T00:00:00Z', expires_at: '2099-01-01T00:00:00Z', revoked_at: null, last_used_at: null },
    ];
    let refuse = true;
    const server = mockServer(
      user({
        'GET /api/v1/user/tokens': () => json(200, { items }),
        'DELETE /api/v1/user/tokens/4': () => {
          if (refuse) return apiError(403, 'Not allowed from a token.', 'req-tok-1', 'FG-AUTH-0002');
          items.length = 0;
          return new Response(null, { status: 204 });
        },
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings', lang: 'en' });
    fireEvent.click(await screen.findByRole('button', { name: 'Revoke deploy' }));
    const dialog = screen.getByRole('dialog');
    expect(within(dialog).getByText('Revoke this token?')).toBeTruthy();
    fireEvent.click(within(dialog).getByRole('button', { name: 'Revoke' }));
    expect(await within(dialog).findByText('Not allowed from a token.')).toBeTruthy();
    refuse = false;
    fireEvent.click(within(dialog).getByRole('button', { name: 'Revoke' }));
    await waitFor(() => {
      expect(screen.queryByRole('dialog', { hidden: false })).toBeNull();
    });
    expect(server.to('DELETE', '/api/v1/user/tokens/4')[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
  });

  it('only an administrator is offered the admin scope', async () => {
    const server = mockServer(user({ 'GET /api/v1/user/tokens': () => json(200, { items: [] }) }));
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings', lang: 'en' });
    await screen.findByText('You have no tokens.');
    expect(screen.queryByLabelText('admin')).toBeNull();
    expect(screen.getByLabelText('read_repository')).toBeTruthy();
  });
});

describe('sessions', () => {
  const sessions = [
    { public_id: '0123456789abcdef', created_at: '2026-11-01T00:00:00Z', last_seen_at: '2026-11-01T01:00:00Z', idle_expires_at: '2026-11-01T05:00:00Z', absolute_expires_at: '2026-11-02T00:00:00Z', oidc: true, current: true },
    { public_id: 'fedcba9876543210', created_at: '2026-10-31T00:00:00Z', last_seen_at: '2026-10-31T01:00:00Z', idle_expires_at: '2026-11-01T05:00:00Z', absolute_expires_at: '2026-11-02T00:00:00Z', oidc: true, current: false },
  ];

  it('lists, asks, and revokes another session by its public id', async () => {
    const server = mockServer(
      user({
        'GET /api/v1/user/sessions': () => json(200, { items: sessions, truncated: false }),
        'DELETE /api/v1/user/sessions/fedcba9876543210': () => new Response(null, { status: 204 }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings?tab=sessions', lang: 'en' });
    await screen.findByText('This session');
    const buttons = screen.getAllByRole('button', { name: /^End session started/ });
    fireEvent.click(buttons[1] as HTMLElement);
    const dialog = screen.getByRole('dialog');
    expect(within(dialog).getByText('That browser will be signed out.')).toBeTruthy();
    fireEvent.click(within(dialog).getByRole('button', { name: 'End session' }));
    await waitFor(() => {
      expect(server.to('DELETE', '/api/v1/user/sessions/fedcba9876543210')).toHaveLength(1);
    });
    expect(server.to('DELETE', '/api/v1/user/sessions/fedcba9876543210')[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
  });

  it('ending the current session warns that you will be signed out', async () => {
    const server = mockServer(
      user({ 'GET /api/v1/user/sessions': () => json(200, { items: sessions, truncated: false }) }),
    );
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings?tab=sessions', lang: 'en' });
    await screen.findByText('This session');
    fireEvent.click(screen.getAllByRole('button', { name: /^End session started/ })[0] as HTMLElement);
    expect(within(screen.getByRole('dialog')).getByText(/You will be signed out/)).toBeTruthy();
  });
});

describe('deleted projects', () => {
  it('lists pending deletions and restores one', async () => {
    let items = [{ ...PROJECT, id: 7 }];
    const server = mockServer(
      user({
        'GET /api/v1/projects': (c: Call) => {
          expect(c.query.get('state')).toBe('pending_deletion');
          return page(items);
        },
        'POST /api/v1/projects/7/restore': () => {
          items = [];
          return json(200, PROJECT);
        },
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings?tab=trash', lang: 'en' });
    await screen.findByText('Waiting for deletion');
    click('Restore Forge Web');
    await screen.findByText('No project is waiting for deletion.');
    expect(server.to('POST', '/api/v1/projects/7/restore')[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
  });

  it('a refused restore (FG-REPO-0019) shows its reason', async () => {
    const server = mockServer(
      user({
        'GET /api/v1/projects': () => page([PROJECT]),
        'POST /api/v1/projects/7/restore': () => apiError(409, 'This project cannot be restored.', 'req-rs-1', 'FG-REPO-0019'),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings?tab=trash', lang: 'en' });
    await screen.findByText('Waiting for deletion');
    click('Restore Forge Web');
    expect(await screen.findByText('This project cannot be restored.')).toBeTruthy();
    expect(screen.getByText('req-rs-1')).toBeTruthy();
  });
});

describe('administration', () => {
  const adminSession = { ...SESSION, user: { ...SESSION.user, is_admin: true } };
  const users = [
    { id: 5, username: 'taro', display_name: 'TARO', kind: 'human', state: 'active', is_admin: true, created_at: '2026-09-01T00:00:00Z' },
    { id: 9, username: 'hanako', display_name: 'HANAKO', kind: 'human', state: 'active', is_admin: false, created_at: '2026-09-02T00:00:00Z' },
  ];
  const admin = (over: Record<string, Handler> = {}) =>
    user({
      'GET /api/v1/session': () => json(200, adminSession),
      'GET /api/v1/admin/users': (c: Call) => {
        const state = c.query.get('state');
        return json(200, {
          items: state === 'disabled' ? [] : users,
          next_cursor: null,
          has_more: false,
        });
      },
      ...over,
    });

  it('a person who is not an administrator has no admin tab, and ?tab=admin shows the tokens', async () => {
    const server = mockServer(user({ 'GET /api/v1/user/tokens': () => json(200, { items: [] }) }));
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings?tab=admin', lang: 'en' });
    await screen.findByRole('heading', { name: 'Access tokens' });
    expect(screen.queryByRole('link', { name: 'Administration' })).toBeNull();
    expect(server.to('GET', '/api/v1/admin/users')).toHaveLength(0);
  });

  it('filters by state, disables after asking, and shows the reason when the guard refuses', async () => {
    const server = mockServer(
      admin({
        'POST /api/v1/admin/users/5/disable': () =>
          apiError(409, 'This was refused because it would leave no administrator or group Owner. Contact the administrator with the request ID.', 'req-ad-1', 'FG-AUTH-0009'),
        'POST /api/v1/admin/users/9/disable': () => json(200, { user_id: 9, outcome: 'disabled' }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings?tab=admin', lang: 'en' });
    await screen.findByText('HANAKO');
    fireEvent.change(screen.getByLabelText('Show'), { target: { value: 'disabled' } });
    await screen.findByText('No users match.');
    expect(server.to('GET', '/api/v1/admin/users').some((c) => c.query.get('state') === 'disabled')).toBe(true);
    fireEvent.change(screen.getByLabelText('Show'), { target: { value: '' } });
    await screen.findByText('HANAKO');

    // The guard's refusal (disabling yourself): one line + request id, no "force".
    click('Disable TARO');
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Disable' }));
    const dialog = screen.getByRole('dialog');
    expect(await within(dialog).findByText(/would leave no administrator or group Owner/)).toBeTruthy();
    expect(within(dialog).getByText('req-ad-1')).toBeTruthy();
    expect(document.body.textContent).not.toMatch(/force/i);
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));

    // Cancel sends nothing; confirm sends with the CSRF header.
    click('Disable HANAKO');
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Cancel' }));
    expect(server.to('POST', '/api/v1/admin/users/9/disable')).toHaveLength(0);
    click('Disable HANAKO');
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Disable' }));
    expect(await screen.findByText('HANAKO was disabled.')).toBeTruthy();
    expect(server.to('POST', '/api/v1/admin/users/9/disable')[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
  });

  it('ends all sessions of a user after asking', async () => {
    const server = mockServer(
      admin({
        'DELETE /api/v1/admin/users/9/sessions': () => json(200, { user_id: 9, revoked_sessions: 3 }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings?tab=admin', lang: 'en' });
    await screen.findByText('HANAKO');
    click('End sessions of HANAKO');
    expect(within(screen.getByRole('dialog')).getByText('End all sessions of this user?')).toBeTruthy();
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'End sessions' }));
    expect(await screen.findByText('Ended 3 session(s) of HANAKO.')).toBeTruthy();
    expect(server.to('DELETE', '/api/v1/admin/users/9/sessions')[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
  });
});

describe('who may see what', () => {
  it("someone else's account settings are not found", async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': () =>
        json(200, { canonical_path: 'hanako', kind: 'user', moved: false, namespace_id: 56, user_id: 9 }),
    });
    await renderApp({ fetch: server.fetch, path: '/hanako/-/settings', lang: 'en' });
    expect(await screen.findByRole('heading', { name: 'Page not found' })).toBeTruthy();
  });

  it('every screen is reachable without a tab-less dead end: the user menu links to the account', async () => {
    const server = mockServer({ ...repoRoutes(), ...signedIn() });
    await renderApp({ fetch: server.fetch, path: '/', lang: 'en' });
    const link = await screen.findByRole('link', { name: 'Account settings', hidden: true });
    expect(link.getAttribute('href')).toBe('/taro/-/settings');
  });
});
