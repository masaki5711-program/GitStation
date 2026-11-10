// The detail under the merge request's one-line status: the check runs of the
// head on screen (name, namespace, result, summary, link, annotations with
// file:line) and the conflicts predicted with other merge requests. The
// status line stays the primary message; a long list is collapsed; a failure
// shows its user_message and request_id in its place only.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { namespaceOf, orderRuns, safeDetailsUrl, type CheckRun } from '../src/api/checks';
import { locationOf, RUNS_OPEN_MAX } from '../src/features/mr/CheckRuns';
import { apiError, mockServer } from './mock';
import { CHECKS_API, MR_API, MR_SCREENS, checkRun, checkRuns, conflicts, mrRoutes, status } from './mr-fixtures';
import { json, renderApp } from './render';

const VIEW = `${MR_SCREENS}/pull/3`;
const HEAD = 'b'.repeat(40);

async function open(over: Parameters<typeof mrRoutes>[0] = {}, lang: 'en' | 'ja' = 'en') {
  const server = mockServer(mrRoutes(over));
  const view = await renderApp({ fetch: server.fetch, path: VIEW, lang });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return { server, ...view };
}

const section = async (name: string) =>
  (await screen.findByRole('heading', { level: 2, name })).closest('section') as HTMLElement;

const FAILING = checkRun(1, 'ci/build', {
  conclusion: 'failure',
  summary: '2 tests failed',
  details_url: 'https://ci.example.test/runs/1',
  annotations: [
    { path: 'src/lib.rs', start_line: 3, end_line: 4, level: 'failure', message: 'assertion failed', title: 'test' },
    { path: 'README.md', start_line: 0, end_line: 0, level: 'notice', message: 'whole file', title: null },
  ],
});

describe('merge request check runs', () => {
  it('lists the runs of the head: name, namespace, result, summary, link and annotations with file:line', async () => {
    const { server } = await open({
      [`GET ${CHECKS_API}`]: () =>
        json(200, checkRuns([checkRun(2, 'kanflow/acceptance'), FAILING, checkRun(3, 'ci/lint', { status: 'in_progress', conclusion: null })])),
    });
    const box = await section('Check results');
    await within(box).findByText('ci/build');
    expect(server.to('GET', CHECKS_API)).toHaveLength(1);
    expect(within(box).getByText('3 checks: 1 not passed, 1 running, 1 passed')).toBeTruthy();
    // Failing first, then running, then passed.
    const runs = within(box).getByRole('list', { name: 'Check runs' });
    const names = [...runs.children].map((li) => li.querySelector('code')?.textContent);
    expect(names).toEqual(['ci/build', 'ci/lint', 'kanflow/acceptance']);
    expect(within(box).getByText('Did not succeed (failure)')).toBeTruthy();
    expect(within(box).getByText('Running')).toBeTruthy();
    expect(within(box).getByText('Succeeded')).toBeTruthy();
    expect(within(box).getAllByText('Namespace: ci')).toHaveLength(2);
    expect(within(box).getByText('Namespace: kanflow')).toBeTruthy();
    expect(within(box).getByText('2 tests failed')).toBeTruthy();
    const link = within(box).getByRole('link', { name: 'Open the details of ci/build' });
    expect(link.getAttribute('href')).toBe('https://ci.example.test/runs/1');
    expect(link.getAttribute('rel')).toBe('noopener noreferrer');
    // Annotations are collapsed behind their count.
    const summary = within(box).getByText('2 annotations');
    const details = summary.closest('details') as HTMLDetailsElement;
    expect(details.open).toBe(false);
    fireEvent.click(summary);
    const at = within(details).getByRole('link', { name: 'src/lib.rs:3-4' });
    expect(at.getAttribute('href')).toBe(`/acme/forge-web/-/blob/${HEAD}/src/lib.rs#L3`);
    expect(within(details).getByRole('link', { name: 'README.md' }).getAttribute('href')).toBe(
      `/acme/forge-web/-/blob/${HEAD}/README.md`,
    );
    expect(within(details).getByText('Failure')).toBeTruthy();
    expect(within(details).getByText(/assertion failed/)).toBeTruthy();
  });

  it('keeps the one-line status as the primary message, above the detail', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () => json(200, status([{ reason: 'checks_failed', detail: 'ci/build' }])),
      [`GET ${CHECKS_API}`]: () => json(200, checkRuns([FAILING])),
    });
    const line = await screen.findByTestId('mr-status');
    await waitFor(() => {
      expect(line.textContent).not.toBe('');
    });
    const box = await section('Check results');
    expect(line.compareDocumentPosition(box) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(line.getAttribute('role')).toBe('status');
  });

  it('says so when no check has been reported', async () => {
    await open();
    const box = await section('Check results');
    expect(await within(box).findByText('No checks have been reported for this version.')).toBeTruthy();
  });

  it('a refused read shows the reason and the request id in its place only', async () => {
    await open({
      [`GET ${CHECKS_API}`]: () => apiError(500, 'Something went wrong on the server.', 'req-checks-1', 'FG-WEB-0001'),
    });
    const box = await section('Check results');
    expect(await within(box).findByText('Something went wrong on the server.')).toBeTruthy();
    expect(within(box).getByText('req-checks-1')).toBeTruthy();
    // The rest of the page still reads.
    expect(screen.getByRole('heading', { level: 2, name: 'Description' })).toBeTruthy();
  });

  it(`collapses a list longer than ${String(RUNS_OPEN_MAX)} runs behind its counts`, async () => {
    const many = Array.from({ length: RUNS_OPEN_MAX + 1 }, (_, i) => checkRun(i + 1, `ci/job-${String(i + 1)}`));
    await open({ [`GET ${CHECKS_API}`]: () => json(200, checkRuns(many)) });
    const box = await section('Check results');
    const summary = await within(box).findByText(`${String(many.length)} checks: 0 not passed, 0 running, ${String(many.length)} passed`);
    expect(summary.tagName).toBe('SUMMARY');
    expect((summary.closest('details') as HTMLDetailsElement).open).toBe(false);
  });

  it('never shows a link that is not http(s)', async () => {
    await open({
      [`GET ${CHECKS_API}`]: () => json(200, checkRuns([checkRun(1, 'ci/x', { details_url: 'javascript:alert(1)' })])),
    });
    const box = await section('Check results');
    await within(box).findByText('ci/x');
    expect(within(box).queryByRole('link', { name: /Open the details/ })).toBeNull();
  });

  it('helpers: namespace, order, safe link, location', () => {
    expect(namespaceOf('ci/build')).toBe('ci');
    expect(namespaceOf('build')).toBeNull();
    expect(safeDetailsUrl('https://ci.example.test/x')).toBe('https://ci.example.test/x');
    expect(safeDetailsUrl('data:text/html,x')).toBeNull();
    expect(safeDetailsUrl(null)).toBeNull();
    const ordered = orderRuns([
      checkRun(1, 'b', { conclusion: 'success' }),
      checkRun(2, 'a', { status: 'queued', conclusion: null }),
      checkRun(3, 'c', { conclusion: 'action_required' }),
    ] as CheckRun[]);
    expect(ordered.map((r) => r.name)).toEqual(['c', 'a', 'b']);
    expect(locationOf({ path: 'a.rs', start_line: 5, end_line: 5 })).toBe('a.rs:5');
    expect(locationOf({ path: 'a.rs', start_line: 0, end_line: 0 })).toBe('a.rs');
  });
});

describe('merge request predicted conflicts', () => {
  const prediction = (iid: number, over: Record<string, unknown> = {}) => ({
    iid,
    draft: false,
    head_sha: 'c'.repeat(40),
    outcome: 'conflict',
    reason: null,
    conflicts: ['src/lib.rs', 'src/main.rs'],
    conflicts_truncated: false,
    checked_at: '2026-11-03T01:00:00Z',
    ...over,
  });

  it('shows nothing when nothing is predicted', async () => {
    const { server } = await open();
    await section('Check results');
    await waitFor(() => {
      expect(server.to('GET', `${MR_API}/3/conflicts`)).toHaveLength(1);
    });
    expect(screen.queryByRole('heading', { name: 'Conflicts with other merge requests' })).toBeNull();
  });

  it('names the other merge requests and the files, collapsed', async () => {
    await open({
      [`GET ${MR_API}/3/conflicts`]: () =>
        json(200, conflicts([prediction(5, { draft: true }), prediction(6, { outcome: 'failed', reason: 'git_timeout', conflicts: [] })])),
    });
    const box = await section('Conflicts with other merge requests');
    expect(within(box).getByText(/would conflict with 1 other open merge request/)).toBeTruthy();
    const five = within(box).getByRole('link', { name: '!5' });
    expect(five.getAttribute('href')).toBe('/acme/forge-web/-/pull/5');
    expect(within(box).getByText('Draft')).toBeTruthy();
    const paths = within(box).getByText('2 files both change');
    expect((paths.closest('details') as HTMLDetailsElement).open).toBe(false);
    expect(within(box).getByText('src/main.rs')).toBeTruthy();
    expect(within(box).getByRole('link', { name: '!6' })).toBeTruthy();
    expect(within(box).getByText('Could not be predicted.')).toBeTruthy();
  });

  it('a failed scan says so; a refused read shows the reason and the request id', async () => {
    const failed = await open({
      [`GET ${MR_API}/3/conflicts`]: () => json(200, conflicts([], { status: 'failed', reason: 'git_failed' })),
    });
    expect(await within(await section('Conflicts with other merge requests')).findByText(/could not run/)).toBeTruthy();
    failed.unmount();
    await open({
      [`GET ${MR_API}/3/conflicts`]: () => apiError(500, 'Something went wrong on the server.', 'req-conf-1', 'FG-WEB-0001'),
    });
    const box = await section('Conflicts with other merge requests');
    expect(await within(box).findByText('req-conf-1')).toBeTruthy();
  });
});

describe('merge request checks: accessibility', () => {
  it('has no axe violations with runs, annotations and conflicts, in ja and en', async () => {
    for (const lang of ['ja', 'en'] as const) {
      const view = await open(
        {
          [`GET ${CHECKS_API}`]: () => json(200, checkRuns([FAILING, checkRun(2, 'ci/lint')])),
          [`GET ${MR_API}/3/conflicts`]: () =>
            json(200, conflicts([{ iid: 5, draft: false, head_sha: 'c'.repeat(40), outcome: 'conflict', reason: null, conflicts: ['a.rs'], conflicts_truncated: false, checked_at: '2026-11-03T01:00:00Z' }])),
        },
        lang,
      );
      await screen.findByText('ci/build');
      await screen.findByRole('link', { name: '!5' });
      const result = await axe.run(document.querySelector('main') as HTMLElement, {
        runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
        rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
      });
      expect(result.violations.map((v) => v.id)).toEqual([]);
      view.unmount();
    }
  });
});
