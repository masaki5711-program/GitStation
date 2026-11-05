// U3a: the project's webhook settings (Maintainer+). The URL is write-only
// (reads show the origin); the secret is shown once after create or rotate,
// in its own live region, and is gone after "I have saved it"; enabling
// again is one press; a refusal shows user_message + request_id; every
// unsafe request carries the CSRF header; destructive requests ask first.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer, SESSION, type Handler } from './mock';
import { json, renderApp } from './render';
import { HOOKS, SECRET, hookRoutes, webhook } from './webhook-fixtures';

const CSRF = SESSION.csrf_token;
const PATH = '/acme/forge-web/-/webhooks';
const ORIGIN = 'https://ci3.example.test:8443';

async function open(over: Record<string, Handler> = {}) {
  const server = mockServer(hookRoutes(over));
  const view = await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
  await screen.findByRole('heading', { level: 1, name: 'Webhooks' });
  return { server, ...view };
}

const click = async (name: string | RegExp) => {
  fireEvent.click(await screen.findByRole('button', { name }));
};

describe('webhook list', () => {
  it('shows the origin and event types, never a full URL or a secret', async () => {
    await open();
    const row = await screen.findByRole('row', { name: /ci3\.example\.test/ });
    expect(within(row).getByText(ORIGIN)).toBeTruthy();
    expect(within(row).getByText('merge_request.*')).toBeTruthy();
    expect(within(row).getByText('Enabled')).toBeTruthy();
    expect(document.body.textContent).not.toContain('whsec_');
  });

  it('says what to do when there is none', async () => {
    await open({ [`GET ${HOOKS}`]: () => json(200, { items: [] }) });
    expect(await screen.findByText('No webhooks yet. Add one below.')).toBeTruthy();
  });

  it('shows the failure and its request id', async () => {
    const server = mockServer(
      hookRoutes({ [`GET ${HOOKS}`]: () => apiError(500, 'Webhooks could not be read.', 'req-hooks-1') }),
    );
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    expect(await screen.findByText('Webhooks could not be read.')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-hooks-1');
  });

  it('a Developer is refused by the server: the reason is shown, not the list', async () => {
    const server = mockServer(
      hookRoutes({ [`GET ${HOOKS}`]: () => apiError(403, 'You need the Maintainer role.', 'req-403') }),
    );
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    expect(await screen.findByText('You need the Maintainer role.')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-403');
  });

  it('shows one line for a stopped webhook: why, and what to press next', async () => {
    const { server } = await open({
      [`GET ${HOOKS}`]: () =>
        json(200, {
          items: [
            webhook(3, { enabled: false, disabled_reason: 'failures', consecutive_failures: 50 }),
            webhook(4, { enabled: false, disabled_reason: 'failing_24h' }),
            webhook(5, { enabled: false, disabled_reason: 'manual' }),
          ],
        }),
      [`PATCH ${HOOKS}/3`]: () => json(200, { outcome: 'updated', fields: ['enabled'], webhook: webhook(3) }),
    });
    expect(
      await screen.findByText('Stopped after 50 failed deliveries in a row. Fix the receiver, then press Enable.'),
    ).toBeTruthy();
    expect(screen.getByText('Stopped after 24 hours of failures. Fix the receiver, then press Enable.')).toBeTruthy();
    expect(screen.getByText('Turned off by a person. Press Enable to start again.')).toBeTruthy();
    await click(`Enable ${ORIGIN}`);
    await waitFor(() => {
      expect(server.to('PATCH', `${HOOKS}/3`)).toHaveLength(1);
    });
    const call = server.to('PATCH', `${HOOKS}/3`)[0];
    expect(call?.body).toEqual({ enabled: true });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByText('Enabled again. Its failure count was reset.')).toBeTruthy();
  });

  it('turns a webhook off with one press', async () => {
    const { server } = await open({
      [`PATCH ${HOOKS}/3`]: () =>
        json(200, {
          outcome: 'updated',
          fields: ['enabled'],
          webhook: webhook(3, { enabled: false, disabled_reason: 'manual' }),
        }),
    });
    await click(`Disable ${ORIGIN}`);
    await waitFor(() => {
      expect(server.to('PATCH', `${HOOKS}/3`)[0]?.body).toEqual({ enabled: false });
    });
    expect(await screen.findByText('Turned off.')).toBeTruthy();
  });
});

describe('create', () => {
  it('creates, shows the secret once, then forgets it', async () => {
    let created = false;
    const { server } = await open({
      [`GET ${HOOKS}`]: () => json(200, { items: created ? [webhook(3), webhook(4)] : [webhook(3)] }),
      [`POST ${HOOKS}`]: () => {
        created = true;
        return json(201, { webhook: webhook(4), secret: SECRET });
      },
    });
    fireEvent.change(screen.getByLabelText('Payload URL'), {
      target: { value: 'https://hooks.example.test:8443/t/abc' },
    });
    fireEvent.change(screen.getByLabelText('Event types'), {
      target: { value: 'merge_request.*, review.requested' },
    });
    await click('Add webhook');
    const panel = await screen.findByTestId('webhook-secret');
    const call = server.to('POST', HOOKS)[0];
    expect(call?.body).toEqual({
      url: 'https://hooks.example.test:8443/t/abc',
      event_types: ['merge_request.*', 'review.requested'],
    });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(within(panel).getByTestId('webhook-secret-value').textContent).toBe(SECRET);
    expect(within(panel).getByText(/You will not see it again/)).toBeTruthy();
    expect(within(panel).getByRole('button', { name: 'Copy the secret' })).toBeTruthy();
    // The announcement sits in a live region that was already in the page.
    expect(
      screen.getAllByRole('status').some((s) => s.textContent.includes('The secret is shown below')),
    ).toBe(true);
    expect(document.activeElement).toBe(panel);
    expect(await screen.findByText('https://ci4.example.test:8443')).toBeTruthy();
    fireEvent.click(within(panel).getByRole('button', { name: 'I have saved it' }));
    await waitFor(() => {
      expect(screen.queryByTestId('webhook-secret')).toBeNull();
    });
    expect(document.body.textContent).not.toContain(SECRET);
    expect(document.activeElement?.id).toBe('webhooks-title');
  });

  it('sends * when no event type is typed', async () => {
    const { server } = await open({
      [`POST ${HOOKS}`]: () => json(201, { webhook: webhook(4), secret: SECRET }),
    });
    fireEvent.change(screen.getByLabelText('Payload URL'), { target: { value: 'https://hooks.example.test:8443/' } });
    fireEvent.change(screen.getByLabelText('Event types'), { target: { value: '' } });
    await click('Add webhook');
    await waitFor(() => {
      expect(server.to('POST', HOOKS)).toHaveLength(1);
    });
    expect((server.to('POST', HOOKS)[0]?.body as { event_types: string[] }).event_types).toEqual(['*']);
  });

  it('asks for the URL before sending anything', async () => {
    const { server } = await open();
    await click('Add webhook');
    expect((await screen.findAllByText('Enter the address that should receive the events.')).length).toBeGreaterThan(0);
    expect(server.to('POST', HOOKS)).toHaveLength(0);
  });

  it('shows a refusal (not on the allowlist) with its request id', async () => {
    await open({
      [`POST ${HOOKS}`]: () => apiError(400, 'This destination is not allowed.', 'req-allow-1', 'FG-INT-0010'),
    });
    fireEvent.change(screen.getByLabelText('Payload URL'), { target: { value: 'https://evil.example.test/' } });
    await click('Add webhook');
    expect(await screen.findByText('This destination is not allowed.')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-allow-1');
    expect(screen.queryByTestId('webhook-secret')).toBeNull();
  });
});

describe('edit, rotate, delete', () => {
  it('edits: the URL is write-only (empty keeps it) and only changed fields are sent', async () => {
    const { server } = await open({
      [`PATCH ${HOOKS}/3`]: () =>
        json(200, { outcome: 'updated', fields: ['event_types'], webhook: webhook(3, { event_types: ['*'] }) }),
    });
    await click(`Edit ${ORIGIN}`);
    const dialog = await screen.findByRole('dialog');
    const url = within(dialog).getByLabelText('New payload URL (leave empty to keep the current one)');
    expect((url as HTMLInputElement).value).toBe('');
    expect(within(dialog).getByText(/Current origin/).textContent).toContain(ORIGIN);
    fireEvent.change(within(dialog).getByLabelText('Event types'), { target: { value: '*' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Save' }));
    await waitFor(() => {
      expect(server.to('PATCH', `${HOOKS}/3`)).toHaveLength(1);
    });
    expect(server.to('PATCH', `${HOOKS}/3`)[0]?.body).toEqual({ event_types: ['*'] });
    expect(await screen.findByText('Saved.')).toBeTruthy();
  });

  it('says so when nothing changed, and sends nothing', async () => {
    const { server } = await open();
    await click(`Edit ${ORIGIN}`);
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Save' }));
    expect(await within(dialog).findByText('Nothing was changed.')).toBeTruthy();
    expect(server.to('PATCH', `${HOOKS}/3`)).toHaveLength(0);
  });

  it('rotates after asking, and shows the new secret once', async () => {
    const { server } = await open({
      [`POST ${HOOKS}/3/secret`]: () =>
        json(200, { secret: SECRET, secret_version: 2, previous_secret_valid_until: '2026-11-04T01:00:00Z' }),
    });
    await click(`Rotate secret of ${ORIGIN}`);
    const confirm = await screen.findByTestId('confirm');
    expect(within(confirm).getByText(/24 hours/)).toBeTruthy();
    expect(server.to('POST', `${HOOKS}/3/secret`)).toHaveLength(0);
    fireEvent.click(within(confirm).getByRole('button', { name: 'Rotate secret' }));
    const panel = await screen.findByTestId('webhook-secret');
    expect(within(panel).getByTestId('webhook-secret-value').textContent).toBe(SECRET);
    expect(within(panel).getByText(/The previous secret still signs deliveries until/)).toBeTruthy();
    expect(server.to('POST', `${HOOKS}/3/secret`)[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
  });

  it('keeps the confirmation open with the reason when a rotation is refused', async () => {
    await open({ [`POST ${HOOKS}/3/secret`]: () => apiError(403, 'People only.', 'req-rot-1') });
    await click(`Rotate secret of ${ORIGIN}`);
    const confirm = await screen.findByTestId('confirm');
    fireEvent.click(within(confirm).getByRole('button', { name: 'Rotate secret' }));
    expect(await within(confirm).findByText('People only.')).toBeTruthy();
    expect(within(confirm).getByTestId('request-id').textContent).toBe('req-rot-1');
  });

  it('deletes only after asking; the history goes with it', async () => {
    const { server } = await open({
      [`DELETE ${HOOKS}/3`]: () => new Response(null, { status: 204 }),
    });
    await click(`Delete ${ORIGIN}`);
    const confirm = await screen.findByTestId('confirm');
    expect(within(confirm).getByText(/delivery history/)).toBeTruthy();
    expect(server.to('DELETE', `${HOOKS}/3`)).toHaveLength(0);
    fireEvent.click(within(confirm).getByRole('button', { name: 'Delete webhook' }));
    await waitFor(() => {
      expect(server.to('DELETE', `${HOOKS}/3`)).toHaveLength(1);
    });
    expect(server.to('DELETE', `${HOOKS}/3`)[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByText('The webhook was deleted.')).toBeTruthy();
  });
});

describe('a11y details', () => {
  it('keeps Cancel focusable while a save is under way (aria-disabled, not disabled)', async () => {
    let release: () => void = () => undefined;
    await open({
      [`PATCH ${HOOKS}/3`]: () =>
        new Promise<Response>((resolve) => {
          release = () => {
            resolve(json(200, { outcome: 'updated', fields: ['event_types'], webhook: webhook(3) }));
          };
        }),
    });
    await click(`Edit ${ORIGIN}`);
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByLabelText('Event types'), { target: { value: '*' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Save' }));
    const cancel = await within(dialog).findByRole('button', { name: 'Cancel' });
    await waitFor(() => {
      expect(cancel.getAttribute('aria-disabled')).toBe('true');
    });
    expect((cancel as HTMLButtonElement).disabled).toBe(false);
    release();
  });

  it('links the help of the payload URL to its field', async () => {
    await open();
    const url = await screen.findByLabelText('Payload URL');
    expect(document.getElementById(url.getAttribute('aria-describedby') ?? '')?.textContent).toContain('write-only');
  });
});

describe('navigation and languages', () => {
  it('the project navigation links the screen for a Maintainer', async () => {
    const server = mockServer(hookRoutes());
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    const link = await screen.findByRole('link', { name: 'Webhooks' });
    expect(link.getAttribute('aria-current')).toBe('page');
  });

  it('shows the link to an administrator without a role, not to a Reporter', async () => {
    const asRole = (role: string | null) =>
      hookRoutes({
        'GET /api/v1/projects/7': () =>
          json(200, {
            id: 7,
            namespace_id: 11,
            path: 'acme/forge-web',
            name: 'forge-web',
            display_name: 'Forge Web',
            description: '',
            visibility: 'internal',
            created_at: '2026-10-01T00:00:00Z',
            ...(role === null ? {} : { role }),
          }),
        'GET /api/v1/session': () => json(200, { ...SESSION, user: { ...SESSION.user, is_admin: role === null } }),
        'GET /api/v1/projects/7/repository/refs': () => json(200, { default_branch: null, refs: [], truncated: false }),
      });
    const admin = mockServer(asRole(null));
    const view = await renderApp({ fetch: admin.fetch, path: '/acme/forge-web/-/branches', lang: 'en' });
    expect(await screen.findByRole('link', { name: 'Webhooks' })).toBeTruthy();
    view.unmount();
    const reporter = mockServer(asRole('reporter'));
    await renderApp({ fetch: reporter.fetch, path: '/acme/forge-web/-/branches', lang: 'en' });
    await screen.findByRole('link', { name: 'Commits' });
    expect(screen.queryByRole('link', { name: 'Webhooks' })).toBeNull();
  });

  it('hides the link from a Developer', async () => {
    const server = mockServer(
      hookRoutes({
        'GET /api/v1/projects/7': () =>
          json(200, {
            id: 7,
            namespace_id: 11,
            path: 'acme/forge-web',
            name: 'forge-web',
            display_name: 'Forge Web',
            description: '',
            visibility: 'internal',
            created_at: '2026-10-01T00:00:00Z',
            role: 'developer',
          }),
        'GET /api/v1/projects/7/repository/refs': () => json(200, { default_branch: null, refs: [], truncated: false }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/branches', lang: 'en' });
    await screen.findByRole('link', { name: 'Commits' });
    expect(screen.queryByRole('link', { name: 'Webhooks' })).toBeNull();
  });

  it('renders in Japanese', async () => {
    const server = mockServer(hookRoutes());
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'ja' });
    expect(await screen.findByRole('heading', { level: 1, name: 'Webhook' })).toBeTruthy();
  });
});
