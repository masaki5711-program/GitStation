// The merge request hooks over the real client and the route-table mock.
import { focusManager, QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { act, renderHook, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { createApiClient } from '../src/api/client';
import { ApiProvider } from '../src/api/context';
import {
  mrKeys,
  parseIid,
  shouldPollStatus,
  STATUS_POLL_MS,
  useAddComment,
  useCreateMergeRequest,
  useDiscussions,
  useMergeRequest,
  useMergeRequests,
  useMergeStatus,
  useReply,
  useResolve,
  useUnresolve,
  useVersionDiff,
  useVersions,
} from '../src/api/mergeRequests';
import { apiError, mockServer } from './mock';
import { status } from './mr-fixtures';
import { json } from './render';

const BASE = '/api/v1/projects/7/merge-requests';
const MR = { iid: 3, title: 't', state: 'open' };
const DISCUSSIONS = { iid: 3, discussions: [], truncated: false };
const STATUS = status([]);
const NEW_MR = { source_branch: 'f', target_branch: 'main', title: 't' };

function setup(routes: Parameters<typeof mockServer>[0]) {
  const server = mockServer(routes);
  const api = createApiClient({ language: () => 'ja', fetch: server.fetch });
  api.csrf.token = 'tok-123';
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <ApiProvider client={api}>
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    </ApiProvider>
  );
  return { server, queryClient, wrapper };
}

describe('merge request queries', () => {
  it('reads the list with the state as a query parameter', async () => {
    const { server, wrapper } = setup({
      [`GET ${BASE}`]: () => json(200, { merge_requests: [MR], next_before: null }),
    });
    const { result } = renderHook(() => useMergeRequests(7, { state: 'all' }), { wrapper });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.merge_requests[0]?.iid).toBe(3);
    expect(server.calls[0]?.query.get('state')).toBe('all');
  });

  it('reads one MR, its status, discussions and versions', async () => {
    const { server, wrapper } = setup({
      [`GET ${BASE}/3`]: () => json(200, MR),
      [`GET ${BASE}/3/status`]: () => json(200, STATUS),
      [`GET ${BASE}/3/discussions`]: () => json(200, DISCUSSIONS),
      [`GET ${BASE}/3/versions`]: () => json(200, { iid: 3, versions: [] }),
    });
    const { result } = renderHook(
      () => ({
        mr: useMergeRequest(7, 3),
        st: useMergeStatus(7, 3),
        d: useDiscussions(7, 3),
        v: useVersions(7, 3),
      }),
      { wrapper },
    );
    await waitFor(() => {
      expect(result.current.mr.isSuccess && result.current.st.isSuccess).toBe(true);
      expect(result.current.d.isSuccess && result.current.v.isSuccess).toBe(true);
    });
    expect(result.current.st.data?.mergeable).toBe(true);
    expect(server.unmatched).toEqual([]);
  });

  it('reads a version diff', async () => {
    const { server, wrapper } = setup({
      [`GET ${BASE}/3/versions/2/diff`]: () => json(200, { from: 'a', to: 'b', commits: [] }),
    });
    const { result } = renderHook(() => useVersionDiff(7, 3, 2), { wrapper });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(server.unmatched).toEqual([]);
  });

  it('an API failure surfaces as an error the page can show', async () => {
    const { wrapper } = setup({
      [`GET ${BASE}/3/status`]: () => apiError(404, 'not found', 'req-9'),
    });
    const { result } = renderHook(() => useMergeStatus(7, 3), { wrapper });
    await waitFor(() => {
      expect(result.current.isError).toBe(true);
    });
  });
});

describe('status polling (phase 1 review)', () => {
  afterEach(() => {
    vi.useRealTimers();
    focusManager.setFocused(undefined);
  });

  it.each([
    ['mergeability_unknown', true],
    ['merge_in_progress', true],
    ['approvals_missing', false],
    ['conflict', false],
  ])('reasons[0] = %s polls: %s', (reason, polls) => {
    expect(shouldPollStatus(status([{ reason, detail: null }]))).toBe(polls);
  });

  it('a mergeable status or no data does not poll', () => {
    expect(shouldPollStatus(STATUS)).toBe(false);
    expect(shouldPollStatus(undefined)).toBe(false);
    // Only the first reason decides (the one the line shows).
    expect(
      shouldPollStatus(
        status([
          { reason: 'draft', detail: null },
          { reason: 'mergeability_unknown', detail: null },
        ]),
      ),
    ).toBe(false);
  });

  it('reads the status again while it waits, and stops when it is known', async () => {
    // Only the timers the poll uses: React's scheduler runs on setImmediate.
    vi.useFakeTimers({
      shouldAdvanceTime: true,
      toFake: ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval'],
    });
    // jsdom's page is not "visible": polling stops in a hidden tab, so say focused.
    focusManager.setFocused(true);
    let answer = status([{ reason: 'mergeability_unknown', detail: null }]);
    const { server, wrapper } = setup({ [`GET ${BASE}/3/status`]: () => json(200, answer) });
    const { result } = renderHook(() => useMergeStatus(7, 3), { wrapper });
    // Read `data` here: the hook re-renders only for the fields read so far.
    await waitFor(() => {
      expect(result.current.data?.mergeable).toBe(false);
    });
    const first = server.to('GET', `${BASE}/3/status`).length;
    answer = status([]);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(STATUS_POLL_MS * 2);
    });
    expect(server.to('GET', `${BASE}/3/status`).length).toBeGreaterThan(first);
    await waitFor(() => {
      expect(result.current.data?.mergeable).toBe(true);
    });
    const after = server.to('GET', `${BASE}/3/status`).length;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(STATUS_POLL_MS * 3);
    });
    expect(server.to('GET', `${BASE}/3/status`)).toHaveLength(after);
  });
});

describe('iid guard (phase 1 review)', () => {
  it.each([
    ['3', 3],
    ['120', 120],
    ['0', null],
    ['-1', null],
    ['03', null],
    ['3a', null],
    ['1.5', null],
    ['', null],
    ['99999999999999999999', null],
  ])('parseIid(%j) is %j', (raw, want) => {
    expect(parseIid(raw)).toBe(want);
  });

  it('the hooks do not call the API for an invalid iid', async () => {
    const { server, wrapper } = setup({});
    const { result } = renderHook(
      () => ({
        mr: useMergeRequest(7, Number.NaN),
        st: useMergeStatus(7, 0),
        d: useDiscussions(7, -1),
        v: useVersions(7, 1.5),
        diff: useVersionDiff(7, 3, Number.NaN),
      }),
      { wrapper },
    );
    await new Promise((r) => setTimeout(r, 20));
    expect(result.current.mr.fetchStatus).toBe('idle');
    expect(result.current.diff.fetchStatus).toBe('idle');
    expect(server.calls).toEqual([]);
  });
});

describe('merge request mutations', () => {
  it('create posts the body with the CSRF header', async () => {
    const { server, wrapper } = setup({ [`POST ${BASE}`]: () => json(201, MR) });
    const { result } = renderHook(() => useCreateMergeRequest(7), { wrapper });
    await act(async () => {
      await result.current.mutateAsync(NEW_MR);
    });
    const call = server.to('POST', BASE)[0];
    expect(call?.headers.get('X-GitStation-CSRF')).toBe('tok-123');
    expect(call?.body).toEqual(NEW_MR);
  });

  it('create invalidates the MR list', async () => {
    const { queryClient, wrapper } = setup({ [`POST ${BASE}`]: () => json(201, MR) });
    queryClient.setQueryData(mrKeys.list(7, {}), { merge_requests: [] });
    const { result } = renderHook(() => useCreateMergeRequest(7), { wrapper });
    await act(async () => {
      await result.current.mutateAsync(NEW_MR);
    });
    expect(queryClient.getQueryState(mrKeys.list(7, {}))?.isInvalidated).toBe(true);
  });

  it.each([
    ['comment', `${BASE}/3/discussions`, { body: 'hi' }],
    ['reply', `${BASE}/3/discussions/9/notes`, { body: 'hi' }],
    ['resolve', `${BASE}/3/discussions/9/resolve`, null],
    ['unresolve', `${BASE}/3/discussions/9/unresolve`, null],
  ] as const)('%s sends CSRF and refreshes discussions and status', async (kind, path, body) => {
    const { server, queryClient, wrapper } = setup({
      [`POST ${path}`]: () => json(200, { id: 9, notes: [] }),
    });
    queryClient.setQueryData(mrKeys.discussions(7, 3), DISCUSSIONS);
    queryClient.setQueryData(mrKeys.status(7, 3), STATUS);
    const { result } = renderHook(
      () => ({
        comment: useAddComment(7, 3),
        reply: useReply(7, 3),
        resolve: useResolve(7, 3),
        unresolve: useUnresolve(7, 3),
      }),
      { wrapper },
    );
    await act(async () => {
      if (kind === 'comment') await result.current.comment.mutateAsync({ body: 'hi' });
      if (kind === 'reply') await result.current.reply.mutateAsync({ discussionId: 9, body: 'hi' });
      if (kind === 'resolve') await result.current.resolve.mutateAsync(9);
      if (kind === 'unresolve') await result.current.unresolve.mutateAsync(9);
    });
    const call = server.to('POST', path)[0];
    expect(call?.headers.get('X-GitStation-CSRF')).toBe('tok-123');
    expect(call?.body).toEqual(body);
    expect(queryClient.getQueryState(mrKeys.discussions(7, 3))?.isInvalidated).toBe(true);
    expect(queryClient.getQueryState(mrKeys.status(7, 3))?.isInvalidated).toBe(true);
  });

  it('a failed mutation does not invalidate and rejects', async () => {
    const { queryClient, wrapper } = setup({
      [`POST ${BASE}/3/discussions/9/resolve`]: () => apiError(409, 'busy', 'req-1'),
    });
    queryClient.setQueryData(mrKeys.status(7, 3), STATUS);
    const { result } = renderHook(() => useResolve(7, 3), { wrapper });
    await act(async () => {
      await expect(result.current.mutateAsync(9)).rejects.toBeDefined();
    });
    expect(queryClient.getQueryState(mrKeys.status(7, 3))?.isInvalidated).toBe(false);
  });
});
