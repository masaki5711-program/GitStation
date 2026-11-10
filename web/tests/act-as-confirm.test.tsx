// U3 (ADR-0025 §5.4): the act-as confirmation popup. The summary, the arming
// (1 s visible + focused, then a real pointer-down on the button or a key,
// and the click at least 300 ms later), one press only, every state, the
// request body, the page timeout under a press in flight, and the `state`
// leaving the address. Pure fake timers (clock and Date).
import axe from 'axe-core';
import { act, fireEvent, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { graphemes, PAGE_TTL_MS, shortTitle, TITLE_MAX_CHARS } from '../src/pages/confirm/ActAsConfirm';
import { ARM_DELAY_MS, ARM_GAP_MS, eventTrust } from '../src/pages/confirm/useArmed';
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
  window.sessionStorage.clear();
  window.history.replaceState(null, '', PAGE);
  vi.spyOn(eventTrust, 'check').mockImplementation((e) => people.has(e));
  vi.spyOn(document, 'hasFocus').mockImplementation(() => focused);
  // Pure fake timers (clock and Date). Testing Library advances them while it
  // waits when a `jest` global with advanceTimersByTime exists.
  vi.useFakeTimers();
  Object.assign(globalThis, { jest: { advanceTimersByTime: (ms: number) => vi.advanceTimersByTime(ms) } });
});
afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  Reflect.deleteProperty(globalThis, 'jest');
  Object.defineProperty(document, 'visibilityState', { value: 'visible', configurable: true });
});

async function open(
  table: Record<string, Handler> = routes(),
  path = `${PAGE}${QUERY}`,
  lang: 'ja' | 'en' = 'en',
) {
  const server = mockServer(table);
  const rendered = await renderApp({ fetch: server.fetch, path, lang });
  return { server, ...rendered };
}

const approve = () => screen.findByRole('button', { name: 'Approve' });
const advance = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));
/** A person's event (jsdom's own are never trusted). */
const people = new WeakSet<Event>();
const trusted = <E extends Event>(e: E): E => {
  people.add(e);
  return e;
};
const down = (el: Element, real = true) => {
  const e = new MouseEvent('pointerdown', { bubbles: true });
  act(() => {
    el.dispatchEvent(real ? trusted(e) : e);
  });
};
const key = (real = true) => {
  const e = new KeyboardEvent('keydown', { key: 'Tab', bubbles: true });
  act(() => {
    document.dispatchEvent(real ? trusted(e) : e);
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
/** Restart the 1 s wait at a known point and let it pass. */
const waitOut = async () => {
  setVisible(false);
  setVisible(true);
  await advance(ARM_DELAY_MS);
};
/** A real, accepted press: wait, arm on the button, let the gap pass. */
const arm = async (button: HTMLElement) => {
  await waitOut();
  down(button);
  await advance(ARM_GAP_MS);
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
    // The 5 minutes come from one constant.
    expect(screen.getByText(/within 5 minutes/)).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Cancel' })).toBeTruthy();
    // No navigation header, no default focus.
    expect(screen.queryByRole('navigation')).toBeNull();
    expect(document.activeElement === button).toBe(false);
  });

  // Review of ADR-0025 (2026-11-09, security LOW d): the title is the
  // author's text — shown cut short and quiet; the number and the commit are
  // the facts the person checks.
  it('cuts a long merge request title short and makes the number and the head prominent', async () => {
    const long = `Please approve ${'x'.repeat(300)} END-OF-TITLE`;
    await open(routes({ [`GET ${CONFIRM}`]: () => json(200, view({ title: long })) }));
    await approve();
    expect(screen.queryByText(long)).toBeNull();
    expect(screen.queryByText(/END-OF-TITLE/)).toBeNull();
    const shown = screen.getByTestId('mr-title');
    expect(shown.textContent.endsWith('…')).toBe(true);
    expect(graphemes(shown.textContent).length).toBe(TITLE_MAX_CHARS + 1);
    expect(shown.getAttribute('title')).toBeNull();
    expect(screen.getByTestId('mr-iid').textContent).toBe('!3');
    expect(screen.getByTestId('mr-iid').tagName).toBe('STRONG');
    expect(screen.getByText(HEAD).closest('strong')).toBeTruthy();
  });

  it('never cuts a character in half and keeps a title on one line', () => {
    const family = '👨‍👩‍👧';
    expect(shortTitle(family.repeat(100))).toBe(`${family.repeat(TITLE_MAX_CHARS)}…`);
    expect(shortTitle('a\n\nb\tc')).toBe('a b c');
    expect(shortTitle('x'.repeat(TITLE_MAX_CHARS))).toBe('x'.repeat(TITLE_MAX_CHARS));
  });

  // Review of the follow-ups (2026-11-10, LOW): a browser without
  // Intl.Segmenter still renders (code points), and a title cannot reorder
  // the text around it with bidi controls.
  it('cuts by code points when Intl.Segmenter is missing', () => {
    vi.stubGlobal('Intl', { ...Intl, Segmenter: undefined });
    try {
      expect(graphemes('ab😀')).toEqual(['a', 'b', '😀']);
      expect(shortTitle('y'.repeat(100))).toBe(`${'y'.repeat(TITLE_MAX_CHARS)}…`);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it('strips bidi controls from the title', async () => {
    expect(shortTitle('safe \u202Etxt.exe\u202C \u2066x\u2069 \u202Ay\u202D')).toBe('safe txt.exe x y');
    await open(routes({ [`GET ${CONFIRM}`]: () => json(200, view({ title: 'Fix \u202Eevil' })) }));
    await approve();
    expect(screen.getByTestId('mr-title').textContent).toBe('Fix evil');
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

describe('the arming', () => {
  it('needs 1 s, then a real pointer-down ON the button; movement, scripts and other places do not arm', async () => {
    const { server } = await open();
    const button = await approve();
    expect(button.getAttribute('aria-disabled')).toBe('true');
    setVisible(false);
    setVisible(true);
    await advance(ARM_DELAY_MS - 100);
    down(button);
    key();
    expect(button.getAttribute('aria-disabled')).toBe('true');
    await advance(100);
    expect(screen.getByText(/Press the mouse on the button or press a key/)).toBeTruthy();
    // Not arming: movement, an untrusted event, a pointer-down elsewhere.
    act(() => {
      document.dispatchEvent(trusted(new MouseEvent('pointermove', { bubbles: true })));
    });
    down(button, false);
    key(false);
    down(screen.getByRole('heading', { level: 1 }));
    expect(button.getAttribute('aria-disabled')).toBe('true');
    // A real one on the button arms it...
    down(button);
    expect(button.getAttribute('aria-disabled')).toBe('false');
    // ...but the press itself cannot be that action: a click now is refused.
    fireEvent.click(button);
    expect(await screen.findByText(/Press once more/)).toBeTruthy();
    expect(pressed(server)).toHaveLength(0);
    // The arming stays; once the gap has passed the next click counts - and
    // uses it up.
    expect(button.getAttribute('aria-disabled')).toBe('false');
    await advance(ARM_GAP_MS);
    fireEvent.click(button);
    await waitFor(() => {
      expect(pressed(server)).toHaveLength(1);
    });
  });

  it('a key press arms it as well (keyboard), and the click must come later', async () => {
    const { server } = await open();
    const button = await approve();
    await waitOut();
    key();
    button.focus();
    expect(button.getAttribute('aria-disabled')).toBe('false');
    await advance(ARM_GAP_MS - 1);
    fireEvent.click(button);
    expect(pressed(server)).toHaveLength(0);
    await advance(1);
    fireEvent.click(button);
    await waitFor(() => {
      expect(pressed(server)).toHaveLength(1);
    });
    // Used up: another click needs a new arming action.
    expect(button.getAttribute('aria-disabled')).toBe('true');
  });

  it('starts over when the page is hidden and shown again', async () => {
    const { server } = await open();
    const button = await approve();
    await waitOut();
    down(button);
    expect(button.getAttribute('aria-disabled')).toBe('false');
    setVisible(false);
    expect(button.getAttribute('aria-disabled')).toBe('true');
    await advance(5000);
    down(button);
    expect(button.getAttribute('aria-disabled')).toBe('true');
    setVisible(true);
    await advance(ARM_DELAY_MS - 100);
    down(button);
    expect(button.getAttribute('aria-disabled')).toBe('true');
    await advance(100);
    down(button);
    expect(button.getAttribute('aria-disabled')).toBe('false');
    expect(pressed(server)).toHaveLength(0);
  });

  it('starts over when the window loses and regains the focus', async () => {
    await open();
    const button = await approve();
    await waitOut();
    down(button);
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
    down(button);
    expect(button.getAttribute('aria-disabled')).toBe('true');
    await advance(ARM_DELAY_MS);
    down(button);
    expect(button.getAttribute('aria-disabled')).toBe('false');
  });

  it('never starts while the page is not focused', async () => {
    focused = false;
    await open();
    const button = await approve();
    await advance(5000);
    down(button);
    expect(button.getAttribute('aria-disabled')).toBe('true');
  });
});

describe('the press', () => {
  async function armed(table: Record<string, Handler> = routes()) {
    const o = await open(table);
    const button = await approve();
    await arm(button);
    return { ...o, button };
  }

  it('sends the intent, the viewed head and target tip, the integration and the state, with CSRF, then says done', async () => {
    const { server, button } = await armed();
    fireEvent.click(button);
    expect(await screen.findByRole('heading', { name: 'Done' })).toBeTruthy();
    const [call] = pressed(server);
    expect(call?.body).toEqual({ intent: 'approve', sha: HEAD, target_sha: TIP, integration: 4, state: STATE });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    expect(screen.getByText('Close this screen and go back to the app.')).toBeTruthy();
    expect(server.calls.filter((c) => c.method === 'GET').some((c) => c.path.includes(STATE) || c.query.has('state'))).toBe(false);
  });

  it('rejects a double submit: three clicks, one request', async () => {
    let release: (r: Response) => void = () => undefined;
    const gate = new Promise<Response>((resolve) => {
      release = resolve;
    });
    const { server, button } = await armed(routes({ [`POST ${ISSUE}`]: () => gate }));
    fireEvent.click(button);
    fireEvent.click(button);
    fireEvent.click(button);
    expect(await screen.findByText('Sending…')).toBeTruthy();
    // Even a fully re-armed click while the first is in flight sends nothing.
    await arm(button);
    fireEvent.click(button);
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

  it('shows the server message and the request id on another failure, and allows a new press', async () => {
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
    await arm(button);
    fireEvent.click(button);
    await screen.findByRole('heading', { name: 'Done' });
    expect(pressed(server)).toHaveLength(2);
  });

  it('a head or target that moved meanwhile (409) is the stale state, with the request id', async () => {
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

  it('a session that ended while the page was open (401) hands over to sign-in, never silence', async () => {
    let signedIn = true;
    const { button } = await armed(
      routes({
        'GET /api/v1/session': () => (signedIn ? json(200, SESSION) : apiError(401, 'Sign in.', 'req-401')),
        [`POST ${ISSUE}`]: () => {
          signedIn = false;
          return apiError(401, 'Sign in.', 'req-401');
        },
      }),
    );
    fireEvent.click(button);
    expect(await screen.findByRole('heading', { name: 'Please sign in' })).toBeTruthy();
  });

  it('the page timeout never fires under a press in flight (9:59.9 race)', async () => {
    let release: (r: Response) => void = () => undefined;
    const gate = new Promise<Response>((resolve) => {
      release = resolve;
    });
    const { server, button } = await armed(routes({ [`POST ${ISSUE}`]: () => gate }));
    await advance(PAGE_TTL_MS - ARM_DELAY_MS - ARM_GAP_MS - 5000);
    fireEvent.click(button);
    await screen.findByText('Sending…');
    await advance(10_000);
    // Past the timeout, with the press still in flight: still the press.
    expect(screen.queryByRole('heading', { name: 'Time ran out' })).toBeNull();
    expect(screen.getByText('Sending…')).toBeTruthy();
    release(json(201, { status: 'issued' }));
    expect(await screen.findByRole('heading', { name: 'Done' })).toBeTruthy();
    expect(pressed(server)).toHaveLength(1);
  });

  it('a press that fails after the timeout then shows the expired state', async () => {
    let release: (r: Response) => void = () => undefined;
    const gate = new Promise<Response>((resolve) => {
      release = resolve;
    });
    const { button } = await armed(routes({ [`POST ${ISSUE}`]: () => gate }));
    await advance(PAGE_TTL_MS - ARM_DELAY_MS - ARM_GAP_MS - 5000);
    fireEvent.click(button);
    await screen.findByText('Sending…');
    await advance(10_000);
    release(apiError(500, 'Down.', 'req-late'));
    expect(await screen.findByRole('heading', { name: 'Time ran out' })).toBeTruthy();
  });
});

describe('the state in the address', () => {
  it('leaves the address bar and is kept for this tab only', async () => {
    window.history.replaceState(null, '', `${PAGE}${QUERY}&x=1`);
    const { server } = await open();
    const button = await approve();
    expect(window.location.search).toBe(`?intent=approve&sha=${HEAD}&integration=4&x=1`);
    expect(window.location.href).not.toContain(STATE);
    await arm(button);
    fireEvent.click(button);
    await screen.findByRole('heading', { name: 'Done' });
    expect(pressed(server)[0]?.body).toMatchObject({ state: STATE });
  });

  it('survives a reload or the sign-in round trip (the address no longer has it)', async () => {
    window.history.replaceState(null, '', `${PAGE}${QUERY}`);
    const first = await open();
    await approve();
    first.unmount();
    const { server } = await open(routes(), `${PAGE}?intent=approve&sha=${HEAD}&integration=4`);
    const button = await approve();
    await arm(button);
    fireEvent.click(button);
    await screen.findByRole('heading', { name: 'Done' });
    expect(pressed(server)[0]?.body).toMatchObject({ state: STATE });
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

  // Review of ADR-0025 (2026-11-10, LOW): a 403 is not always "another
  // person" (a disabled integration is a 403 too): a neutral sentence and the
  // request id, never the server's detail.
  it('refuses on 403 with a neutral sentence and the request id, without any detail', async () => {
    await open(routes({ [`GET ${CONFIRM}`]: () => apiError(403, 'Secret detail about roles.', 'req-403') }));
    expect(await screen.findByRole('heading', { name: 'This cannot be confirmed' })).toBeTruthy();
    expect(screen.queryByText(/someone else's account/)).toBeNull();
    expect(screen.queryByText(/Secret detail/)).toBeNull();
    expect(screen.getByTestId('request-id').textContent).toBe('req-403');
    expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
  });

  // ADR-0025 (decision of 2026-11-10): whoever may not press gets the
  // invisible project's 404: one neutral notice (the binding screen's too),
  // never the server's detail.
  it('says not found on 404 with the neutral sentence and the request id', async () => {
    await open(routes({ [`GET ${CONFIRM}`]: () => apiError(404, 'Not found.', 'req-404') }));
    expect(await screen.findByRole('heading', { name: 'Not found' })).toBeTruthy();
    expect(screen.getByText('What this confirmation is for was not found, or this account cannot confirm it. If you think this is wrong, tell an administrator the request ID below.')).toBeTruthy();
    expect(screen.queryByText(/Not found\./)).toBeNull();
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
      window.sessionStorage.clear();
      const { server, unmount } = await open(routes(), `${PAGE}${q}`);
      expect(await screen.findByRole('heading', { name: 'The link is not valid' })).toBeTruthy();
      expect(server.calls.filter((c) => c.path.includes('confirm'))).toHaveLength(0);
      unmount();
    }
  });
});

describe('accessibility', () => {
  it('has no axe violations on the summary or on a refusal, in every language', async () => {
    // axe runs on real timers.
    vi.useRealTimers();
    Reflect.deleteProperty(globalThis, 'jest');
    for (const lang of ['ja', 'en'] as const) {
      window.sessionStorage.clear();
      const ok = await open(routes(), `${PAGE}${QUERY}`, lang);
      await screen.findByRole('button', { name: lang === 'ja' ? '承認する' : 'Approve' });
      const run = (root: HTMLElement) =>
        axe.run(root, {
          runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
          rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
        });
      expect((await run(document.querySelector('main') as HTMLElement)).violations.map((v) => v.id)).toEqual([]);
      ok.unmount();
      window.sessionStorage.clear();
      const no = await open(routes({ [`GET ${CONFIRM}`]: () => apiError(403, 'x', 'r') }), `${PAGE}${QUERY}`, lang);
      await screen.findByRole('alert');
      expect((await run(document.querySelector('main') as HTMLElement)).violations.map((v) => v.id)).toEqual([]);
      no.unmount();
    }
  });
});
