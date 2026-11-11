// The merge request detail layout: the header (state, author, relative time,
// "source → target"), the thread count on the Overview tab, the side panel
// (approvals, checks, threads, versions, work item, last update) and its
// states. Only fields the API returns are shown (no labels or milestone).
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { approvalTotals, checkTotals } from '../src/features/mr/MrSidePanel';
import { apiError, mockServer } from './mock';
import {
  MR_API,
  MR_SCREENS,
  discussion,
  discussions,
  mr,
  mrRoutes,
  status,
  version,
  versions,
} from './mr-fixtures';
import { json, renderApp } from './render';

const VIEW = `${MR_SCREENS}/pull/3`;

async function open(over: Parameters<typeof mrRoutes>[0] = {}, path = VIEW) {
  const server = mockServer(mrRoutes(over));
  const view = await renderApp({ fetch: server.fetch, path, lang: 'en' });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return { server, ...view };
}

const sidePanel = () => screen.findByRole('complementary', { name: 'About this merge request' });

function withApprovalsAndChecks() {
  return {
    ...status([{ reason: 'approvals_missing', detail: 'rule=@protected_branch required=2 counted=1' }]),
    approvals: {
      rules: [
        { name: '@protected_branch', applies: true, required: 2, counted: 1 },
        { name: 'docs', applies: false, required: 1, counted: 0 },
      ],
      approvals: [
        { approver_id: 8, username: 'taro', counted: true, uncounted_reason: null },
        { approver_id: 9, username: 'jiro', counted: false, uncounted_reason: 'committer' },
      ],
      codeowners: [],
    },
    checks: [
      { name: 'ci/pipeline', status: 'completed', conclusion: 'success', stale: false, waits_for_merge_result: false },
      { name: 'lint', status: 'in_progress', conclusion: null, stale: false, waits_for_merge_result: false },
    ],
    work_item_link: { linked: true },
  };
}

describe('merge request header', () => {
  it('shows the state, the author, a relative time with the exact moment, and source → target', async () => {
    await open({ [`GET ${MR_API}/3`]: () => json(200, mr(3, { draft: true })) });
    const header = screen.getByRole('heading', { level: 1 }).closest('header');
    if (header === null) throw new Error('no header');
    expect(within(header).getByText('Open')).toBeTruthy();
    expect(within(header).getByText('Draft')).toBeTruthy();
    expect(within(header).getByText('Opened by 佐藤 花子')).toBeTruthy();
    const time = screen.getByTestId('mr-opened-at');
    expect(time.tagName).toBe('TIME');
    expect(time.getAttribute('dateTime')).toBe('2026-10-20T01:00:00Z');
    expect(time.getAttribute('title')).not.toBe('');
    // Relative ("… ago"), not the date itself.
    expect(time.textContent).toMatch(/ago|yesterday|last|now/);
    const branches = screen.getByTestId('mr-branches');
    // The sentence for a screen reader; the chips are decoration.
    expect(within(branches).getByText('dev into main')).toBeTruthy();
    const chips = branches.querySelectorAll('code');
    expect([...chips].map((c) => c.textContent)).toEqual(['dev', 'main']);
    expect(chips[0]?.closest('[aria-hidden="true"]')).not.toBeNull();
  });
});

describe('merge request tabs', () => {
  it('the Overview tab carries the thread count (words for a screen reader); none when there are no threads', async () => {
    await open({
      [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([discussion(1), discussion(2)])),
    });
    const tabs = screen.getByRole('navigation', { name: 'Merge request views' });
    const overview = await within(tabs).findByRole('link', { name: /^Overview/ });
    await waitFor(() => {
      expect(overview.textContent).toContain('Threads: 2');
    });
    expect(overview.getAttribute('aria-current')).toBe('page');
    expect(within(tabs).getByRole('link', { name: 'Changes' })).toBeTruthy();
  });

  it('shows no count without threads', async () => {
    await open();
    await screen.findByText('No comments yet.');
    const tabs = screen.getByRole('navigation', { name: 'Merge request views' });
    expect(within(tabs).getByRole('link', { name: 'Overview' })).toBeTruthy();
  });
});

describe('merge request side panel', () => {
  it('summarises approvals, approvers, checks, threads, versions and the work item from the API', async () => {
    const { server } = await open({
      [`GET ${MR_API}/3/status`]: () => json(200, withApprovalsAndChecks()),
      [`GET ${MR_API}/3/discussions`]: () =>
        json(200, discussions([discussion(1), discussion(2, { resolved: true })])),
      [`GET ${MR_API}/3/versions`]: () =>
        json(200, versions([version(1), version(2, { head_sha: 'b'.repeat(40) })])),
    });
    const side = await sidePanel();
    await waitFor(() => {
      expect(within(side).getByTestId('mr-side-approvals').textContent).toContain('1 of 2');
    });
    const people = within(side).getByRole('list', { name: 'Approved by' });
    expect(within(people).getByText('taro')).toBeTruthy();
    expect(within(people).getByText('jiro (does not count)')).toBeTruthy();
    // One passed, one still running: neutral, not failed.
    const checks = within(side).getByTestId('mr-side-checks');
    expect(checks.textContent).toBe('1 of 2 passed');
    expect(checks.querySelector('[data-badge]')?.getAttribute('data-badge')).toBe('draft');
    await waitFor(() => {
      expect(within(side).getByTestId('mr-side-threads').textContent).toBe('1 unresolved of 2');
    });
    await waitFor(() => {
      expect(within(side).getByTestId('mr-side-versions').textContent).toContain('2');
    });
    expect(within(side).getByTestId('mr-side-versions').textContent).toContain('Latest commit bbbbbbbb');
    expect(within(side).getByTestId('mr-side-work-item').textContent).toBe('Linked');
    expect(within(side).getByText('Updated')).toBeTruthy();
    // The panel asks nothing of its own: the queries the page reads anyway.
    expect(server.to('GET', `${MR_API}/3/status`)).toHaveLength(1);
    expect(server.to('GET', `${MR_API}/3/versions`)).toHaveLength(1);
    expect(server.unmatched).toEqual([]);
  });

  it('says when nothing is required and leaves out a work item row the API does not give', async () => {
    await open();
    const side = await sidePanel();
    await waitFor(() => {
      expect(within(side).getByTestId('mr-side-approvals').textContent).toBe('Not required');
    });
    expect(within(side).getByTestId('mr-side-checks').textContent).toBe('None required');
    expect(within(side).queryByTestId('mr-side-work-item')).toBeNull();
    await waitFor(() => {
      expect(within(side).getByTestId('mr-side-threads').textContent).toBe('None');
    });
  });

  it('a failed status read: the panel points at the status line, which alone holds the alert', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () => apiError(503, '状態を読めませんでした', 'req-side-1'),
    });
    const side = await sidePanel();
    expect(
      await within(side).findByText('Could not be read. The line at the top shows why.'),
    ).toBeTruthy();
    const area = screen.getByTestId('mr-status-area');
    const alert = await within(area).findByRole('alert');
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-side-1');
    expect(screen.getAllByRole('alert')).toHaveLength(1);
    // The rest of the panel still shows.
    expect(within(side).getByText('Updated')).toBeTruthy();
  });

  it('the Changes tab keeps the whole width (no side panel)', async () => {
    await open({}, `${VIEW}/changes`);
    await screen.findByText('Version 1 (the latest)');
    expect(screen.queryByTestId('mr-side')).toBeNull();
  });
});

describe('merge request states', () => {
  it('shows loading while the merge request is read', async () => {
    const waiting: ((r: Response) => void)[] = [];
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}/3`]: () =>
          new Promise<Response>((resolve) => {
            waiting.push(resolve);
          }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: VIEW, lang: 'en' });
    await waitFor(() => {
      expect(waiting.length).toBeGreaterThan(0);
    });
    expect(screen.getByText('Loading…')).toBeTruthy();
    expect(screen.queryByRole('heading', { level: 1, name: /Synthetic change 3/ })).toBeNull();
    for (const answer of waiting) answer(json(200, mr(3)));
    expect(await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ })).toBeTruthy();
  });

  it('a failed read shows user_message and request_id, and "try again" reads it again', async () => {
    let fail = true;
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}/3`]: () =>
          fail ? apiError(503, '読み込めませんでした', 'req-mr-503') : json(200, mr(3)),
      }),
    );
    await renderApp({ fetch: server.fetch, path: VIEW, lang: 'en' });
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('読み込めませんでした')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-mr-503');
    fail = false;
    fireEvent.click(within(alert).getByRole('button', { name: 'Try again' }));
    expect(await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ })).toBeTruthy();
  });
});

describe('side panel sums', () => {
  it('counts each applying rule up to what it needs', () => {
    expect(
      approvalTotals({
        approvals: {
          rules: [
            { name: 'a', applies: true, required: 1, counted: 3 },
            { name: 'b', applies: true, required: 2, counted: 1 },
            { name: 'c', applies: false, required: 5, counted: 0 },
          ],
          approvals: [],
          codeowners: [],
        },
      }),
    ).toEqual({ counted: 2, required: 3 });
    expect(approvalTotals({ approvals: null })).toEqual({ counted: 0, required: 0 });
  });

  it('a stale or unreported check is never passed', () => {
    const c = (status: string | null, conclusion: string | null, stale = false) => ({
      name: 'x',
      status,
      conclusion,
      stale,
      waits_for_merge_result: false,
    });
    expect(
      checkTotals({
        checks: [
          c('completed', 'success'),
          c('completed', 'success', true),
          c('completed', 'failure'),
          c(null, null),
          c('queued', null),
        ],
      }),
    ).toEqual({ passed: 1, failed: 1 });
  });
});
