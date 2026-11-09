// M3 U4: the bell's poll with fake timers — every 60 s while answers are
// good, doubling across failures (and back after a success), stopped when the
// session ended, and nothing while the tab is hidden.
import { act, renderHook } from '@testing-library/react';
import { QueryClientProvider, focusManager } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { createApiClient } from '../src/api/client';
import { ApiProvider } from '../src/api/context';
import { POLL_MS, useUnreadCount } from '../src/api/notifications';
import { createQueryClient } from '../src/App';
import { apiError, BELL_PATH } from './mock';
import { json } from './render';

function harness(answers: (() => Response)[]) {
  let calls = 0;
  const fetch = (input: RequestInfo | URL, init?: RequestInit) => {
    const req = input instanceof Request ? input : new Request(input, init);
    if (new URL(req.url).pathname !== BELL_PATH) throw new Error(`unexpected ${req.url}`);
    const answer = answers[Math.min(calls, answers.length - 1)];
    calls += 1;
    return Promise.resolve(answer === undefined ? json(200, { count: 0, capped: false }) : answer());
  };
  const api = createApiClient({ language: () => 'en', fetch });
  const queryClient = createQueryClient();
  const wrapper = ({ children }: { children: ReactNode }) => (
    <ApiProvider client={api}>
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    </ApiProvider>
  );
  renderHook(() => useUnreadCount(), { wrapper });
  return { calls: () => calls };
}

const ok = () => json(200, { count: 1, capped: false });
const down = () => apiError(503, 'Unavailable.', 'req-poll');

/** Advance the clock and let the answers settle. */
async function wait(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

describe('the unread count poll', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    focusManager.setFocused(true);
  });
  afterEach(() => {
    focusManager.setFocused(undefined);
    vi.useRealTimers();
  });

  it('asks every 60 s, backs off across failures and returns to 60 s after a success', async () => {
    const h = harness([ok, down, down, ok, ok]);
    await wait(10);
    expect(h.calls()).toBe(1);
    await wait(POLL_MS);
    expect(h.calls()).toBe(2); // fails: next in 120 s
    await wait(POLL_MS);
    expect(h.calls()).toBe(2);
    await wait(POLL_MS);
    expect(h.calls()).toBe(3); // fails again: next in 240 s
    await wait(3 * POLL_MS);
    expect(h.calls()).toBe(3);
    await wait(POLL_MS);
    expect(h.calls()).toBe(4); // ok: back to 60 s
    await wait(POLL_MS);
    expect(h.calls()).toBe(5);
  });

  it('stops when the session ended', async () => {
    const h = harness([() => apiError(401, 'Sign in.', 'req-401', 'FG-AUTH-0001')]);
    await wait(10);
    expect(h.calls()).toBe(1);
    await wait(30 * POLL_MS);
    expect(h.calls()).toBe(1);
  });

  it('does not ask while the tab is hidden', async () => {
    const h = harness([ok]);
    await wait(10);
    expect(h.calls()).toBe(1);
    act(() => {
      focusManager.setFocused(false);
    });
    await wait(5 * POLL_MS);
    expect(h.calls()).toBe(1);
  });
});
