// U2 phase 2: merge. A confirmation dialog that is also the method choice
// (only the allowed methods, the default selected), POST merge (202), then the
// attempt is read with a backoff of 1 s up to 5 s until it ends, at most for 2
// minutes ("still processing, it continues if you leave" + request id). The
// end is one line; failures show user_message / request_id, never error_code.
import { act, fireEvent, screen, waitFor, within } from '@testing-library/react';
import { focusManager } from '@tanstack/react-query';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { MERGE_POLL_GIVE_UP_MS, mergePollDelay } from '../src/api/mergeRequests';
import { apiError, mockServer, SESSION } from './mock';
import { MR_API, MR_SCREENS, attempt, mergeSettings, mr, mrRoutes, status } from './mr-fixtures';
import { json, renderApp } from './render';

const VIEW = `${MR_SCREENS}/pull/3`;
const HEAD = 'b'.repeat(40);
const SETTINGS_API = '/api/v1/projects/7/merge-settings';

const mergeable = (can_merge: boolean, ok = true) => ({
  ...status(ok ? [] : [{ reason: 'conflict', detail: null }]),
  viewer: { can_approve: false, can_unapprove: false, can_merge, can_mark_ready: false, approved_by_me: false },
});

async function open(routes: Parameters<typeof mrRoutes>[0] = {}) {
  const server = mockServer(
    mrRoutes({
      [`GET ${MR_API}/3/status`]: () => json(200, mergeable(true)),
      [`GET ${SETTINGS_API}`]: () => json(200, mergeSettings()),
      ...routes,
    }),
  );
  const view = await renderApp({ fetch: server.fetch, path: VIEW, lang: 'en' });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return { server, ...view };
}

const progress = () => screen.getByTestId('mr-merge-progress');

describe('mergePollDelay', () => {
  it('backs off 1 s, 2 s, 4 s, then 5 s; stops when finished or after 2 minutes', () => {
    const q = { state: 'queued' };
    expect([1, 2, 3, 4, 9].map((n) => mergePollDelay(n, 0, q))).toEqual([1000, 2000, 4000, 5000, 5000]);
    expect(mergePollDelay(0, 0, q)).toBe(1000);
    for (const s of ['applied', 'superseded', 'failed']) expect(mergePollDelay(1, 0, { state: s })).toBe(false);
    expect(mergePollDelay(3, MERGE_POLL_GIVE_UP_MS, q)).toBe(false);
    expect(mergePollDelay(3, MERGE_POLL_GIVE_UP_MS - 1, q)).toBe(4000);
  });
});

describe('the merge button', () => {
  it('is absent without can_merge, even when mergeable', async () => {
    await open({ [`GET ${MR_API}/3/status`]: () => json(200, mergeable(false)) });
    await screen.findByText('Every condition is met.');
    expect(screen.queryByRole('button', { name: /merge/i })).toBeNull();
  });

  it('is absent while something blocks the merge', async () => {
    await open({ [`GET ${MR_API}/3/status`]: () => json(200, mergeable(true, false)) });
    await screen.findByRole('region', { name: 'Merge conditions' });
    await waitFor(() => {
      expect(screen.getByTestId('mr-status').textContent).toBe('It conflicts with other changes');
    });
    expect(screen.queryByRole('button', { name: /merge/i })).toBeNull();
  });
});

describe('the merge dialog', () => {
  it('offers only the allowed methods with the default chosen; Cancel returns the focus', async () => {
    const { server } = await open({
      [`GET ${SETTINGS_API}`]: () =>
        json(200, mergeSettings({ allowed_methods: ['squash', 'fast_forward'], default_method: 'fast_forward', delete_source_default: true })),
    });
    const button = await screen.findByRole('button', { name: 'Merge…' });
    fireEvent.click(button);
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByRole('heading', { name: 'Merge this merge request?' })).toBeTruthy();
    const radios = await within(dialog).findAllByRole('radio');
    expect(radios.map((r) => r.closest('label')?.textContent)).toEqual(['Squash', 'Fast-forward']);
    expect(within(dialog).getByLabelText<HTMLInputElement>('Fast-forward').checked).toBe(true);
    expect(within(dialog).queryByLabelText('Merge commit')).toBeNull();
    expect(within(dialog).getByLabelText<HTMLInputElement>('Delete the source branch dev after the merge').checked).toBe(true);
    expect(dialog.contains(document.activeElement)).toBe(true);
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).toBeNull();
    });
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Merge…' }));
    });
    expect(server.to('POST', `${MR_API}/3/merge`)).toHaveLength(0);
  });

  it('a stale view (409) stays in the dialog with the reason, the request id and a reload', async () => {
    await open({
      [`POST ${MR_API}/3/merge`]: () =>
        apiError(409, 'A new version was pushed. Look at it before merging.', 'req-merge-409', 'FG-REVIEW-0008'),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Merge…' }));
    const dialog = await screen.findByRole('dialog');
    await within(dialog).findAllByRole('radio');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Merge' }));
    const alert = await within(dialog).findByRole('alert');
    expect(within(alert).getByText('A new version was pushed. Look at it before merging.')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-merge-409');
    expect(alert.textContent).not.toContain('FG-');
    expect(within(dialog).getByRole('button', { name: 'Reload the merge request' })).toBeTruthy();
  });

  it('a refusal (403) stays in the dialog with its reason', async () => {
    await open({
      [`POST ${MR_API}/3/merge`]: () => apiError(403, 'You may not merge into main.', 'req-merge-403', 'FG-AUTH-0003'),
      'GET /api/v1/session': () => json(200, SESSION),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Merge…' }));
    const dialog = await screen.findByRole('dialog');
    await within(dialog).findAllByRole('radio');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Merge' }));
    const alert = await within(dialog).findByRole('alert');
    expect(within(alert).getByText('You may not merge into main.')).toBeTruthy();
    expect(within(dialog).queryByRole('button', { name: 'Reload the merge request' })).toBeNull();
  });
});

describe('merge progress', () => {
  afterEach(() => {
    vi.useRealTimers();
    focusManager.setFocused(undefined);
  });

  const fake = (date = false) => {
    vi.useFakeTimers({
      shouldAdvanceTime: true,
      toFake: ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval', ...(date ? (['Date'] as const) : [])],
    });
    focusManager.setFocused(true);
  };

  async function startMerge(states: Record<string, unknown>[], extra: Parameters<typeof mrRoutes>[0] = {}) {
    let reads = 0;
    let merging = false;
    const opened = await open({
      [`GET ${MR_API}/3/status`]: () =>
        json(200, merging ? { ...mergeable(true, false), reasons: [{ reason: 'merge_in_progress', detail: null }] } : mergeable(true)),
      [`POST ${MR_API}/3/merge`]: () => {
        merging = true;
        return json(202, attempt('queued'));
      },
      [`GET ${MR_API}/3/merge`]: () => {
        const next = states[Math.min(reads, states.length - 1)];
        reads += 1;
        return json(200, { iid: 3, merge: next });
      },
      ...extra,
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Merge…' }));
    const dialog = await screen.findByRole('dialog');
    await within(dialog).findAllByRole('radio');
    fireEvent.click(within(dialog).getByLabelText('Squash'));
    fireEvent.click(within(dialog).getByLabelText('Delete the source branch dev after the merge'));
    fireEvent.click(within(dialog).getByRole('button', { name: 'Merge' }));
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).toBeNull();
    });
    return opened;
  }

  const tick = async (ms: number) => {
    await act(async () => {
      await vi.advanceTimersByTimeAsync(ms);
    });
  };

  it('posts the viewed head, the method and the branch choice with CSRF, then follows the attempt to applied', async () => {
    fake();
    const { server } = await startMerge([
      attempt('executing'),
      attempt('applied', { new_sha: 'c'.repeat(40), source_outcome: 'kept_refused', finished_at: '2026-11-03T01:00:05Z' }),
    ]);
    const call = server.to('POST', `${MR_API}/3/merge`)[0];
    expect(call?.body).toEqual({ sha: HEAD, method: 'squash', delete_source_branch: true });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    // The live region stays mounted; it says each state once.
    const region = progress();
    expect(region.getAttribute('aria-live')).toBe('polite');
    expect(region.textContent).toContain('The merge is queued.');
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByTestId('mr-merge-progress'));
    });
    await tick(1100);
    await waitFor(() => {
      expect(progress().textContent).toContain('Merging…');
    });
    await tick(2100);
    await waitFor(() => {
      expect(progress().textContent).toContain('Merged into main.');
    });
    expect(progress().textContent).toContain('The source branch was kept: you may not delete it.');
    const reads = server.to('GET', `${MR_API}/3/merge`).length;
    await tick(10_000);
    expect(server.to('GET', `${MR_API}/3/merge`)).toHaveLength(reads);
  });

  it.each([
    ['target_moved', 'The target branch moved meanwhile. Merge again.'],
    ['conflict', 'It now conflicts with the target. Push a fix, then merge again.'],
    ['job_failed', 'The merge could not be finished. Merge again; if it repeats, tell an administrator with the request ID.'],
    ['method_not_allowed', 'That merge method is no longer allowed. Choose another one and merge again.'],
    ['something_new', 'The merge was stopped. Look at the merge request and merge again.'],
  ])('superseded %s is one line with the request id, never the error code', async (reason, line) => {
    fake();
    await startMerge([attempt('superseded', { reason, error_code: 'FG-REVIEW-0003', finished_at: '2026-11-03T01:00:05Z' })]);
    await tick(1100);
    await waitFor(() => {
      expect(progress().textContent).toContain(line);
    });
    expect(progress().textContent).toContain('req-merge-1');
    expect(document.body.textContent).not.toContain('FG-REVIEW');
  });

  it('failed says so with the request id', async () => {
    fake();
    await startMerge([attempt('failed', { reason: 'git_failed', error_code: 'FG-GIT-0003' })]);
    await tick(1100);
    await waitFor(() => {
      expect(progress().textContent).toContain('The merge failed. Tell an administrator with the request ID.');
    });
    expect(progress().textContent).toContain('req-merge-1');
    expect(document.body.textContent).not.toContain('FG-GIT');
  });

  it('after 2 minutes it stops asking: still processing, it continues if you leave', async () => {
    fake(true);
    const { server } = await startMerge([attempt('executing')]);
    for (let i = 0; i < 30; i += 1) await tick(5000);
    await waitFor(() => {
      expect(progress().textContent).toContain('Still processing. It continues even if you leave this page.');
    });
    expect(progress().textContent).toContain('req-merge-1');
    const reads = server.to('GET', `${MR_API}/3/merge`).length;
    expect(reads).toBeLessThan(40);
    await tick(30_000);
    expect(server.to('GET', `${MR_API}/3/merge`)).toHaveLength(reads);
  });

  it('a merge already running when the page opens is followed too', async () => {
    fake();
    await open({
      [`GET ${MR_API}/3/status`]: () =>
        json(200, { ...mergeable(true, false), reasons: [{ reason: 'merge_in_progress', detail: null }] }),
      [`GET ${MR_API}/3/merge`]: () => json(200, { iid: 3, merge: attempt('executing') }),
    });
    await waitFor(() => {
      expect(progress().textContent).toContain('Merging…');
    });
  });

  it('applied with the branch deleted, and a merged merge request reads again', async () => {
    fake();
    let merged = false;
    const { server } = await startMerge(
      [attempt('applied', { source_outcome: 'deleted' })],
      {
        [`GET ${MR_API}/3`]: () => json(200, mr(3, merged ? { state: 'merged' } : {})),
      },
    );
    merged = true;
    const before = server.to('GET', `${MR_API}/3`).length;
    await tick(1100);
    await waitFor(() => {
      expect(progress().textContent).toContain('The source branch was deleted.');
    });
    await waitFor(() => {
      expect(server.to('GET', `${MR_API}/3`).length).toBeGreaterThan(before);
    });
  });
});
