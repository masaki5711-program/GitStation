// U2 re-review round 2 (2026-11-03).
// HIGH: no rule can be added before the rule list was read (the name check
//   would pass against an empty list and replace an existing rule).
// HIGH: the reload after a refused action only reads again; it never moves
//   the head the screen showed, so "a new version was pushed" takes over.
// LOW: opening Changes counts a head as seen only once that version's diff
//   was shown (not on the version list, not when the diff failed).
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { settingsRoutes } from './merge-settings-fixtures';
import { apiError, mockServer } from './mock';
import { MR_API, MR_SCREENS, mr, mrRoutes, status, version, versions } from './mr-fixtures';
import { json, renderApp } from './render';

const B = 'b'.repeat(40);
const C = 'c'.repeat(40);
const ALL = { can_approve: true, can_unapprove: true, can_merge: true, can_mark_ready: true, approved_by_me: false };
const APPROVED = { iid: 3, outcome: 'approved', counted: true, uncounted_reason: null, approval: null };

const tab = (name: string) =>
  within(screen.getByRole('navigation', { name: 'Merge request views' })).getByRole('link', { name });

function pushable(opts: { diffFails?: boolean; refuseFirstApprove?: boolean } = {}) {
  let head = B;
  let refused = false;
  const server = mockServer(
    mrRoutes({
      [`GET ${MR_API}/3`]: () => json(200, mr(3, { head_sha: head })),
      [`GET ${MR_API}/3/versions`]: () =>
        json(200, versions(head === B ? [version(1, { head_sha: B })] : [version(1, { head_sha: B }), version(2, { head_sha: C })])),
      [`GET ${MR_API}/3/versions/2/diff`]: () =>
        opts.diffFails === true
          ? apiError(503, 'The changes could not be read.', 'req-diff-1')
          : json(200, { version: version(2, { head_sha: C }), from: B, to: C, commits: [], commits_truncated: false, diff: { truncated: false, files: [] } }),
      [`GET ${MR_API}/3/status`]: () =>
        json(200, { ...status([{ reason: 'unresolved_discussions', detail: 'count=1' }]), head_sha: head, viewer: ALL }),
      [`POST ${MR_API}/3/approve`]: (c) => {
        if (opts.refuseFirstApprove === true && !refused) {
          // A version was pushed between the read and the click.
          refused = true;
          head = C;
        }
        return (c.body as { sha: string }).sha === head
          ? json(200, APPROVED)
          : apiError(409, 'A new version was pushed. Look at it before approving.', 'req-stale-2', 'FG-REVIEW-0008');
      },
    }),
  );
  return { server, push: () => (head = C) };
}

describe('HIGH: adding a rule needs the rule list', () => {
  it('while the list is loading there is no add button', async () => {
    const server = mockServer(
      settingsRoutes({ 'GET /api/v1/projects/7/approval-rules': () => new Promise<Response>(() => undefined) }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    await screen.findByRole('region', { name: 'Approval rules' });
    await screen.findByRole('button', { name: 'Save merge settings' });
    expect(screen.queryByRole('button', { name: 'Add an approval rule' })).toBeNull();
  });

  it('when the list failed there is no add button, and the failure is shown', async () => {
    const server = mockServer(
      settingsRoutes({ 'GET /api/v1/projects/7/approval-rules': () => apiError(503, 'Rules could not be read.', 'req-rules-1') }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    const rules = await screen.findByRole('region', { name: 'Approval rules' });
    expect(await within(rules).findByText('Rules could not be read.')).toBeTruthy();
    expect(within(rules).queryByRole('button', { name: 'Add an approval rule' })).toBeNull();
  });
});

describe('HIGH: the reload after a refusal only reads again', () => {
  it('409 -> reload: approve stays hidden behind the new-version line; reviewing it lets approve name the new head', async () => {
    const { server } = pushable({ refuseFirstApprove: true });
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    fireEvent.click(await screen.findByRole('button', { name: 'Approve' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Reload the merge request' }));
    await waitFor(() => {
      expect(screen.getByTestId('mr-new-version').textContent).toBe(
        'A new version was pushed. Review it before approving or merging.',
      );
    });
    expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
    expect(server.to('POST', `${MR_API}/3/approve`)).toHaveLength(1);
    fireEvent.click(screen.getByRole('button', { name: 'Review the new version' }));
    await screen.findByText('Version 2 (the latest)');
    fireEvent.click(tab('Overview'));
    fireEvent.click(await screen.findByRole('button', { name: 'Approve' }));
    await waitFor(() => {
      expect(server.to('POST', `${MR_API}/3/approve`)).toHaveLength(2);
    });
    expect(server.to('POST', `${MR_API}/3/approve`)[1]?.body).toEqual({ sha: C });
  });
});

describe('LOW: seen means the diff was shown', () => {
  it('a version whose diff failed to load is not counted as seen', async () => {
    const { server, push } = pushable({ diffFails: true });
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    await screen.findByRole('button', { name: 'Approve' });
    push();
    // Once to the Changes tab and back: the version list now has v2 cached.
    fireEvent.click(tab('Changes'));
    await screen.findByText(/Version/);
    fireEvent.click(tab('Overview'));
    await screen.findByTestId('mr-new-version');
    // Changes again: it opens on v2 at once, but its diff cannot be read.
    fireEvent.click(tab('Changes'));
    expect(await screen.findByText('The changes could not be read.')).toBeTruthy();
    fireEvent.click(tab('Overview'));
    await waitFor(() => {
      expect(screen.getByTestId('mr-new-version').textContent).toBe(
        'A new version was pushed. Review it before approving or merging.',
      );
    });
    expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
    expect(server.to('POST', `${MR_API}/3/approve`)).toHaveLength(0);
  });
});
