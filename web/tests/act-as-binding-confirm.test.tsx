// M3 X3 (ADR-0025 Amendment X3 §4): the binding confirmation popup. It shows
// forge's facts only (repository, the kanflow project id the app named, the
// setting, whether it is bound now), arms like the merge request popup, sends
// exactly the confirmed binding, takes the state out of the address, refuses
// a malformed link without asking the API, and says why it cannot help.
import axe from 'axe-core';
import { act, fireEvent, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ARM_DELAY_MS, ARM_GAP_MS, eventTrust } from '../src/pages/confirm/useArmed';
import { apiError, mockServer, SESSION, type Handler } from './mock';
import { json, renderApp } from './render';

const STATE = 'B'.repeat(22);
const PAGE = '/-/confirm/projects/7/integration-bindings';
const SET = `?intent=binding_set&integration=4&kanflow_project_id=kf-p1&resolves_work_items=false&state=${STATE}`;
const REMOVE = `?intent=binding_remove&integration=4&kanflow_project_id=kf-p1&state=${STATE}`;
const CONFIRM = '/api/v1/projects/7/integration-bindings/act-as-confirmation';
const ISSUE = '/-/confirm/projects/7/integration-bindings/assertions';

const view = (over: Record<string, unknown> = {}) => ({
  project_id: 7,
  project_path: 'specs/zeva-engine',
  project_display_name: 'ZEVA engine specs',
  intent: 'binding_set',
  kanflow_project_id: 'kf-p1',
  resolves_work_items: false,
  bound: false,
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
    [`GET ${CONFIRM}`]: () => json(200, view()),
    [`POST ${ISSUE}`]: () => json(201, { status: 'issued' }),
    ...over,
  };
}

const people = new WeakSet<Event>();
beforeEach(() => {
  window.sessionStorage.clear();
  window.history.replaceState(null, '', PAGE);
  vi.spyOn(eventTrust, 'check').mockImplementation((e) => people.has(e));
  vi.spyOn(document, 'hasFocus').mockImplementation(() => true);
  vi.useFakeTimers();
  Object.assign(globalThis, { jest: { advanceTimersByTime: (ms: number) => vi.advanceTimersByTime(ms) } });
});
afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  Reflect.deleteProperty(globalThis, 'jest');
  Object.defineProperty(document, 'visibilityState', { value: 'visible', configurable: true });
});

async function open(table: Record<string, Handler> = routes(), path = `${PAGE}${SET}`, lang: 'ja' | 'en' = 'en') {
  const server = mockServer(table);
  const rendered = await renderApp({ fetch: server.fetch, path, lang });
  return { server, ...rendered };
}

const advance = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));
const setVisible = (visible: boolean) => {
  Object.defineProperty(document, 'visibilityState', { value: visible ? 'visible' : 'hidden', configurable: true });
  act(() => {
    document.dispatchEvent(new Event('visibilitychange'));
  });
};
const arm = async (button: HTMLElement) => {
  setVisible(false);
  setVisible(true);
  await advance(ARM_DELAY_MS);
  const e = new MouseEvent('pointerdown', { bubbles: true });
  people.add(e);
  act(() => {
    button.dispatchEvent(e);
  });
  await advance(ARM_GAP_MS);
};

describe('the binding confirmation', () => {
  it('shows forge facts: the app, the person, the repository, the kanflow project id, the setting and the state now', async () => {
    const { container } = await open();
    const button = await screen.findByRole('button', { name: 'Bind' });
    expect(screen.getByText('kanflow (kanflow-svc)')).toBeTruthy();
    expect(screen.getByText('山田 太郎 (taro)')).toBeTruthy();
    expect(screen.getByText('specs/zeva-engine')).toBeTruthy();
    expect(screen.getByText('kf-p1')).toBeTruthy();
    expect(screen.getByText(/Shared specification repository/)).toBeTruthy();
    expect(screen.getByText('Not bound')).toBeTruthy();
    // No default focus on the button; it is not usable before the arming.
    expect(document.activeElement).not.toBe(button);
    expect(button.getAttribute('aria-disabled')).toBe('true');
    vi.useRealTimers();
    Reflect.deleteProperty(globalThis, 'jest');
    // The same run as the merge request popup's test (jsdom has no canvas for colour contrast).
    const results = await axe.run(container, {
      runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
      rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
    });
    expect(results.violations.map((v) => v.id)).toEqual([]);
  });

  it('sends exactly the confirmed binding, with CSRF, then says done; the state leaves the address', async () => {
    const { server } = await open();
    expect(window.location.href).not.toContain(STATE);
    const button = await screen.findByRole('button', { name: 'Bind' });
    await arm(button);
    fireEvent.click(button);
    expect(await screen.findByRole('heading', { name: 'Done' })).toBeTruthy();
    const [call] = server.to('POST', ISSUE);
    expect(call?.body).toEqual({
      intent: 'binding_set',
      integration: 4,
      kanflow_project_id: 'kf-p1',
      resolves_work_items: false,
      state: STATE,
    });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    // The state never goes to a GET.
    expect(server.calls.filter((c) => c.method === 'GET').some((c) => c.path.includes(STATE) || c.query.has('state'))).toBe(false);
  });

  it('removes a binding the same way (no setting is sent)', async () => {
    const { server } = await open(routes({ [`GET ${CONFIRM}`]: () => json(200, view({ intent: 'binding_remove', resolves_work_items: null, bound: true })) }), `${PAGE}${REMOVE}`);
    const button = await screen.findByRole('button', { name: 'Remove binding' });
    expect(screen.getByText('Bound')).toBeTruthy();
    await arm(button);
    fireEvent.click(button);
    await screen.findByRole('heading', { name: 'Done' });
    expect(server.to('POST', ISSUE)[0]?.body).toEqual({
      intent: 'binding_remove',
      integration: 4,
      kanflow_project_id: 'kf-p1',
      resolves_work_items: null,
      state: STATE,
    });
  });

  it('offers nothing to press when the binding to remove is not there', async () => {
    await open(routes({ [`GET ${CONFIRM}`]: () => json(200, view({ intent: 'binding_remove', resolves_work_items: null, bound: false })) }), `${PAGE}${REMOVE}`);
    expect(await screen.findByRole('heading', { name: 'This link was already used or cannot be used' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Remove binding' })).toBeNull();
  });

  it.each([
    ['a setting on a removal', `?intent=binding_remove&integration=4&kanflow_project_id=kf-p1&resolves_work_items=true&state=${STATE}`],
    ['no setting on a binding', `?intent=binding_set&integration=4&kanflow_project_id=kf-p1&state=${STATE}`],
    ['a merge request intent', `?intent=approve&integration=4&kanflow_project_id=kf-p1&state=${STATE}`],
    ['a kanflow project id that needs escaping', `?intent=binding_set&integration=4&kanflow_project_id=kf%20p1&resolves_work_items=true&state=${STATE}`],
  ])('refuses a malformed link without asking the API (%s)', async (_label, query) => {
    const { server } = await open(routes(), `${PAGE}${query}`);
    expect(await screen.findByRole('heading', { name: 'The link is not valid' })).toBeTruthy();
    expect(server.calls.some((c) => c.path === CONFIRM)).toBe(false);
  });

  it('refuses on 403 with a neutral sentence and the request id (no detail), and says not found on 404', async () => {
    const first = await open(routes({ [`GET ${CONFIRM}`]: () => apiError(403, 'Secret detail about roles.', 'req-403') }));
    expect(await screen.findByRole('heading', { name: 'This cannot be confirmed' })).toBeTruthy();
    expect(screen.queryByText(/Secret detail/)).toBeNull();
    expect(screen.getByTestId('request-id').textContent).toBe('req-403');
    first.unmount();
    window.sessionStorage.clear();
    await open(routes({ [`GET ${CONFIRM}`]: () => apiError(404, 'Not found.', 'req-404') }));
    expect(await screen.findByRole('heading', { name: 'Not found' })).toBeTruthy();
    // ADR-0025 (decision of 2026-11-10): every refusal is this one 404 — the
    // same neutral notice as the merge request's screen, never the server's detail.
    expect(screen.getByText('What this confirmation is for was not found, or this account cannot confirm it. If you think this is wrong, tell an administrator the request ID below.')).toBeTruthy();
    expect(screen.queryByText(/Not found\./)).toBeNull();
    expect(screen.getByTestId('request-id').textContent).toBe('req-404');
  });

  it('says the same neutral not found when the press is answered 404', async () => {
    await open(routes({ [`POST ${ISSUE}`]: () => apiError(404, 'Not found.', 'req-404p') }));
    const button = await screen.findByRole('button', { name: 'Bind' });
    await arm(button);
    fireEvent.click(button);
    expect(await screen.findByRole('heading', { name: 'Not found' })).toBeTruthy();
    expect(screen.getByText('What this confirmation is for was not found, or this account cannot confirm it. If you think this is wrong, tell an administrator the request ID below.')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-404p');
  });

  it('says it in Japanese too', async () => {
    await open(routes(), `${PAGE}${SET}`, 'ja');
    expect(await screen.findByRole('button', { name: '結びつける' })).toBeTruthy();
    expect(screen.getByText(/共有する仕様のリポジトリ/)).toBeTruthy();
  });
});
