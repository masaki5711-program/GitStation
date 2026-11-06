// U3 (ADR-0025 §5.4): the act-as confirmation popup. The summary, the 1-second
// arming (visible + focused, then a fresh pointer / key action; hiding the page
// starts it over), one press only, every state, and the request bodies.
import axe from 'axe-core';
import { act, fireEvent, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { PAGE_TTL_MS } from '../src/pages/confirm/ActAsConfirm';
import { ARM_DELAY_MS } from '../src/pages/confirm/useArmed';
import { apiError, mockServer, PROJECT, SESSION, type Handler } from './mock';
import { json, renderApp } from './render';

const HEAD = 'a'.repeat(40);
const TIP = 'b'.repeat(40);
const STATE = 'S'.repeat(22);
const PAGE = '/-/confirm/projects/7/merge-requests/3';
const QUERY = `?intent=approve&sha=${HEAD}&integration=4&state=${STATE}`;
const CONFIRM = '/api/v1/projects/7/merge-requests/3/act-as-confirmation';
const ISSUE = '/-/confirm/projects/7/merge-requests/3/assertions';

const view = (over: Record<string, unknown> = {}) => ({
  project_id: 7,
  iid: 3,
  title: 'Raise the line speed',
  source_branch: 'feat/speed',
  target_branch: 'main',
  head_sha: HEAD,
  target_sha: TIP,
  sha_matches: true,
  intent: 'approve',
  integration_id: 4,
  integration_kind: 'kanflow',
  integration_name: 'kanflow-svc',
  person_display_name: '山田 太郎',
  person_username: 'taro',
  ...over,
});

function routes(over: Record<string, Handler> = {}): Record<string, Handler> {
  return {
    'GET /api/v1/session': () => json(200, SESSION),
    'GET /api/v1/projects/7': () => json(200, PROJECT),
    [`GET ${CONFIRM}`]: () => json(200, view()),
    [`POST ${ISSUE}`]: () => json(201, { status: 'issued' }),
    ...over,
  };
}

let focused = true;
beforeEach(() => {
  focused = true;
  vi.spyOn(document, 'hasFocus').mockImplementation(() => focused);
  vi.useFakeTimers({ shouldAdvanceTime: true, toFake: ['setTimeout', 'clearTimeout'] });
});
afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  Object.defineProperty(document, 'visibilityState', { value: 'visible', configurable: true });
});

async function open(
  table: Record<string, Handler> = routes(),
  path = `${PAGE}${QUERY}`,
  lang: 'ja' | 'en' = 'en',
) {
  const server = mockServer(table);
  const view = await renderApp({ fetch: server.fetch, path, lang });
  return { server, ...view };
}

const approve = () => screen.findByRole('button', { name: 'Approve' });
const advance = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));
const move = () => {
  act(() => {
    fireEvent.pointerMove(document);
  });
};
const setVisible = (visible: boolean) => {
  Object.defineProperty(document, 'visibilityState', {
    value: visible ? 'visible' : 'hidden',
    configurable: true,
  });
  act(() => {
    document.dispatchEvent(new Event('visibilitychange'));
  });
};
const pressed = (server: ReturnType<typeof mockServer>) => server.to('POST', ISSUE);

describe('the summary', () => {
  it('shows who asks, who, what, which merge request and the exact head', async () => {
    await open();
    const button = await approve();
    expect(screen.getByRole('heading', { level: 1, name: 'Confirm on your behalf' })).toBeTruthy();
    expect(screen.getByText(/kanflow \(kanflow-svc\) is trying to approve this merge request for you/)).toBeTruthy();
    expect(screen.getByText('山田 太郎 (taro)')).toBeTruthy();
    expect(screen.getByText('Raise the line speed')).toBeTruthy();
    expect(screen.getByText(HEAD)).toBeTruthy();
    expect(screen.getByText(TIP)).toBeTruthy();
    expect(screen.getByText('feat/speed')).toBeTruthy();
    expect(screen.getByText('main')).toBeTruthy();
    await screen.findByText('acme/forge-web');
    expect(screen.getByText(/within 5 minutes/)).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Cancel' })).toBeTruthy();
    // No navigation header, no default focus.
    expect(screen.queryByRole('navigation')).toBeNull();
    expect(document.activeElement === button).toBe(false);
  });

  it('says it in simple Japanese too, and for unapprove', async () => {
    await open(
      routes({ [`GET ${CONFIRM}`]: () => json(200, view({ intent: 'unapprove' })) }),
      `${PAGE}?intent=unapprove&sha=${HEAD}&integration=4&state=${STATE}`,
      'ja',
    );
    expect(await screen.findByRole('button', { name: '承認を取り消す' })).toBeTruthy();
    expect(screen.getByText(/あなたの代わりに/)).toBeTruthy();
  });
});

describe('the arming delay', () => {
  it('is disabled for 1 second, then waits for a fresh pointer action', async () => {
    const { server } = await open();
    const button = await approve();
    expect(button.getAttribute('aria-disabled')).toBe('true');
    // Restart the wait at a known point (the page was shown just now).
    setVisible(false);
    setVisible(true);
    await advance(ARM_DELAY_MS - 150);
    move();
    expect(button.getAttribute('aria-disabled')).toBe('true');
    fireEvent.click(button);
    expect(pressed(server)).toHaveLength(0);

    await advance(150);
    // Waited, but an action before arming does not count and none came after.
    expect(button.getAttribute('aria-disabled')).toBe('true');
    expect(screen.getByText(/Move the mouse or press a key/)).toBeTruthy();
    fireEvent.click(button);
    expect(pressed(server)).toHaveLength(0);

    move();
    expect(button.getAttribute('aria-disabled')).toBe('false');
    expect(screen.getByText('You can press the button.')).toBeTruthy();
  });

  it('is armed by a key as well, and a key press on it sends once (keyboard)', async () => {
    const { server } = await open();
    const button = await approve();
    await advance(ARM_DELAY_MS);
    act(() => {
      fireEvent.keyDown(document, { key: 'Tab' });
    });
    button.focus();
    expect(button.getAttribute('aria-disabled')).toBe('false');
    // A real Enter / Space on a button is a click.
    fireEvent.click(button);
    await waitFor(() => {
      expect(pressed(server)).toHaveLength(1);
    });
  });

  it('starts over when the page is hidden and shown again', async () => {
    const { server } = await open();
    const button = await approve();
    await advance(ARM_DELAY_MS);
    move();
    expect(button.getAttribute('aria-disabled')).toBe('false');

    setVisible(false);
    expect(button.getAttribute('aria-disabled')).toBe('true');
    await advance(5000);
    move();
    expect(button.getAttribute('aria-disabled')).toBe('true');

    setVisible(true);
    await advance(ARM_DELAY_MS - 150);
    move();
    expect(button.getAttribute('aria-disabled')).toBe('true');
    await advance(150);
    move();
    expect(button.getAttribute('aria-disabled')).toBe('false');
    expect(pressed(server)).toHaveLength(0);
  });

  it('starts over when the window loses and regains the focus', async () => {
    await open();
    const button = await approve();
    await advance(ARM_DELAY_MS);
    move();
    expect(button.getAttribute('aria-disabled')).toBe('false');
    focused = false;
    act(() => {
      window.dispatchEvent(new Event('blur'));
    });
    expect(button.getAttribute('aria-disabled')).toBe('true');
    focused = true;
    act(() => {
      window.dispatchEvent(new Event('focus'));
    });
    move();
    expect(button.getAttribute('aria-disabled')).toBe('true');
    await advance(ARM_DELAY_MS);
    move();
    expect(button.getAttribute('aria-disabled')).toBe('false');
  });

  it('never starts while the page is not focused', async () => {
    focused = false;
    await open();
    const button = await approve();
    await advance(5000);
    move();
    expect(button.getAttribute('aria-disabled')).toBe('true');
  });
});

describe('the press', () => {
  async function armed(table: Record<string, Handler> = routes()) {
    const o = await open(table);
    const button = await approve();
    await advance(ARM_DELAY_MS);
    move();
    return { ...o, button };
  }

  it('sends the intent, the viewed head, the integration and the state, with CSRF, then says done', async () => {
    const { server, button } = await armed();
    fireEvent.click(button);
    expect(await screen.findByRole('heading', { name: 'Done' })).toBeTruthy();
    const [call] = pressed(server);
    expect(call?.body).toEqual({ intent: 'approve', sha: HEAD, integration: 4, state: STATE });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    expect(screen.getByText('Close this screen and go back to the app.')).toBeTruthy();
    // The state is in no query: it only travels in the one body.
    expect(server.calls.filter((c) => c.method === 'GET').some((c) => c.path.includes(STATE) || c.query.has('state'))).toBe(false);
  });

  it('rejects a double submit: two clicks, one request', async () => {
    let release: (r: Response) => void = () => undefined;
    const gate = new Promise<Response>((resolve) => {
      release = resolve;
    });
    const { server, button } = await armed(routes({ [`POST ${ISSUE}`]: () => gate }));
    fireEvent.click(button);
    fireEvent.click(button);
    fireEvent.click(button);
    expect(await screen.findByText('Sending…')).toBeTruthy();
    expect(button.getAttribute('aria-disabled')).toBe('true');
    release(json(201, { status: 'issued' }));
    await screen.findByRole('heading', { name: 'Done' });
    expect(pressed(server)).toHaveLength(1);
  });

  it('lets the person cancel: nothing is sent', async () => {
    const { server } = await armed();
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(await screen.findByRole('heading', { name: 'Canceled' })).toBeTruthy();
    expect(pressed(server)).toHaveLength(0);
  });

  it('shows the server message and the request id on another failure, and allows a retry', async () => {
    let fail = true;
    const { server, button } = await armed(
      routes({
        [`POST ${ISSUE}`]: () => (fail ? apiError(500, 'Something went wrong on the server.', 'req-500') : json(201, { status: 'issued' })),
      }),
    );
    fireEvent.click(button);
    expect(await screen.findByText('Something went wrong on the server.')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-500');
    expect(screen.queryByText(/FG-/)).toBeNull();
    fail = false;
    fireEvent.click(button);
    await screen.findByRole('heading', { name: 'Done' });
    expect(pressed(server)).toHaveLength(2);
  });

  it('a head that moved meanwhile (409) is the stale state, with the request id', async () => {
    const { button } = await armed(routes({ [`POST ${ISSUE}`]: () => apiError(409, 'Head changed.', 'req-409') }));
    fireEvent.click(button);
    expect(await screen.findByRole('heading', { name: 'The merge request was updated' })).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-409');
  });

  it('an already used link (400) is the already-decided state', async () => {
    const { button } = await armed(routes({ [`POST ${ISSUE}`]: () => apiError(400, 'Bad request.', 'req-400') }));
    fireEvent.click(button);
    expect(await screen.findByRole('heading', { name: 'This link was already used or cannot be used' })).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-400');
  });
});

describe('the states', () => {
  it('shows a loading state first', async () => {
    let release: (r: Response) => void = () => undefined;
    const gate = new Promise<Response>((resolve) => {
      release = resolve;
    });
    await open(routes({ [`GET ${CONFIRM}`]: () => gate }));
    await screen.findByText('Loading…');
    expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
    release(json(200, view()));
    await approve();
  });

  it('refuses another person without any detail (403)', async () => {
    await open(routes({ [`GET ${CONFIRM}`]: () => apiError(403, 'Secret detail about roles.', 'req-403') }));
    expect(await screen.findByRole('heading', { name: 'You cannot do this' })).toBeTruthy();
    expect(screen.queryByText(/Secret detail/)).toBeNull();
    expect(screen.queryByTestId('request-id')).toBeNull();
    expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
  });

  it('says not found on 404, with the request id', async () => {
    await open(routes({ [`GET ${CONFIRM}`]: () => apiError(404, 'Not found.', 'req-404') }));
    expect(await screen.findByRole('heading', { name: 'Not found' })).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-404');
  });

  it('shows an error with the request id and a retry', async () => {
    await open(routes({ [`GET ${CONFIRM}`]: () => apiError(500, 'Server trouble.', 'req-500g') }));
    expect(await screen.findByText('Server trouble.')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-500g');
  });

  it('offers no button when the head is not the head the app showed', async () => {
    await open(routes({ [`GET ${CONFIRM}`]: () => json(200, view({ sha_matches: false })) }));
    expect(await screen.findByRole('heading', { name: 'The merge request was updated' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
  });

  it('expires after the page has been open too long', async () => {
    const { server } = await open();
    await approve();
    await advance(PAGE_TTL_MS);
    expect(await screen.findByRole('heading', { name: 'Time ran out' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
    expect(pressed(server)).toHaveLength(0);
  });

  it('a malformed link asks the API nothing', async () => {
    for (const q of [
      '',
      `?intent=merge&sha=${HEAD}&integration=4&state=${STATE}`,
      `?intent=approve&sha=zz&integration=4&state=${STATE}`,
      `?intent=approve&sha=${HEAD}&integration=0&state=${STATE}`,
      `?intent=approve&sha=${HEAD}&integration=4&state=short`,
      `?intent=approve&sha=${HEAD}&integration=4`,
    ]) {
      const { server, unmount } = await open(routes(), `${PAGE}${q}`);
      expect(await screen.findByRole('heading', { name: 'The link is not valid' })).toBeTruthy();
      expect(server.calls.filter((c) => c.path.includes('confirm'))).toHaveLength(0);
      unmount();
    }
  });
});

describe('accessibility', () => {
  it('has no axe violations on the summary or on a refusal, in every language', async () => {
    for (const lang of ['ja', 'en'] as const) {
      const ok = await open(routes(), `${PAGE}${QUERY}`, lang);
      await screen.findByRole('button', { name: lang === 'ja' ? '承認する' : 'Approve' });
      const main = document.querySelector('main') as HTMLElement;
      const run = (root: HTMLElement) =>
        axe.run(root, {
          runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
          rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
        });
      expect((await run(main)).violations.map((v) => v.id)).toEqual([]);
      ok.unmount();
      const no = await open(routes({ [`GET ${CONFIRM}`]: () => apiError(403, 'x', 'r') }), `${PAGE}${QUERY}`, lang);
      await screen.findByRole('alert');
      expect((await run(document.querySelector('main') as HTMLElement)).violations.map((v) => v.id)).toEqual([]);
      no.unmount();
    }
  });
});
