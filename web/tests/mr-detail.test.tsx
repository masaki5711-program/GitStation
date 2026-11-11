// The merge request detail layout: the header (state, one sentence with the
// author and a relative time, "source → target"), the unresolved-thread
// count on the Overview tab, the side panel (approvals, checks, threads,
// versions, work item, last update) with its loading and failure per row,
// and its place before the detail sections. Only fields the API returns are
// shown (no labels or milestone).
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import type { TFunction } from 'i18next';
import { beforeAll, describe, expect, it } from 'vitest';

import { createI18n } from '../src/i18n';
import { approvalBadge, approvalTotals, checksBadge, checkTotals } from '../src/features/mr/MrSidePanel';
import { openedParts } from '../src/pages/repo/mr/MergeRequestScreen';
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

async function open(
  over: Parameters<typeof mrRoutes>[0] = {},
  path = VIEW,
  lang: 'en' | 'ja' = 'en',
) {
  const server = mockServer(mrRoutes(over));
  const view = await renderApp({ fetch: server.fetch, path, lang });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return { server, ...view };
}

const sidePanel = () => screen.findByRole('complementary', { name: 'About this merge request' });
const row = (id: string) => screen.getByTestId(id);

const check = (name: string, status: string | null, conclusion: string | null, stale = false) => ({
  name,
  status,
  conclusion,
  stale,
  waits_for_merge_result: false,
});

function withApprovalsAndChecks(checks = [check('ci/pipeline', 'completed', 'success'), check('lint', 'in_progress', null)]) {
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
    checks,
    work_item_link: { linked: true },
  };
}

/** A handler that never answers (the query stays pending). */
const never = () => new Promise<Response>(() => undefined);

describe('merge request header', () => {
  it('one sentence with the time and the author, a <time> with the exact moment, and source → target', async () => {
    await open({ [`GET ${MR_API}/3`]: () => json(200, mr(3, { draft: true })) });
    const header = screen.getByRole('heading', { level: 1 }).closest('header');
    if (header === null) throw new Error('no header');
    expect(within(header).getByText('Open')).toBeTruthy();
    expect(within(header).getByText('Draft')).toBeTruthy();
    expect(screen.getByTestId('mr-opened').textContent).toMatch(/^Opened .*ago.* by 佐藤 花子$/);
    const time = screen.getByTestId('mr-opened-at');
    expect(time.tagName).toBe('TIME');
    expect(time.getAttribute('dateTime')).toBe('2026-10-20T01:00:00Z');
    expect(time.getAttribute('title')).not.toBe('');
    const branches = screen.getByTestId('mr-branches');
    expect(within(branches).getByText('dev into main')).toBeTruthy();
    const chips = branches.querySelectorAll('code');
    expect([...chips].map((c) => c.textContent)).toEqual(['dev', 'main']);
    expect(chips[0]?.closest('[aria-hidden="true"]')).not.toBeNull();
  });

  it('follows the language word order (ja: the time first, then the author)', async () => {
    await open({}, VIEW, 'ja');
    const opened = screen.getByTestId('mr-opened');
    expect(opened.textContent).toMatch(/^.+前に 佐藤 花子 が作成$/);
    expect(opened.firstElementChild?.tagName).toBe('TIME');
  });

  it('splits the sentence at the markers only, in the dictionary order', async () => {
    const en = await createI18n('en');
    const sentence = en.t('mr.openedAgo', {
      author: 'author',
      when: 'when',
    });
    expect(openedParts(sentence)).toEqual(['Opened ', 'when', ' by ', 'author']);
    expect(openedParts('<b>x</b> {{y}}')).toEqual(['<b>x</b> {{y}}']);
  });
});

describe('merge request tabs', () => {
  it('the Overview tab counts unresolved threads, the same number as the side panel', async () => {
    await open({
      [`GET ${MR_API}/3/discussions`]: () =>
        json(200, discussions([discussion(1), discussion(2, { resolved: true }), discussion(4)])),
    });
    const tabs = screen.getByRole('navigation', { name: 'Merge request views' });
    const overview = within(tabs).getByRole('link', { name: /^Overview/ });
    await waitFor(() => {
      expect(within(overview).getByTestId('mr-tab-count').textContent).toBe('2');
    });
    expect(overview.textContent).toContain('Unresolved threads: 2');
    expect(row('mr-side-threads').textContent).toBe('2 unresolved of 3');
  });

  it('shows no count when every thread is resolved', async () => {
    await open({
      [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([discussion(1, { resolved: true })])),
    });
    await waitFor(() => {
      expect(row('mr-side-threads').textContent).toBe('0 unresolved of 1');
    });
    const tabs = screen.getByRole('navigation', { name: 'Merge request views' });
    expect(within(tabs).getByRole('link', { name: 'Overview' })).toBeTruthy();
  });
});

describe('merge request side panel', () => {
  it('summarises approvals, checks, threads, versions and the work item in words', async () => {
    const { server } = await open({
      [`GET ${MR_API}/3/status`]: () => json(200, withApprovalsAndChecks()),
      [`GET ${MR_API}/3/versions`]: () =>
        json(200, versions([version(1), version(2, { head_sha: 'b'.repeat(40) })])),
    });
    const side = await sidePanel();
    await waitFor(() => {
      expect(row('mr-side-approvals').textContent).toContain('Approvals 1/2, 1 more needed');
    });
    const people = within(side).getByRole('list', { name: 'Approved by' });
    expect(within(people).getByText('taro')).toBeTruthy();
    expect(within(people).getByText('jiro (does not count)')).toBeTruthy();
    // One passed, one still running: said as "running or waiting", neutral.
    const checks = row('mr-side-checks');
    expect(checks.textContent).toBe('1 running or waiting (checks 1/2 passed)');
    expect(checks.querySelector('[data-badge]')?.getAttribute('data-badge')).toBe('draft');
    await waitFor(() => {
      expect(row('mr-side-versions').textContent).toContain('Latest commit bbbbbbbb');
    });
    expect(row('mr-side-work-item').textContent).toBe('Linked');
    // The panel asks nothing of its own.
    expect(server.to('GET', `${MR_API}/3/status`)).toHaveLength(1);
    expect(server.to('GET', `${MR_API}/3/versions`)).toHaveLength(1);
    expect(server.unmatched).toEqual([]);
  });

  it('a failed check says how many failed; a stale run is not passed', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () =>
        json(
          200,
          withApprovalsAndChecks([
            check('a', 'completed', 'success'),
            check('b', 'completed', 'failure'),
            check('c', 'completed', 'success', true),
          ]),
        ),
    });
    await waitFor(() => {
      expect(row('mr-side-checks').textContent).toBe('1 failed (checks 1/3 passed)');
    });
    expect(row('mr-side-checks').querySelector('[data-badge]')?.getAttribute('data-badge')).toBe('unresolved');
  });

  it('says when nothing is required and leaves out a work item row the API does not give', async () => {
    await open();
    await waitFor(() => {
      expect(row('mr-side-approvals').textContent).toBe('Not required');
    });
    expect(row('mr-side-checks').textContent).toBe('None required');
    expect(screen.queryByTestId('mr-side-work-item')).toBeNull();
    await waitFor(() => {
      expect(row('mr-side-threads').textContent).toBe('None');
    });
  });

  it('each row shows a loading placeholder while its source is read', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: never,
      [`GET ${MR_API}/3/versions`]: never,
      [`GET ${MR_API}/3/discussions`]: never,
    });
    await sidePanel();
    for (const id of ['mr-side-approvals', 'mr-side-checks', 'mr-side-threads', 'mr-side-versions']) {
      expect(row(id).textContent).toBe('Loading…');
    }
  });

  it('a failed status read: a compact error with request_id and retry; the status line holds the one alert', async () => {
    let fail = true;
    const { server } = await open({
      [`GET ${MR_API}/3/status`]: () =>
        fail ? apiError(503, '状態を読めませんでした', 'req-side-1') : json(200, status([])),
    });
    const err = await within(await sidePanel()).findByTestId('mr-side-status-error');
    expect(within(err).getByText('状態を読めませんでした')).toBeTruthy();
    expect(within(err).getByTestId('inline-request-id').textContent).toBe('req-side-1');
    expect(screen.getAllByRole('alert')).toHaveLength(1);
    fail = false;
    const before = server.to('GET', `${MR_API}/3/status`).length;
    fireEvent.click(within(err).getByRole('button', { name: 'Try again' }));
    await waitFor(() => {
      expect(row('mr-side-approvals').textContent).toBe('Not required');
    });
    expect(server.to('GET', `${MR_API}/3/status`).length).toBeGreaterThan(before);
  });

  it('a failed versions read and a failed threads read each show their own error in their row', async () => {
    await open({
      [`GET ${MR_API}/3/versions`]: () => apiError(503, '版を読めませんでした', 'req-ver-1'),
      [`GET ${MR_API}/3/discussions`]: () => apiError(503, 'スレッドを読めませんでした', 'req-thr-1'),
    });
    await sidePanel();
    await waitFor(() => {
      expect(within(row('mr-side-versions')).getByTestId('inline-request-id').textContent).toBe('req-ver-1');
    });
    expect(within(row('mr-side-versions')).getByText('版を読めませんでした')).toBeTruthy();
    await waitFor(() => {
      expect(within(row('mr-side-threads')).getByTestId('inline-request-id').textContent).toBe('req-thr-1');
    });
    expect(within(row('mr-side-threads')).getByRole('button', { name: 'Try again' })).toBeTruthy();
  });

  it('comes before the detail sections in the document (first on a narrow screen)', async () => {
    await open();
    const side = await sidePanel();
    const conditions = screen.getByRole('heading', { level: 2, name: 'Merge conditions' });
    expect(side.compareDocumentPosition(conditions) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
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

describe('side panel sums and words', () => {
  let t: TFunction;
  beforeAll(async () => {
    t = (await createI18n('en')).t;
  });

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

  it('a stale, running or unreported check is pending, never passed', () => {
    expect(
      checkTotals({
        checks: [
          check('a', 'completed', 'success'),
          check('b', 'completed', 'success', true),
          check('c', 'completed', 'failure'),
          check('d', null, null),
          check('e', 'queued', null),
        ],
      }),
    ).toEqual({ passed: 1, failed: 1, pending: 3 });
  });

  it('badges carry the state in words', () => {
    expect(approvalBadge(t, { counted: 2, required: 2 })).toEqual({ tone: 'open', label: 'Approved 2/2' });
    expect(approvalBadge(t, { counted: 0, required: 2 }).label).toBe('Approvals 0/2, 2 more needed');
    expect(checksBadge(t, { passed: 3, failed: 0, pending: 0 })).toEqual({
      tone: 'open',
      label: 'Checks 3/3 passed',
    });
    expect(checksBadge(t, { passed: 1, failed: 1, pending: 1 }).tone).toBe('unresolved');
    expect(checksBadge(t, { passed: 1, failed: 0, pending: 2 }).label).toBe(
      '2 running or waiting (checks 1/3 passed)',
    );
  });
});
