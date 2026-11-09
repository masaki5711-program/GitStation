// M3 U4: the in-app notifications — the header's bell (the unread count from
// the server, polled with backoff, announced when it grows) and the
// `/-/notifications` page (all / unread, links to the merge request, mark one
// or all read with the CSRF header, load more, empty / loading / error
// states), with axe on the page in every language.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { MAX_POLL_MS, POLL_MS, pollDelay } from '../src/api/notifications';
import { badgeText } from '../src/components/NotificationBell';
import type { Language } from '../src/i18n';
import { apiError, BELL_PATH, mockServer, page, SESSION, signedIn, type Handler } from './mock';
import { json, renderApp } from './render';

const LIST = '/api/v1/user/notifications';
const CSRF = SESSION.csrf_token;

const item = (id: number, over: Record<string, unknown> = {}) => ({
  id,
  kind: 'mr_approved',
  project_id: 7,
  project_path: 'acme/forge-web',
  merge_request_iid: 4,
  merge_request_title: `Fix rounding ${String(id)}`,
  actor_name: 'Hanako',
  created_at: '2026-11-09T09:00:00Z',
  read_at: null,
  ...over,
});

const home = (): Record<string, Handler> => ({
  'GET /api/v1/projects': () => page([]),
  'GET /api/v1/groups': () => page([]),
});

const routes = (over: Record<string, Handler> = {}): Record<string, Handler> => ({
  ...signedIn(),
  [`GET ${BELL_PATH}`]: () => json(200, { count: 2, capped: false }),
  [`GET ${LIST}`]: () =>
    json(200, { items: [item(9), item(8, { kind: 'mr_merged', read_at: '2026-11-09T10:00:00Z' })], next_before: null }),
  ...over,
});

async function openPage(over: Record<string, Handler> = {}, path = '/-/notifications', lang: Language = 'en') {
  const server = mockServer(routes(over));
  const view = await renderApp({ fetch: server.fetch, path, lang });
  await screen.findByRole('heading', { level: 1, name: lang === 'en' ? 'Notifications' : /.+/ });
  await waitFor(() => {
    expect(screen.queryByText(/読み込んでいます|Loading|Memuat/)).toBeNull();
  });
  return { server, ...view };
}

describe('the bell', () => {
  it('names the unread count and links to the notifications page', async () => {
    const server = mockServer({ ...routes(), ...home() });
    await renderApp({ fetch: server.fetch, lang: 'en' });
    const bell = await screen.findByRole('link', { name: 'Notifications (2 unread)' });
    expect(bell.getAttribute('href')).toBe('/-/notifications');
    expect(within(bell).getByText('2').getAttribute('aria-hidden')).toBe('true');
    expect(server.unmatched).toEqual([]);
  });

  it('shows no number at zero and says so when the count cannot be read', async () => {
    const zero = mockServer({ ...routes({ [`GET ${BELL_PATH}`]: () => json(200, { count: 0, capped: false }) }), ...home() });
    const first = await renderApp({ fetch: zero.fetch, lang: 'en' });
    expect(await screen.findByRole('link', { name: 'Notifications' })).toBeTruthy();
    first.unmount();
    const failing = mockServer({
      ...routes({ [`GET ${BELL_PATH}`]: () => apiError(503, 'Unavailable.', 'req-bell-1') }),
      ...home(),
    });
    await renderApp({ fetch: failing.fetch, lang: 'en' });
    expect(await screen.findByRole('link', { name: 'Notifications (the unread count could not be read)' })).toBeTruthy();
  });

  it('is not shown while signed out', async () => {
    const server = mockServer({
      'GET /api/v1/session': () => apiError(401, 'x', 'r', 'FG-AUTH-0001'),
    });
    await renderApp({ fetch: server.fetch, lang: 'en' });
    await screen.findByRole('heading', { name: 'Please sign in' });
    expect(screen.queryByRole('link', { name: /Notifications/ })).toBeNull();
    expect(server.to('GET', BELL_PATH)).toHaveLength(0);
  });

  it('caps the badge at 99+ and backs off after failures', () => {
    expect(badgeText(5, false)).toBe('5');
    expect(badgeText(99, false)).toBe('99');
    expect(badgeText(100, true)).toBe('99+');
    expect(badgeText(99, true)).toBe('99+');
    expect(pollDelay(0)).toBe(POLL_MS);
    expect(pollDelay(1)).toBe(POLL_MS * 2);
    expect(pollDelay(2)).toBe(POLL_MS * 4);
    expect(pollDelay(50)).toBe(MAX_POLL_MS);
  });
});

describe('the notifications page', () => {
  it('lists each item in one sentence with a link to its merge request', async () => {
    await openPage();
    const list = screen.getByRole('list', { name: 'All' });
    const rows = within(list).getAllByRole('listitem');
    expect(rows).toHaveLength(2);
    expect(within(rows[0] as HTMLElement).getByText('Hanako approved your merge request')).toBeTruthy();
    expect(within(rows[0] as HTMLElement).getByText('Unread')).toBeTruthy();
    const link = within(rows[0] as HTMLElement).getByRole('link', { name: /acme\/forge-web!4 Fix rounding 9/ });
    expect(link.getAttribute('href')).toBe('/acme/forge-web/-/pull/4');
    expect(within(rows[1] as HTMLElement).getByText('Hanako merged your merge request')).toBeTruthy();
    // A read item has no "Mark as read" button.
    expect(within(rows[1] as HTMLElement).queryByRole('button')).toBeNull();
    expect(screen.getByRole('link', { name: 'Notification settings' }).getAttribute('href')).toBe(
      '/taro/-/settings?tab=notifications',
    );
  });

  it('marks one read with the CSRF header, refreshes the count and announces it', async () => {
    let count = 1;
    const { server } = await openPage({
      [`GET ${BELL_PATH}`]: () => json(200, { count, capped: false }),
      [`POST ${LIST}/9/read`]: () => {
        count = 3; // two more arrived meanwhile
        return json(200, { id: 9, read_at: '2026-11-09T11:00:00Z' });
      },
    });
    await screen.findByRole('link', { name: 'Notifications (1 unread)' });
    fireEvent.click(screen.getByRole('button', { name: 'Mark as read (Fix rounding 9)' }));
    await waitFor(() => {
      expect(server.to('POST', `${LIST}/9/read`)).toHaveLength(1);
    });
    expect(server.to('POST', `${LIST}/9/read`)[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    const done = await screen.findByText('Marked as read.');
    expect(done.closest('[role="status"]')).not.toBeNull();
    // The focus moves on to the next item's link (the button is gone).
    await waitFor(() => {
      expect(document.activeElement?.textContent).toContain('Fix rounding 8');
    });
    // The count was asked again; its growth is announced politely.
    const news = await screen.findByText('New notifications. 3 unread.');
    expect(news.getAttribute('role')).toBe('status');
    expect(await screen.findByRole('link', { name: 'Notifications (3 unread)' })).toBeTruthy();
  });

  it('following the link marks the item read', async () => {
    const { server } = await openPage({
      [`POST ${LIST}/9/read`]: () => json(200, { id: 9, read_at: '2026-11-09T11:00:00Z' }),
      'GET /api/v1/namespaces/resolve': () => apiError(404, 'Not found.', 'req-nf', 'FG-REPO-0005'),
    });
    fireEvent.click(screen.getByRole('link', { name: /Fix rounding 9/ }));
    await waitFor(() => {
      expect(server.to('POST', `${LIST}/9/read`)).toHaveLength(1);
    });
  });

  it('marks all read up to the newest item shown', async () => {
    const { server } = await openPage({ [`POST ${LIST}/read-all`]: () => new Response(null, { status: 204 }) });
    fireEvent.click(screen.getByRole('button', { name: 'Mark all as read' }));
    await waitFor(() => {
      expect(server.to('POST', `${LIST}/read-all`)).toHaveLength(1);
    });
    const call = server.to('POST', `${LIST}/read-all`)[0];
    expect(call?.body).toEqual({ up_to: 9 });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    // No count is shown (the server gives none: it would count hidden items).
    expect(await screen.findByText('All notifications were marked as read.')).toBeTruthy();
    // The button went away with the last unread item: the heading has the focus.
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1, name: 'Notifications' }));
    });
  });

  it('says the same result again when it happens again', async () => {
    await openPage({
      [`GET ${LIST}`]: () => json(200, { items: [item(9), item(7)], next_before: null }),
      [`POST ${LIST}/9/read`]: () => json(200, { id: 9, read_at: '2026-11-09T11:00:00Z' }),
      [`POST ${LIST}/7/read`]: () => json(200, { id: 7, read_at: '2026-11-09T11:00:00Z' }),
    });
    fireEvent.click(screen.getByRole('button', { name: 'Mark as read (Fix rounding 9)' }));
    const status = await screen.findByText('Marked as read.');
    fireEvent.click(screen.getByRole('button', { name: 'Mark as read (Fix rounding 7)' }));
    // Emptied first, then the same words: a change a screen reader reads out.
    await waitFor(() => {
      expect(status.textContent).toBe('');
    });
    await waitFor(() => {
      expect(status.textContent).toBe('Marked as read.');
    });
  });

  it('names an unknown kind in a general sentence', async () => {
    await openPage({ [`GET ${LIST}`]: () => json(200, { items: [item(9, { kind: 'mr_future' })], next_before: null }) });
    expect(screen.getByText('There is a new notification about your merge request')).toBeTruthy();
    expect(screen.queryByText(/mr_future/)).toBeNull();
  });

  it('marks the bell as the current page here', async () => {
    await openPage({ [`GET ${BELL_PATH}`]: () => json(200, { count: 120, capped: true }) });
    const bell = await screen.findByRole('link', { name: 'Notifications (99+ unread)' });
    expect(bell.getAttribute('aria-current')).toBe('page');
  });

  it('shows only unread ones on the unread filter and says when there are none', async () => {
    const { server } = await openPage({
      [`GET ${LIST}`]: (c) =>
        c.query.get('unread') === 'true'
          ? json(200, { items: [], next_before: null })
          : json(200, { items: [item(9)], next_before: null }),
    });
    const unread = screen.getByRole('button', { name: 'Unread only' });
    expect(unread.getAttribute('aria-pressed')).toBe('false');
    fireEvent.click(unread);
    expect(await screen.findByText('You have no unread notifications.')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Unread only' }).getAttribute('aria-pressed')).toBe('true');
    expect(server.calls.some((c) => c.path === LIST && c.query.get('unread') === 'true')).toBe(true);
    // Nothing unread is shown: no "mark all".
    expect(screen.queryByRole('button', { name: 'Mark all as read' })).toBeNull();
  });

  it('pages back with before and shows the empty state', async () => {
    const { server } = await openPage({
      [`GET ${LIST}`]: (c) =>
        c.query.get('before') === '9'
          ? json(200, { items: [item(5)], next_before: null })
          : json(200, { items: [item(9)], next_before: 9 }),
    });
    fireEvent.click(screen.getByRole('button', { name: 'Show more' }));
    const added = await screen.findByRole('link', { name: /Fix rounding 5/ });
    // The focus goes to the first new item and the number is said.
    await waitFor(() => {
      expect(document.activeElement).toBe(added);
    });
    expect(await screen.findByText('1 more notification is shown.')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Show more' })).toBeNull();
    expect(server.calls.filter((c) => c.path === LIST).map((c) => c.query.get('limit'))).toEqual(['20', '20']);
    const empty = mockServer(routes({ [`GET ${LIST}`]: () => json(200, { items: [], next_before: null }) }));
    await renderApp({ fetch: empty.fetch, path: '/-/notifications', lang: 'en' });
    expect(await screen.findByText('You have no notifications.')).toBeTruthy();
  });

  it('shows a failure to read with its request id and a retry', async () => {
    const server = mockServer(routes({ [`GET ${LIST}`]: () => apiError(500, 'The list could not be read.', 'req-inbox-1') }));
    await renderApp({ fetch: server.fetch, path: '/-/notifications', lang: 'en' });
    expect(await screen.findByText('The list could not be read.')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-inbox-1');
    expect(screen.getByRole('button', { name: 'Try again' })).toBeTruthy();
  });

  it('shows a refused mark with its request id', async () => {
    await openPage({ [`POST ${LIST}/9/read`]: () => apiError(404, 'Not found.', 'req-inbox-2', 'FG-REPO-0005') });
    fireEvent.click(screen.getByRole('button', { name: 'Mark as read (Fix rounding 9)' }));
    expect(await screen.findByText('Not found.')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-inbox-2');
  });

  it.each([
    [403, 'req-mark-403', 'FG-AUTH-0016'],
    [401, 'req-mark-401', 'FG-AUTH-0001'],
  ])('a refused mark (%i) reads the session, the list and the count again and logs its request id', async (status, requestId, code) => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    try {
      const { server } = await openPage({
        [`POST ${LIST}/9/read`]: () => apiError(status, 'Refused.', requestId, code),
      });
      const before = {
        session: server.to('GET', '/api/v1/session').length,
        list: server.to('GET', LIST).length,
        count: server.to('GET', BELL_PATH).length,
      };
      fireEvent.click(screen.getByRole('button', { name: 'Mark as read (Fix rounding 9)' }));
      await waitFor(() => {
        expect(server.to('GET', '/api/v1/session').length).toBeGreaterThan(before.session);
        expect(server.to('GET', LIST).length).toBeGreaterThan(before.list);
        expect(server.to('GET', BELL_PATH).length).toBeGreaterThan(before.count);
      });
      await waitFor(() => {
        expect(warn.mock.calls.some((c) => String(c[0]).includes(`request ${requestId}`))).toBe(true);
      });
    } finally {
      warn.mockRestore();
    }
  });
});

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`);
}

describe.each(['ja', 'en', 'id'] as Language[])('U4 axe (%s)', (lang) => {
  it('the page with unread and read items, and the bell', async () => {
    const { container } = await openPage({}, '/-/notifications', lang);
    await screen.findAllByRole('listitem');
    expect(await violations(container)).toEqual([]);
  });

  it('the empty page', async () => {
    const { container } = await openPage({ [`GET ${LIST}`]: () => json(200, { items: [], next_before: null }) }, '/-/notifications?unread=1', lang);
    await waitFor(() => {
      expect(screen.queryAllByRole('listitem')).toHaveLength(0);
    });
    expect(await violations(container)).toEqual([]);
  });
});
