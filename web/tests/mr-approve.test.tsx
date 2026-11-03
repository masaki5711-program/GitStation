// U2 phase 1: the conditions panel (every reason, in the server's order, the
// one-line status stays first), approve / withdraw with the viewed head and
// the CSRF header, "Mark as ready" for drafts, and buttons decided by the
// `viewer` flags only (never by the role).
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer, SESSION } from './mock';
import { MR_API, MR_SCREENS, mr, mrRoutes, status } from './mr-fixtures';
import { json, renderApp } from './render';

const VIEW = `${MR_SCREENS}/pull/3`;
const HEAD = 'b'.repeat(40);
const CSRF = SESSION.csrf_token;

type Viewer = ReturnType<typeof status>['viewer'];
const viewer = (over: Partial<Viewer> = {}): Viewer => ({
  can_approve: false,
  can_unapprove: false,
  can_merge: false,
  can_mark_ready: false,
  approved_by_me: false,
  ...over,
});

const THREE = [
  { reason: 'approvals_missing', detail: 'rule=@protected_branch required=1 counted=0' },
  { reason: 'required_check_missing', detail: 'name=ci/gates' },
  { reason: 'unresolved_discussions', detail: 'count=2' },
];

function withViewer(reasons: { reason: string; detail: string | null }[], v: Partial<Viewer>, over = {}) {
  return { ...status(reasons), viewer: viewer(v), ...over };
}

async function open(routes: Parameters<typeof mrRoutes>[0] = {}) {
  const server = mockServer(mrRoutes(routes));
  const view = await renderApp({ fetch: server.fetch, path: VIEW, lang: 'en' });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return { server, ...view };
}

const panel = () => screen.findByRole('region', { name: 'Merge conditions' });

describe('the conditions panel', () => {
  it('lists every reason in the server order; the one-line status stays the first', async () => {
    await open({ [`GET ${MR_API}/3/status`]: () => json(200, withViewer(THREE, {})) });
    const region = await panel();
    const items = await within(region).findAllByRole('listitem');
    const lines = items.map((li) => li.textContent);
    expect(lines.slice(0, 3)).toEqual([
      'Not enough approvals (1 needed, 0 given)',
      'The required check "ci/gates" has not reported yet',
      'Unresolved threads: 2',
    ]);
    // The status line (plan 08) is still the first reason only, above the panel.
    const line = screen.getByTestId('mr-status');
    await waitFor(() => {
      expect(line.textContent).toBe('Not enough approvals (1 needed, 0 given)');
    });
    expect(line.compareDocumentPosition(region) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it('a mergeable merge request says every condition is met', async () => {
    await open();
    const region = await panel();
    expect(await within(region).findByText('Every condition is met.')).toBeTruthy();
  });

  it('an unknown reason name is the generic line, not an error', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () => json(200, withViewer([{ reason: 'mystery', detail: null }], {})),
    });
    const region = await panel();
    expect(await within(region).findByText('Cannot be merged right now. Check the changes')).toBeTruthy();
  });

  it('explains why an approval does not count, and shows the rule counts and checks', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () =>
        json(
          200,
          withViewer(THREE.slice(0, 2), {}, {
            approvals: {
              rules: [
                { name: '@protected_branch', applies: true, required: 2, counted: 1 },
                { name: 'Synthetic leads', applies: false, required: 1, counted: 0 },
              ],
              approvals: [
                { approver_id: 8, username: 'jiro', counted: true, uncounted_reason: null },
                { approver_id: 9, username: 'saburo', counted: false, uncounted_reason: 'committer' },
                { approver_id: 10, username: 'shiro', counted: false, uncounted_reason: 'brand_new' },
              ],
              codeowners: [],
            },
            checks: [{ name: 'ci/gates', status: null, conclusion: null }],
            codeowners: { enforced: false, suspended_until: '2026-11-03T05:00:00Z' },
          }),
        ),
    });
    const region = await panel();
    expect(await within(region).findByText('Protected branch: 1 of 2 approvals')).toBeTruthy();
    // A rule that does not apply to this target is not listed as missing.
    expect(within(region).queryByText(/Synthetic leads/)).toBeNull();
    expect(within(region).getByText('jiro: counts')).toBeTruthy();
    expect(
      within(region).getByText('saburo: does not count (they pushed commits of this merge request)'),
    ).toBeTruthy();
    expect(within(region).getByText('shiro: does not count (it does not count now)')).toBeTruthy();
    expect(within(region).getByText('ci/gates: not reported yet')).toBeTruthy();
    expect(within(region).getByText(/CODEOWNERS approval is suspended until/)).toBeTruthy();
  });
});

describe('approve and withdraw', () => {
  it('no flags: no approve, withdraw, merge or ready button, whatever the role', async () => {
    await open({ [`GET ${MR_API}/3/status`]: () => json(200, withViewer(THREE, {})) });
    await panel();
    await screen.findByText('Unresolved threads: 2');
    expect(screen.queryByRole('button', { name: /approve|approval|merge|ready/i })).toBeNull();
  });

  it('approves the head that was read, with the CSRF header, then reads the status again', async () => {
    let approved = false;
    const { server } = await open({
      [`GET ${MR_API}/3/status`]: () =>
        json(200, withViewer(THREE, { can_approve: true, can_unapprove: true, approved_by_me: approved })),
      [`POST ${MR_API}/3/approve`]: () => {
        approved = true;
        return json(200, { iid: 3, outcome: 'approved', counted: true, uncounted_reason: null, approval: null });
      },
    });
    const button = await screen.findByRole('button', { name: 'Approve' });
    const reads = server.to('GET', `${MR_API}/3/status`).length;
    fireEvent.click(button);
    await waitFor(() => {
      expect(server.to('POST', `${MR_API}/3/approve`)).toHaveLength(1);
    });
    const call = server.to('POST', `${MR_API}/3/approve`)[0];
    expect(call?.body).toEqual({ sha: HEAD });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByRole('button', { name: 'Withdraw my approval' })).toBeTruthy();
    expect(server.to('GET', `${MR_API}/3/status`).length).toBeGreaterThan(reads);
    expect(screen.getByTestId('mr-action-result').textContent).toBe('You approved this version.');
    expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
  });

  it('an approval that does not count says why', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () => json(200, withViewer(THREE, { can_approve: true })),
      [`POST ${MR_API}/3/approve`]: () =>
        json(200, {
          iid: 3,
          outcome: 'approved',
          counted: false,
          uncounted_reason: 'not_in_rule',
          approval: null,
        }),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Approve' }));
    await waitFor(() => {
      expect(screen.getByTestId('mr-action-result').textContent).toBe(
        'Your approval was recorded but does not count (no rule asks for this approver).',
      );
    });
  });

  it('withdraw is shown only with can_unapprove and approved_by_me', async () => {
    let approvedByMe = true;
    const { server } = await open({
      [`GET ${MR_API}/3/status`]: () =>
        json(200, withViewer(THREE, { can_approve: true, can_unapprove: true, approved_by_me: approvedByMe })),
      [`POST ${MR_API}/3/unapprove`]: () => {
        approvedByMe = false;
        return json(200, { iid: 3, outcome: 'withdrawn', counted: false, uncounted_reason: null, approval: null });
      },
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Withdraw my approval' }));
    await waitFor(() => {
      expect(server.to('POST', `${MR_API}/3/unapprove`)).toHaveLength(1);
    });
    expect(server.to('POST', `${MR_API}/3/unapprove`)[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await screen.findByRole('button', { name: 'Approve' })).toBeTruthy();
    expect(screen.getByTestId('mr-action-result').textContent).toBe('Your approval was withdrawn.');
  });

  it('can_unapprove without an approval of mine shows no withdraw button', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () => json(200, withViewer(THREE, { can_unapprove: true })),
    });
    await panel();
    await screen.findByText('Unresolved threads: 2');
    expect(screen.queryByRole('button', { name: 'Withdraw my approval' })).toBeNull();
  });

  it('a stale view (409) shows the reason, the request id and a reload that reads everything again', async () => {
    const { server } = await open({
      [`GET ${MR_API}/3/status`]: () => json(200, withViewer(THREE, { can_approve: true })),
      [`POST ${MR_API}/3/approve`]: () =>
        apiError(409, 'A new version was pushed. Look at it before approving.', 'req-stale-1', 'FG-REVIEW-0008'),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Approve' }));
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('A new version was pushed. Look at it before approving.')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-stale-1');
    expect(alert.textContent).not.toContain('FG-');
    const before = server.to('GET', `${MR_API}/3`).length;
    fireEvent.click(screen.getByRole('button', { name: 'Reload the merge request' }));
    await waitFor(() => {
      expect(server.to('GET', `${MR_API}/3`).length).toBeGreaterThan(before);
    });
  });

  it('a refusal (403) shows the reason and request id, without a reload button', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () => json(200, withViewer(THREE, { can_approve: true })),
      [`POST ${MR_API}/3/approve`]: () => apiError(403, 'You cannot approve this.', 'req-403-a', 'FG-AUTH-0003'),
      'GET /api/v1/session': () => json(200, SESSION),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Approve' }));
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('You cannot approve this.')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-403-a');
    expect(screen.queryByRole('button', { name: 'Reload the merge request' })).toBeNull();
  });
});

describe('mark as ready', () => {
  const DRAFT = [{ reason: 'draft', detail: null }, ...THREE];

  it('a draft with can_mark_ready: "next" is the button; PATCH draft=false with CSRF', async () => {
    let draft = true;
    const { server } = await open({
      [`GET ${MR_API}/3`]: () => json(200, mr(3, { draft })),
      [`GET ${MR_API}/3/status`]: () =>
        json(200, withViewer(draft ? DRAFT : THREE, { can_mark_ready: true })),
      [`PATCH ${MR_API}/3`]: () => {
        draft = false;
        return json(200, mr(3, { draft: false }));
      },
    });
    await waitFor(() => {
      expect(screen.getByTestId('mr-status').textContent).toBe('It is still a draft');
    });
    // The hint "the author takes it out of draft" is replaced by the button.
    expect(screen.queryByText('The author takes it out of draft when it is ready.')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Mark as ready' }));
    await waitFor(() => {
      expect(server.to('PATCH', `${MR_API}/3`)).toHaveLength(1);
    });
    const call = server.to('PATCH', `${MR_API}/3`)[0];
    expect(call?.body).toEqual({ draft: false });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    await waitFor(() => {
      expect(screen.getByTestId('mr-status').textContent).toBe('Not enough approvals (1 needed, 0 given)');
    });
    expect(screen.queryByRole('button', { name: 'Mark as ready' })).toBeNull();
  });

  it('a draft without can_mark_ready keeps the hint and has no button', async () => {
    await open({
      [`GET ${MR_API}/3`]: () => json(200, mr(3, { draft: true })),
      [`GET ${MR_API}/3/status`]: () => json(200, withViewer(DRAFT, {})),
    });
    expect(await screen.findByText('The author takes it out of draft when it is ready.')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Mark as ready' })).toBeNull();
  });

  it('can_mark_ready on a merge request that is not a draft shows no button', async () => {
    await open({ [`GET ${MR_API}/3/status`]: () => json(200, withViewer(THREE, { can_mark_ready: true })) });
    await waitFor(() => {
      expect(screen.getByTestId('mr-status').textContent).toBe('Not enough approvals (1 needed, 0 given)');
    });
    expect(screen.queryByRole('button', { name: 'Mark as ready' })).toBeNull();
  });

  it('a refused mark-ready shows the reason and the request id at the status line', async () => {
    await open({
      [`GET ${MR_API}/3`]: () => json(200, mr(3, { draft: true })),
      [`GET ${MR_API}/3/status`]: () => json(200, withViewer(DRAFT, { can_mark_ready: true })),
      [`PATCH ${MR_API}/3`]: () => apiError(409, 'It changed meanwhile.', 'req-ready-409', 'FG-REVIEW-0005'),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Mark as ready' }));
    const area = screen.getByTestId('mr-status-area');
    const alert = await within(area).findByRole('alert');
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-ready-409');
    expect(within(area).getByRole('button', { name: 'Reload the merge request' })).toBeTruthy();
  });
});
