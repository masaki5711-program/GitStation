// U2 re-review (2026-11-03).
// HIGH 1: the head the screen showed is state of the merge request screen
//   (set at the first load, advanced only by an explicit reload / "review the
//   new version" / opening Changes), never by a background refetch. Approve
//   and merge name it; a newer head hides them behind a persistent line.
// HIGH 2: adding an approval rule under a name that exists is refused in the
//   dialog (the PUT would replace that rule).
// LOW: an untouched merge settings form that takes values changed elsewhere
//   says so politely.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { RULE, settingsRoutes } from './merge-settings-fixtures';
import { mockServer } from './mock';
import { MR_API, MR_SCREENS, mergeSettings, mr, mrRoutes, status, version, versions } from './mr-fixtures';
import { json, renderApp } from './render';

const B = 'b'.repeat(40);
const C = 'c'.repeat(40);
const ALL = { can_approve: true, can_unapprove: true, can_merge: true, can_mark_ready: true, approved_by_me: false };
const APPROVED = { iid: 3, outcome: 'approved', counted: true, uncounted_reason: null, approval: null };

/** A server whose merge request head can be moved by a "push". */
function pushable() {
  let head = B;
  const server = mockServer(
    mrRoutes({
      [`GET ${MR_API}/3`]: () => json(200, mr(3, { head_sha: head })),
      [`GET ${MR_API}/3/versions`]: () =>
        json(200, versions(head === B ? [version(1, { head_sha: B })] : [version(1, { head_sha: B }), version(2, { head_sha: C })])),
      [`GET ${MR_API}/3/versions/2/diff`]: () =>
        json(200, { version: version(2, { head_sha: C }), from: B, to: C, commits: [], commits_truncated: false, diff: { truncated: false, files: [] } }),
      [`GET ${MR_API}/3/status`]: () =>
        json(200, { ...status([{ reason: 'unresolved_discussions', detail: 'count=1' }]), head_sha: head, viewer: ALL }),
      [`POST ${MR_API}/3/approve`]: () => json(200, APPROVED),
    }),
  );
  return { server, push: () => (head = C) };
}

const tab = (name: string) =>
  within(screen.getByRole('navigation', { name: 'Merge request views' })).getByRole('link', { name });

describe('HIGH 1: the head the screen showed', () => {
  it('a push read back by a refetch (tabs remount, cache stale) hides approve and shows the persistent line', async () => {
    const { server, push } = pushable();
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    await screen.findByRole('button', { name: 'Approve' });
    push();
    fireEvent.click(tab('Changes'));
    await screen.findByText(/Version 1|Version 2/);
    fireEvent.click(tab('Overview'));
    const line = await screen.findByTestId('mr-new-version');
    await waitFor(() => {
      expect(line.textContent).toBe('A new version was pushed. Review it before approving or merging.');
    });
    expect(line.getAttribute('role')).toBe('status');
    expect(screen.queryByRole('button', { name: 'Approve' })).toBeNull();
    expect(server.to('POST', `${MR_API}/3/approve`)).toHaveLength(0);
  });

  it('"review the new version" takes the user to Changes; back on Overview approve names the new head', async () => {
    const { server, push } = pushable();
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    await screen.findByRole('button', { name: 'Approve' });
    push();
    fireEvent.click(tab('Changes'));
    await screen.findByText(/Version/);
    fireEvent.click(tab('Overview'));
    fireEvent.click(await screen.findByRole('button', { name: 'Review the new version' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe(`${MR_SCREENS}/pull/3/changes`);
    });
    await screen.findByText('Version 2 (the latest)');
    fireEvent.click(tab('Overview'));
    fireEvent.click(await screen.findByRole('button', { name: 'Approve' }));
    await waitFor(() => {
      expect(server.to('POST', `${MR_API}/3/approve`)[0]?.body).toEqual({ sha: C });
    });
  });

  it('never opening Changes: approve names the head shown at the first load', async () => {
    const { server, push } = pushable();
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    const approve = await screen.findByRole('button', { name: 'Approve' });
    push();
    fireEvent.click(approve);
    await waitFor(() => {
      expect(server.to('POST', `${MR_API}/3/approve`)[0]?.body).toEqual({ sha: B });
    });
  });

  it('the line region is there (empty) before any push', async () => {
    const { server } = pushable();
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    await screen.findByRole('button', { name: 'Approve' });
    const line = screen.getByTestId('mr-new-version');
    expect(line.getAttribute('role')).toBe('status');
    expect(line.textContent).toBe('');
  });
});

describe('HIGH 2: a rule name that exists', () => {
  it('is refused in the add dialog with an inline error; nothing is sent', async () => {
    const server = mockServer(settingsRoutes({ 'GET /api/v1/projects/7/approval-rules': () => json(200, { rules: [RULE] }) }));
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    fireEvent.click(await screen.findByRole('button', { name: 'Add an approval rule' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(await within(dialog).findByLabelText('Name'), { target: { value: 'leads' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Save the rule' }));
    expect(await within(dialog).findByText('A rule with this name exists. Change that rule in the list, or choose another name.')).toBeTruthy();
    expect(within(dialog).getByLabelText('Name').getAttribute('aria-invalid')).toBe('true');
    expect(server.calls.filter((c) => c.method === 'PUT')).toHaveLength(0);
  });
});

describe('LOW: settings changed elsewhere', () => {
  it('an untouched form taking new values says so politely', async () => {
    const changed = mergeSettings({
      allowed_methods: ['squash'],
      default_method: 'squash',
      updated_at: '2026-11-03T05:00:00.000001Z',
      viewer: { can_manage: true, can_loosen: true, can_suspend_codeowners: true },
      codeowners: { enforced: false, suspended_at: '2026-11-03T05:00:00Z', suspended_until: '2026-11-03T05:30:00Z' },
    });
    const server = mockServer(
      settingsRoutes({ 'PUT /api/v1/projects/7/merge-settings/codeowners-enforcement': () => json(200, changed) }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    const merging = await screen.findByRole('region', { name: 'Merging' });
    fireEvent.change(await within(merging).findByLabelText('Suspend for (minutes)'), { target: { value: '30' } });
    fireEvent.click(within(merging).getByRole('button', { name: 'Suspend CODEOWNERS' }));
    expect(
      await within(merging).findByText('The merge settings were changed elsewhere. The form now shows them.'),
    ).toBeTruthy();
    expect(within(merging).getByLabelText<HTMLSelectElement>('Default method').value).toBe('squash');
  });
});
