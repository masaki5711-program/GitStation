// U3a: a webhook's delivery history: state, attempts, response code, time;
// redeliver, whose 429 `delivery_pending` refusal is shown plainly (the
// server's user_message + request_id); older pages by `before`.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer, SESSION, type Handler } from './mock';
import { json, renderApp } from './render';
import { HOOKS, delivery, hookRoutes, webhook } from './webhook-fixtures';

const CSRF = SESSION.csrf_token;
const PATH = '/acme/forge-web/-/webhooks/3/deliveries';
const LIST = `${HOOKS}/3/deliveries`;

async function open(over: Record<string, Handler> = {}) {
  const server = mockServer(
    hookRoutes({
      [`GET ${HOOKS}/3`]: () => json(200, webhook(3)),
      [`GET ${LIST}`]: () =>
        json(200, {
          items: [
            delivery(2, { state: 'failed', attempts: 10, last_status: 503, event_type: 'merge_request.opened' }),
            delivery(1),
          ],
          next_before: null,
        }),
      ...over,
    }),
  );
  const view = await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
  await screen.findByRole('heading', { level: 1, name: 'Delivery history' });
  return { server, ...view };
}

describe('delivery history', () => {
  it('lists state, attempts, response code and time for each delivery', async () => {
    await open();
    const failed = await screen.findByRole('row', { name: /merge_request\.opened/ });
    expect(within(failed).getByText('Failed')).toBeTruthy();
    expect(within(failed).getByText('10')).toBeTruthy();
    expect(within(failed).getByText('503')).toBeTruthy();
    const ok = screen.getByRole('row', { name: /merge_request\.merged/ });
    expect(within(ok).getByText('Succeeded')).toBeTruthy();
    expect(within(ok).getByText('200')).toBeTruthy();
    expect(ok.querySelector('time')?.getAttribute('datetime')).toBe('2026-11-03T01:15:00Z');
    expect(screen.getByText('https://ci3.example.test:8443')).toBeTruthy();
  });

  it('shows the reason when there is no response (skipped, a connection failure)', async () => {
    await open({
      [`GET ${LIST}`]: () =>
        json(200, {
          items: [delivery(5, { state: 'skipped', attempts: 0, last_status: null, last_reason: 'owner_lost' })],
          next_before: null,
        }),
    });
    const row = await screen.findByRole('row', { name: /merge_request\.merged/ });
    expect(within(row).getByText('Skipped')).toBeTruthy();
    expect(within(row).getByText('owner_lost')).toBeTruthy();
  });

  it('says when there is no delivery yet', async () => {
    await open({ [`GET ${LIST}`]: () => json(200, { items: [], next_before: null }) });
    expect(await screen.findByText('No deliveries yet. They appear here when an event is sent.')).toBeTruthy();
  });

  it('redelivers one delivery and says it was queued', async () => {
    const { server } = await open({
      [`POST ${LIST}/2/redeliver`]: () => json(202, { outcome: 'queued' }),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Redeliver merge_request.opened (#2)' }));
    await waitFor(() => {
      expect(server.to('POST', `${LIST}/2/redeliver`)).toHaveLength(1);
    });
    expect(server.to('POST', `${LIST}/2/redeliver`)[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByText('Queued for delivery again.')).toBeTruthy();
  });

  it('refreshes the webhook after a redelivery, and reads the session after a 403', async () => {
    const { server } = await open({
      [`POST ${LIST}/2/redeliver`]: () => json(202, { outcome: 'queued' }),
      [`POST ${LIST}/1/redeliver`]: () => apiError(403, 'Rejected.', 'req-csrf-1'),
    });
    const hookReads = () => server.to('GET', `${HOOKS}/3`).length;
    await screen.findByRole('button', { name: 'Redeliver merge_request.opened (#2)' });
    const before = hookReads();
    fireEvent.click(screen.getByRole('button', { name: 'Redeliver merge_request.opened (#2)' }));
    await waitFor(() => {
      expect(hookReads()).toBe(before + 1);
    });
    const sessions = server.to('GET', '/api/v1/session').length;
    fireEvent.click(screen.getByRole('button', { name: 'Redeliver merge_request.merged (#1)' }));
    await screen.findByText('Rejected.');
    await waitFor(() => {
      expect(server.to('GET', '/api/v1/session').length).toBe(sessions + 1);
    });
  });

  it('shows the 429 refusal plainly, with its request id, and queues nothing', async () => {
    await open({
      [`POST ${LIST}/1/redeliver`]: () =>
        apiError(429, 'This delivery is still in progress. Try again in a minute.', 'req-429-1', 'FG-WEB-0010'),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Redeliver merge_request.merged (#1)' }));
    expect(await screen.findByText('This delivery is still in progress. Try again in a minute.')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-429-1');
    expect(screen.queryByText('Queued for delivery again.')).toBeNull();
  });

  it('loads older deliveries with before', async () => {
    const { server } = await open({
      [`GET ${LIST}`]: (c) =>
        c.query.get('before') === null
          ? json(200, { items: [delivery(2)], next_before: 2 })
          : json(200, { items: [delivery(1, { event_type: 'review.requested' })], next_before: null }),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Show older deliveries' }));
    expect(await screen.findByRole('row', { name: /review\.requested/ })).toBeTruthy();
    expect(server.to('GET', LIST).map((c) => c.query.get('before'))).toEqual([null, '2']);
    // The loading is announced, so it is not silent.
    expect(screen.getAllByRole('status').some((s) => s.textContent === '1 more deliveries loaded.')).toBe(true);
    expect(screen.queryByRole('button', { name: 'Show older deliveries' })).toBeNull();
  });

  it('shows why a stopped webhook stopped, and links back', async () => {
    await open({
      [`GET ${HOOKS}/3`]: () => json(200, webhook(3, { enabled: false, disabled_reason: 'failing_24h' })),
    });
    expect(
      await screen.findByText('Stopped after 24 hours of failures. Fix the receiver, then press Enable.'),
    ).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Back to webhooks' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/webhooks',
    );
  });

  it('shows the failure of the history with its request id', async () => {
    const server = mockServer(
      hookRoutes({
        [`GET ${HOOKS}/3`]: () => json(200, webhook(3)),
        [`GET ${LIST}`]: () => apiError(404, 'That webhook does not exist.', 'req-404-1'),
      }),
    );
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    expect(await screen.findByText('That webhook does not exist.')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-404-1');
  });

  it('an address that is not a webhook id is not found', async () => {
    const server = mockServer(hookRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/webhooks/abc/deliveries', lang: 'en' });
    expect(await screen.findByRole('heading', { level: 1, name: /not found/i })).toBeTruthy();
  });
});
