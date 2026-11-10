// Integrations (M3 I1a / X3 UI): the administration (list, create, view,
// disable / enable) and a project's kanflow bindings in its settings (the
// state, and a Maintainer's direct unbind after a confirmation). Every refusal
// is the server's one line + request_id; every unsafe request carries the
// CSRF header; an action the API would refuse is not offered.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { parseEventTypes } from '../src/pages/settings/Integrations';
import { mayUnbind } from '../src/pages/settings/IntegrationBindings';
import { settingsRoutes } from './merge-settings-fixtures';
import { apiError, mockServer, PROJECT, SESSION, type Handler } from './mock';
import { json, renderApp } from './render';

const CSRF = SESSION.csrf_token;
const ADMIN = { ...SESSION, user: { ...SESSION.user, is_admin: true } };
const LIST = '/api/v1/admin/integrations';
const ONE = `${LIST}/4`;
const PATH = '/taro/-/settings?tab=integrations';
const BINDINGS = '/api/v1/projects/7/integration-bindings';
const SETTINGS = '/acme/forge-web/-/settings';

const integration = (over: Record<string, unknown> = {}) => ({
  id: 4,
  kind: 'kanflow',
  service_account: { id: 3, username: 'sa-kanflow' },
  enabled: true,
  event_types: ['*'],
  bindings: 1,
  created_by: 5,
  created_at: '2026-11-01T00:00:00Z',
  updated_at: '2026-11-01T00:00:00Z',
  ...over,
});
const binding = (over: Record<string, unknown> = {}) => ({
  integration_id: 4,
  kind: 'kanflow',
  project_id: 7,
  external_project_id: 'KF-12',
  granted_membership: true,
  created_by: 5,
  created_at: '2026-11-02T00:00:00Z',
  ...over,
});
const account = (id: number, username: string, over: Record<string, unknown> = {}) => ({
  id,
  username,
  display_name: username,
  purpose: 'synthetic',
  state: 'active',
  active_tokens: 0,
  last_token_expiry: null,
  created_at: '2026-10-01T00:00:00Z',
  created_by: 5,
  owner: { id: 5, username: 'taro', active: true },
  ...over,
});

function admin(over: Record<string, Handler> = {}) {
  const state = { items: [integration()] };
  const routes: Record<string, Handler> = {
    'GET /api/v1/session': () => json(200, ADMIN),
    'GET /api/v1/user/notifications/unread-count': () => json(200, { count: 0, capped: false }),
    'GET /api/v1/namespaces/resolve': () =>
      json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
    'GET /api/v1/user/tokens': () => json(200, { items: [] }),
    [`GET ${LIST}`]: () => json(200, { items: state.items, truncated: false }),
    [`GET ${ONE}`]: () => json(200, state.items[0]),
    [`GET ${ONE}/bindings`]: () => json(200, { items: [binding()] }),
    'GET /api/v1/admin/service-accounts': () =>
      json(200, {
        items: [account(3, 'sa-kanflow'), account(8, 'sa-kanflow-2'), account(9, 'sa-old', { state: 'disabled' })],
        truncated: false,
      }),
    ...over,
  };
  return { state, server: mockServer(routes) };
}

const click = (name: string | RegExp) => {
  fireEvent.click(screen.getByRole('button', { name }));
};

const runAxe = (root: HTMLElement) =>
  axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });

describe('integrations: who sees them', () => {
  it('a person who is not an administrator has no tab, and ?tab=integrations shows the tokens', async () => {
    const server = mockServer({
      'GET /api/v1/session': () => json(200, SESSION),
      'GET /api/v1/user/notifications/unread-count': () => json(200, { count: 0, capped: false }),
      'GET /api/v1/namespaces/resolve': () =>
        json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
      'GET /api/v1/user/tokens': () => json(200, { items: [] }),
    });
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    await screen.findByText('You have no tokens.');
    expect(screen.queryByRole('link', { name: 'Integrations' })).toBeNull();
    expect(server.to('GET', LIST)).toHaveLength(0);
  });

  it('an administrator has the tab next to service accounts, and the list', async () => {
    const { server } = admin();
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings', lang: 'en' });
    fireEvent.click(await screen.findByRole('link', { name: 'Integrations' }));
    const row = await screen.findByRole('row', { name: /kanflow #4/ });
    expect(within(row).getByText('sa-kanflow')).toBeTruthy();
    expect(within(row).getByText('Enabled')).toBeTruthy();
    expect(within(row).getByText('All events')).toBeTruthy();
    expect(within(row).getByRole('button', { name: 'Disable kanflow #4' })).toBeTruthy();
  });

  it('an empty list says so', async () => {
    const { server, state } = admin();
    state.items = [];
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    expect(await screen.findByText('There are no integrations yet. Create one below.')).toBeTruthy();
  });

  it('a refused list shows the reason and the request id', async () => {
    const { server } = admin({
      [`GET ${LIST}`]: () => apiError(500, 'Something went wrong on the server.', 'req-int-list', 'FG-WEB-0001'),
    });
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    expect(await screen.findByText('Something went wrong on the server.')).toBeTruthy();
    expect(screen.getByText('req-int-list')).toBeTruthy();
  });
});

describe('integrations: create, view, disable, enable', () => {
  it('offers only active service accounts without an integration, validates, and posts', async () => {
    const { server, state } = admin({
      [`POST ${LIST}`]: () => {
        const made = integration({ id: 5, service_account: { id: 8, username: 'sa-kanflow-2' }, bindings: 0, event_types: ['merge_request.*'] });
        state.items.push(made);
        return json(201, made);
      },
    });
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    await screen.findByRole('row', { name: /kanflow #4/ });
    const select = await screen.findByLabelText('Service account');
    const options = within(select).getAllByRole('option').map((o) => o.textContent);
    expect(options).toEqual(['Choose a service account', 'sa-kanflow-2 (sa-kanflow-2)']);
    click('Create integration');
    expect((await screen.findAllByText('Choose the service account it acts as.')).length).toBeGreaterThan(0);
    expect(server.to('POST', LIST)).toHaveLength(0);
    fireEvent.change(select, { target: { value: '8' } });
    fireEvent.change(screen.getByLabelText('Event types'), { target: { value: 'merge_request.*' } });
    click('Create integration');
    await waitFor(() => {
      expect(server.to('POST', LIST)).toHaveLength(1);
    });
    const post = server.to('POST', LIST)[0];
    expect(post?.body).toEqual({ kind: 'kanflow', service_account_id: 8, event_types: ['merge_request.*'] });
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByText('kanflow #5 was created.')).toBeTruthy();
    expect(await screen.findByRole('row', { name: /kanflow #5/ })).toBeTruthy();
  });

  it('a refused create reads as the server line with the request id', async () => {
    const { server } = admin({
      [`POST ${LIST}`]: () => apiError(403, 'A browser session is needed for this. Sign in again.', 'req-int-sess', 'FG-AUTH-0002'),
    });
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    fireEvent.change(await screen.findByLabelText('Service account'), { target: { value: '8' } });
    click('Create integration');
    expect(await screen.findByText('A browser session is needed for this. Sign in again.')).toBeTruthy();
    expect(screen.getByText('req-int-sess')).toBeTruthy();
  });

  it('the detail shows the facts and the bound projects, with the way back', async () => {
    const { server } = admin();
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    fireEvent.click(await screen.findByRole('link', { name: 'View kanflow #4' }));
    expect(await screen.findByRole('heading', { name: 'kanflow #4' })).toBeTruthy();
    const row = await screen.findByRole('row', { name: /Project 7/ });
    expect(within(row).getByText('KF-12')).toBeTruthy();
    expect(within(row).getByText('Reporter (given by the binding)')).toBeTruthy();
    expect(within(row).getByRole('link', { name: 'Project 7' }).getAttribute('href')).toBe('/-/projects/7');
    expect(screen.getByRole('link', { name: 'Back to integrations' })).toBeTruthy();
  });

  it('disabling asks first, patches enabled=false, refreshes, and moves the focus to the heading', async () => {
    const { server, state } = admin({
      [`PATCH ${ONE}`]: () => {
        state.items[0] = integration({ enabled: false });
        return json(200, { outcome: 'updated', fields: ['enabled'], integration: state.items[0] });
      },
    });
    await renderApp({ fetch: server.fetch, path: `${PATH}&integration=4`, lang: 'en' });
    await screen.findByRole('heading', { name: 'kanflow #4' });
    click('Disable kanflow #4');
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(/stops receiving events/)).toBeTruthy();
    expect(server.to('PATCH', ONE)).toHaveLength(0);
    fireEvent.click(within(dialog).getByRole('button', { name: 'Disable' }));
    await waitFor(() => {
      expect(server.to('PATCH', ONE)).toHaveLength(1);
    });
    expect(server.to('PATCH', ONE)[0]?.body).toEqual({ enabled: false });
    expect(server.to('PATCH', ONE)[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByText(/is disabled: kanflow receives no events/)).toBeTruthy();
    expect(screen.getAllByRole('status').some((s) => s.textContent === 'kanflow #4 was disabled.')).toBe(true);
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'kanflow #4' }));
    });
    expect(screen.getByRole('button', { name: 'Enable kanflow #4' })).toBeTruthy();
  });

  it('enabling is not asked, and a refusal shows the reason and the request id', async () => {
    const { server, state } = admin({
      [`PATCH ${ONE}`]: () => apiError(403, 'The request was rejected.', 'req-int-csrf', 'FG-AUTH-0016'),
    });
    state.items[0] = integration({ enabled: false });
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    await screen.findByRole('row', { name: /kanflow #4/ });
    expect(screen.queryByRole('button', { name: 'Disable kanflow #4' })).toBeNull();
    click('Enable kanflow #4');
    expect(await screen.findByText('The request was rejected.')).toBeTruthy();
    expect(screen.getByText('req-int-csrf')).toBeTruthy();
    expect(server.to('PATCH', ONE)[0]?.body).toEqual({ enabled: true });
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('a refused disable stays in the dialog with its reason and request id', async () => {
    const { server } = admin({
      [`PATCH ${ONE}`]: () => apiError(403, 'The request was rejected.', 'req-int-dis', 'FG-AUTH-0016'),
    });
    await renderApp({ fetch: server.fetch, path: `${PATH}&integration=4`, lang: 'en' });
    await screen.findByRole('heading', { name: 'kanflow #4' });
    click('Disable kanflow #4');
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Disable' }));
    expect(await within(dialog).findByText('The request was rejected.')).toBeTruthy();
    expect(within(dialog).getByText('req-int-dis')).toBeTruthy();
  });

  it('event types are split on commas and spaces', () => {
    expect(parseEventTypes(' merge_request.*, push  review.opened ')).toEqual(['merge_request.*', 'push', 'review.opened']);
    expect(parseEventTypes('  ')).toEqual([]);
  });
});

describe('project settings: kanflow bindings', () => {
  const routes = (role: string, over: Record<string, Handler> = {}) =>
    settingsRoutes({
      'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role }),
      [`GET ${BINDINGS}`]: () => json(200, { items: [binding()] }),
      ...over,
    });

  it('shows the binding to everyone who can read the project, without an unbind for a Developer', async () => {
    const server = mockServer(routes('developer'));
    await renderApp({ fetch: server.fetch, path: SETTINGS, lang: 'en' });
    const section = (await screen.findByRole('heading', { name: 'Kanflow' })).closest('section') as HTMLElement;
    const row = await within(section).findByRole('row', { name: /kanflow #4/ });
    expect(within(row).getByText('KF-12')).toBeTruthy();
    expect(within(section).queryByRole('button', { name: /Unbind/ })).toBeNull();
    expect(within(section).getByText(/Binding is done from kanflow/)).toBeTruthy();
  });

  it('an unbound repository says how to bind it', async () => {
    const server = mockServer(routes('maintainer', { [`GET ${BINDINGS}`]: () => json(200, { items: [] }) }));
    await renderApp({ fetch: server.fetch, path: SETTINGS, lang: 'en' });
    expect(await screen.findByText(/not bound to a kanflow project/)).toBeTruthy();
  });

  it('a refused read shows the reason and the request id', async () => {
    const server = mockServer(
      routes('maintainer', { [`GET ${BINDINGS}`]: () => apiError(500, 'Something went wrong on the server.', 'req-bind-read', 'FG-WEB-0001') }),
    );
    await renderApp({ fetch: server.fetch, path: SETTINGS, lang: 'en' });
    expect(await screen.findByText('req-bind-read')).toBeTruthy();
  });

  it('a Maintainer unbinds after a confirmation; the request is the DELETE with CSRF', async () => {
    const state = { items: [binding()] };
    const server = mockServer(
      routes('maintainer', {
        [`GET ${BINDINGS}`]: () => json(200, { items: state.items }),
        [`DELETE ${BINDINGS}/4`]: () => {
          state.items = [];
          return new Response(null, { status: 204 });
        },
      }),
    );
    await renderApp({ fetch: server.fetch, path: SETTINGS, lang: 'en' });
    fireEvent.click(await screen.findByRole('button', { name: 'Unbind kanflow #4 (kanflow projects: KF-12)' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(/Reporter membership/)).toBeTruthy();
    expect(server.to('DELETE', `${BINDINGS}/4`)).toHaveLength(0);
    fireEvent.click(within(dialog).getByRole('button', { name: 'Unbind' }));
    await waitFor(() => {
      expect(server.to('DELETE', `${BINDINGS}/4`)).toHaveLength(1);
    });
    expect(server.to('DELETE', `${BINDINGS}/4`)[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByText(/not bound to a kanflow project/)).toBeTruthy();
    expect(screen.getAllByRole('status').some((s) => s.textContent === 'The binding to kanflow #4 was removed.')).toBe(true);
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByRole('heading', { name: 'Kanflow' }));
    });
  });

  it('two bindings of one integration are one row with one unbind that names both and removes both', async () => {
    // A shared spec repository bound to two kanflow projects through one
    // integration: the route removes every binding of the integration.
    const state = {
      items: [
        binding({ external_project_id: 'KF-12', granted_membership: false }),
        binding({ external_project_id: 'KF-30', granted_membership: true, created_at: '2026-11-03T00:00:00Z' }),
      ],
    };
    const server = mockServer(
      routes('maintainer', {
        [`GET ${BINDINGS}`]: () => json(200, { items: state.items }),
        [`DELETE ${BINDINGS}/4`]: () => {
          state.items = [];
          return new Response(null, { status: 204 });
        },
      }),
    );
    await renderApp({ fetch: server.fetch, path: SETTINGS, lang: 'en' });
    const section = (await screen.findByRole('heading', { name: 'Kanflow' })).closest('section') as HTMLElement;
    const rows = await within(section).findAllByRole('row', { name: /kanflow #4/ });
    expect(rows).toHaveLength(1);
    expect(within(rows[0] as HTMLElement).getByText('KF-12')).toBeTruthy();
    expect(within(rows[0] as HTMLElement).getByText('KF-30')).toBeTruthy();
    const buttons = within(section).getAllByRole('button', { name: /Unbind/ });
    expect(buttons).toHaveLength(1);
    expect(buttons[0]?.getAttribute('aria-label')).toBe('Unbind kanflow #4 (kanflow projects: KF-12, KF-30)');
    fireEvent.click(buttons[0] as HTMLElement);
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(/all of them are removed together/)).toBeTruthy();
    const listed = within(within(dialog).getByRole('list', { name: 'Kanflow project' })).getAllByRole('listitem');
    expect(listed.map((li) => li.textContent)).toEqual(['KF-12', 'KF-30']);
    // One of the two gave the service account its membership.
    expect(within(dialog).getByText(/Reporter membership/)).toBeTruthy();
    fireEvent.click(within(dialog).getByRole('button', { name: 'Unbind' }));
    await waitFor(() => {
      expect(server.to('DELETE', `${BINDINGS}/4`)).toHaveLength(1);
    });
    expect(server.calls.filter((c) => c.method === 'DELETE')).toHaveLength(1);
    expect(await screen.findByText(/not bound to a kanflow project/)).toBeTruthy();
  });

  it('no membership line when no binding of the integration gave one', async () => {
    const server = mockServer(routes('maintainer', { [`GET ${BINDINGS}`]: () => json(200, { items: [binding({ granted_membership: false })] }) }));
    await renderApp({ fetch: server.fetch, path: SETTINGS, lang: 'en' });
    fireEvent.click(await screen.findByRole('button', { name: /Unbind kanflow #4/ }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).queryByText(/Reporter membership/)).toBeNull();
  });

  it('a refused unbind stays in the dialog with its reason and request id', async () => {
    const server = mockServer(
      routes('owner', {
        [`DELETE ${BINDINGS}/4`]: () => apiError(403, 'You do not have permission for this.', 'req-unbind', 'FG-AUTH-0003'),
      }),
    );
    await renderApp({ fetch: server.fetch, path: SETTINGS, lang: 'en' });
    fireEvent.click(await screen.findByRole('button', { name: /Unbind kanflow #4/ }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Unbind' }));
    expect(await within(dialog).findByText('You do not have permission for this.')).toBeTruthy();
    expect(within(dialog).getByText('req-unbind')).toBeTruthy();
  });

  it('the unbind follows the route rule: Maintainer, Owner or an administrator', () => {
    expect(mayUnbind('maintainer', false)).toBe(true);
    expect(mayUnbind('owner', false)).toBe(true);
    expect(mayUnbind('developer', false)).toBe(false);
    expect(mayUnbind('reporter', false)).toBe(false);
    expect(mayUnbind(null, false)).toBe(false);
    expect(mayUnbind(null, true)).toBe(true);
  });
});

describe('integrations: accessibility', () => {
  it('has no axe violations on the list, the detail and the project section, in ja and en', async () => {
    for (const lang of ['ja', 'en'] as const) {
      const { server } = admin();
      const list = await renderApp({ fetch: server.fetch, path: PATH, lang });
      await screen.findByRole('row', { name: /kanflow #4/ });
      await waitFor(() => {
        expect(document.querySelector('main form')).not.toBeNull();
      });
      expect((await runAxe(document.querySelector('main') as HTMLElement)).violations.map((v) => v.id)).toEqual([]);
      list.unmount();
      const detail = await renderApp({ fetch: server.fetch, path: `${PATH}&integration=4`, lang });
      await screen.findByRole('row', { name: /7/ });
      expect((await runAxe(document.querySelector('main') as HTMLElement)).violations.map((v) => v.id)).toEqual([]);
      detail.unmount();
      const project = mockServer(
        settingsRoutes({ [`GET ${BINDINGS}`]: () => json(200, { items: [binding()] }) }),
      );
      const settings = await renderApp({ fetch: project.fetch, path: SETTINGS, lang });
      await screen.findByRole('row', { name: /kanflow #4/ });
      expect((await runAxe(document.querySelector('main') as HTMLElement)).violations.map((v) => v.id)).toEqual([]);
      settings.unmount();
    }
  });
});
