// Service accounts (M3 S0 UI): administrators only. List, create, disable, the
// tokens of one account (the secret is shown once), revoke, and the check
// grants. Every refusal is the server's one line + request_id; every unsafe
// request carries the CSRF header; a change refreshes the list and the item.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer, type Handler, SESSION } from './mock';
import { json, renderApp } from './render';

const CSRF = SESSION.csrf_token;
const ADMIN = { ...SESSION, user: { ...SESSION.user, is_admin: true } };
const LIST = '/api/v1/admin/service-accounts';
const ONE = `${LIST}/3`;
const PATH = '/taro/-/settings?tab=service-accounts';

const account = (over: Record<string, unknown> = {}) => ({
  id: 3,
  username: 'sa-ci-runner',
  display_name: 'CI runner',
  purpose: 'Reports CI results',
  state: 'active',
  active_tokens: 1,
  last_token_expiry: '2027-01-30T00:00:00Z',
  created_at: '2026-10-01T00:00:00Z',
  created_by: 5,
  owner: { id: 5, username: 'taro', active: true },
  ...over,
});
const token = (over: Record<string, unknown> = {}) => ({
  id: 11,
  public_id: 'abcd1234',
  name: 'ci',
  scopes: ['read_repository'],
  created_at: '2026-11-01T00:00:00Z',
  expires_at: '2027-01-30T00:00:00Z',
  last_used_at: null,
  revoked_at: null,
  issued_by_user_id: 5,
  ...over,
});

function backend(over: Record<string, Handler> = {}) {
  const state = { accounts: [account()], tokens: [token()] };
  const routes: Record<string, Handler> = {
    'GET /api/v1/session': () => json(200, ADMIN),
    'GET /api/v1/namespaces/resolve': () =>
      json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
    'GET /api/v1/user/tokens': () => json(200, { items: [] }),
    [`GET ${LIST}`]: () => json(200, { items: state.accounts, truncated: false }),
    [`GET ${ONE}`]: () => json(200, state.accounts[0]),
    [`GET ${ONE}/tokens`]: () => json(200, { items: state.tokens }),
    [`GET ${ONE}/check-grants`]: () => json(200, { grants: [{ project_id: 7, namespace: 'ci' }] }),
    ...over,
  };
  return { state, server: mockServer(routes) };
}

const click = (name: string | RegExp) => {
  fireEvent.click(screen.getByRole('button', { name }));
};

/** Open the list, then one account, so the list is cached when the change happens. */
async function openFromList(server: ReturnType<typeof backend>['server']) {
  await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
  await screen.findByRole('row', { name: /CI runner/ });
  fireEvent.click(screen.getByRole('link', { name: 'Manage tokens of CI runner' }));
  await screen.findByRole('heading', { name: 'CI runner' });
}

/** Back to the list: it was marked stale by the change, so it is read again. */
async function backToListReadAgain(server: ReturnType<typeof backend>['server']) {
  fireEvent.click(screen.getByRole('link', { name: 'Back to service accounts' }));
  await screen.findByRole('row', { name: /CI runner/ });
  await waitFor(() => {
    expect(server.to('GET', LIST).length).toBeGreaterThanOrEqual(2);
  });
}

describe('service accounts: who sees them', () => {
  it('a person who is not an administrator has no tab, and ?tab=service-accounts shows the tokens', async () => {
    const server = mockServer({
      'GET /api/v1/session': () => json(200, SESSION),
    'GET /api/v1/namespaces/resolve': () =>
      json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
      'GET /api/v1/user/tokens': () => json(200, { items: [] }),
    });
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    await screen.findByText('You have no tokens.');
    expect(screen.queryByRole('link', { name: 'Service accounts' })).toBeNull();
    expect(server.to('GET', LIST)).toHaveLength(0);
  });

  it('an administrator has the tab and the list', async () => {
    const { server } = backend();
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings', lang: 'en' });
    const tab = await screen.findByRole('link', { name: 'Service accounts' });
    fireEvent.click(tab);
    const row = await screen.findByRole('row', { name: /CI runner/ });
    expect(within(row).getByText('sa-ci-runner')).toBeTruthy();
    expect(within(row).getByText('Reports CI results')).toBeTruthy();
    expect(within(row).getByText('taro')).toBeTruthy();
  });

  it('a refused list shows the reason and the request id', async () => {
    const { server } = backend({
      [`GET ${LIST}`]: () => apiError(500, 'Something went wrong on the server.', 'req-sa-list', 'FG-WEB-0001'),
    });
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    expect(await screen.findByText('Something went wrong on the server.')).toBeTruthy();
    expect(screen.getByText('req-sa-list')).toBeTruthy();
  });
});

describe('service accounts: create and disable', () => {
  it('validates first, then sends the account owned by the administrator and refreshes the list', async () => {
    const { server, state } = backend({
      [`POST ${LIST}`]: () => {
        const made = account({ id: 4, username: 'sa-kanflow', display_name: 'Kanflow', purpose: 'PM sync', active_tokens: 0 });
        state.accounts.push(made);
        return json(201, made);
      },
    });
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    await screen.findByRole('row', { name: /CI runner/ });
    click('Create service account');
    expect((await screen.findAllByText('Enter a username.')).length).toBeGreaterThan(0);
    expect(server.to('POST', LIST)).toHaveLength(0);
    fireEvent.change(screen.getByLabelText('Username'), { target: { value: 'sa-kanflow' } });
    fireEvent.change(screen.getByLabelText('Display name'), { target: { value: 'Kanflow' } });
    click('Create service account');
    expect((await screen.findAllByText('Describe what it is for.')).length).toBeGreaterThan(0);
    fireEvent.change(screen.getByLabelText('Description'), { target: { value: 'PM sync' } });
    click('Create service account');
    await waitFor(() => {
      expect(server.to('POST', LIST)).toHaveLength(1);
    });
    const post = server.to('POST', LIST)[0];
    expect(post?.body).toEqual({ username: 'sa-kanflow', display_name: 'Kanflow', purpose: 'PM sync', owner_id: 5 });
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByRole('row', { name: /Kanflow/ })).toBeTruthy();
    expect(await screen.findByText('The service account Kanflow was created.')).toBeTruthy();
    expect(server.to('GET', LIST).length).toBeGreaterThanOrEqual(2);
  });

  it('a refusal reads as the server line with the request id', async () => {
    const { server } = backend({
      [`POST ${LIST}`]: () => apiError(409, 'That username is already in use.', 'req-sa-dup', 'FG-REPO-0003'),
    });
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    await screen.findByRole('row', { name: /CI runner/ });
    fireEvent.change(screen.getByLabelText('Username'), { target: { value: 'sa-ci-runner' } });
    fireEvent.change(screen.getByLabelText('Description'), { target: { value: 'dup' } });
    click('Create service account');
    expect(await screen.findByText('That username is already in use.')).toBeTruthy();
    expect(screen.getByText('req-sa-dup')).toBeTruthy();
  });

  it('disabling asks first, posts to the user disable route and refreshes the list and the item', async () => {
    const { server, state } = backend({
      'POST /api/v1/admin/users/3/disable': () => {
        state.accounts[0] = account({ state: 'disabled', active_tokens: 0 });
        return json(200, { user_id: 3, outcome: 'disabled' });
      },
    });
    await openFromList(server);
    click('Disable CI runner');
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(/tokens are revoked/)).toBeTruthy();
    expect(server.to('POST', '/api/v1/admin/users/3/disable')).toHaveLength(0);
    fireEvent.click(within(dialog).getByRole('button', { name: 'Disable' }));
    await waitFor(() => {
      expect(server.to('POST', '/api/v1/admin/users/3/disable')).toHaveLength(1);
    });
    expect(server.to('POST', '/api/v1/admin/users/3/disable')[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByText(/is disabled/)).toBeTruthy();
    expect(server.to('GET', ONE).length).toBeGreaterThanOrEqual(2);
    expect(screen.queryByRole('button', { name: /Enable/ })).toBeNull();
    await backToListReadAgain(server);
    expect(within(screen.getByRole('row', { name: /CI runner/ })).getByText('Disabled')).toBeTruthy();
  });

  it('a refused disable stays in the dialog with its reason and request id', async () => {
    const { server } = backend({
      'POST /api/v1/admin/users/3/disable': () => apiError(403, 'The request was rejected.', 'req-sa-csrf', 'FG-AUTH-0016'),
    });
    await renderApp({ fetch: server.fetch, path: `${PATH}&account=3`, lang: 'en' });
    await screen.findByRole('heading', { name: 'CI runner' });
    click('Disable CI runner');
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Disable' }));
    expect(await within(dialog).findByText('The request was rejected.')).toBeTruthy();
    expect(within(dialog).getByText('req-sa-csrf')).toBeTruthy();
  });
});

describe('service accounts: tokens', () => {
  const issued = {
    id: 12,
    public_id: 'wxyz9999',
    name: 'deploy',
    scopes: ['read_repository', 'merge_execute'],
    expires_at: '2027-01-30T00:00:00Z',
    is_agent: false,
    issued_by_user_id: 5,
    token: 'fgp_wxyz9999_SYNTHETICSECRET',
  };

  it('lists the tokens and the check grants, and never offers the admin scope', async () => {
    const { server } = backend();
    await renderApp({ fetch: server.fetch, path: `${PATH}&account=3`, lang: 'en' });
    const row = await screen.findByRole('row', { name: /ci/ });
    expect(within(row).getByText('read_repository')).toBeTruthy();
    expect(screen.queryByLabelText('admin')).toBeNull();
    expect(screen.getByLabelText('merge_execute')).toBeTruthy();
    expect(await screen.findByText('Project 7')).toBeTruthy();
  });

  it('issuing shows the secret once with the warning and a copy button, and drops it afterwards', async () => {
    const { server, state } = backend({
      [`POST ${ONE}/tokens`]: () => {
        state.tokens.push(token({ id: 12, name: 'deploy', public_id: 'wxyz9999' }));
        return json(201, issued);
      },
    });
    await openFromList(server);
    await screen.findByRole('row', { name: /ci/ });
    click('Issue token');
    expect((await screen.findAllByText('Enter a name.')).length).toBeGreaterThan(0);
    expect(server.to('POST', `${ONE}/tokens`)).toHaveLength(0);
    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'deploy' } });
    fireEvent.click(screen.getByLabelText('merge_execute'));
    fireEvent.change(screen.getByLabelText('Valid for (days)'), { target: { value: '30' } });
    click('Issue token');
    const panel = await screen.findByTestId('token-secret');
    expect(within(panel).getByTestId('token-value').textContent).toBe('fgp_wxyz9999_SYNTHETICSECRET');
    expect(within(panel).getByText(/only time the token is shown/)).toBeTruthy();
    expect(within(panel).getByRole('button', { name: 'Copy the token' })).toBeTruthy();
    expect(document.activeElement).toBe(panel);
    const post = server.to('POST', `${ONE}/tokens`)[0];
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(post?.body).toEqual({ name: 'deploy', scopes: ['read_repository', 'merge_execute'], expires_in_days: 30, is_agent: false });
    expect(await screen.findByRole('row', { name: /deploy/ })).toBeTruthy();
    expect(server.to('GET', ONE).length).toBeGreaterThanOrEqual(2);
    click('I have stored it');
    expect(screen.queryByTestId('token-secret')).toBeNull();
    expect(document.body.textContent).not.toContain('SYNTHETICSECRET');
    await backToListReadAgain(server);
  });

  it('a refused issue shows the reason and the request id, and no secret', async () => {
    const { server } = backend({
      [`POST ${ONE}/tokens`]: () =>
        apiError(403, 'A browser session is needed for this. Sign in again.', 'req-sa-sess', 'FG-AUTH-0002'),
    });
    await renderApp({ fetch: server.fetch, path: `${PATH}&account=3`, lang: 'en' });
    await screen.findByRole('row', { name: /ci/ });
    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'x' } });
    click('Issue token');
    expect(await screen.findByText('A browser session is needed for this. Sign in again.')).toBeTruthy();
    expect(screen.getByText('req-sa-sess')).toBeTruthy();
    expect(screen.queryByTestId('token-secret')).toBeNull();
  });

  it('a disabled account says it cannot get tokens, and has no enable button', async () => {
    const { server, state } = backend();
    state.accounts[0] = account({ state: 'disabled', active_tokens: 0 });
    await renderApp({ fetch: server.fetch, path: `${PATH}&account=3`, lang: 'en' });
    expect(await screen.findByText(/is disabled/)).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Issue token' })).toBeNull();
    expect(screen.queryByRole('button', { name: /Enable/ })).toBeNull();
  });

  it('revoking asks first, sends DELETE and refreshes the list and the item', async () => {
    const { server, state } = backend({
      [`DELETE ${ONE}/tokens/11`]: () => {
        state.tokens[0] = token({ revoked_at: '2026-11-04T00:00:00Z' });
        return new Response(null, { status: 204 });
      },
    });
    await openFromList(server);
    await screen.findByRole('row', { name: /ci/ });
    click('Revoke ci');
    const dialog = await screen.findByRole('dialog');
    expect(server.to('DELETE', `${ONE}/tokens/11`)).toHaveLength(0);
    fireEvent.click(within(dialog).getByRole('button', { name: 'Revoke' }));
    await waitFor(() => {
      expect(server.to('DELETE', `${ONE}/tokens/11`)).toHaveLength(1);
    });
    expect(server.to('DELETE', `${ONE}/tokens/11`)[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).toBeNull();
    });
    expect(await screen.findByText('Revoked')).toBeTruthy();
    expect(server.to('GET', ONE).length).toBeGreaterThanOrEqual(2);
    await backToListReadAgain(server);
  });

  it('a missing account is shown as the server answered, with the way back', async () => {
    const gone = () => apiError(404, 'That does not exist or you cannot see it.', 'req-sa-404', 'FG-REPO-0005');
    const { server } = backend({
      [`GET ${ONE}`]: gone,
      [`GET ${ONE}/tokens`]: gone,
      [`GET ${ONE}/check-grants`]: gone,
    });
    await renderApp({ fetch: server.fetch, path: `${PATH}&account=3`, lang: 'en' });
    expect((await screen.findAllByText('That does not exist or you cannot see it.')).length).toBeGreaterThan(0);
    expect(screen.getByRole('link', { name: 'Back to service accounts' })).toBeTruthy();
  });
});
