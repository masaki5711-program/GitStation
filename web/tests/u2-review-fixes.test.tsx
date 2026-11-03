// U2 independent review (2026-11-03): the fixes, each pinned by a test.
// HIGH 1: approve / merge send the head of the version on screen (the
//   versions the Changes tab renders), never a status head read later; when
//   the two differ the actions hide behind "a new version was pushed — reload".
// HIGH 2: an approval rule dialog never offers Save before the approver names
//   were read (nor when they failed), so a PUT cannot drop the approvers.
// HIGH 3: Enter in the user search runs the search, not the rule form.
// MEDIUM focus: dialogs give the focus back to their opener; saving the merge
//   settings keeps the focus on Save.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { RULE, settingsRoutes } from './merge-settings-fixtures';
import { mockServer } from './mock';
import { MR_API, MR_SCREENS, attempt, mergeSettings, mrRoutes, status, version, versions } from './mr-fixtures';
import { json, renderApp } from './render';

const B = 'b'.repeat(40);
const C = 'c'.repeat(40);
const ALL = { can_approve: true, can_unapprove: true, can_merge: true, can_mark_ready: true, approved_by_me: false };

const at = (head: string, reasons: { reason: string; detail: string | null }[] = []) => ({
  ...status(reasons),
  head_sha: head,
  viewer: ALL,
});

describe('HIGH 1: the head that was on screen', () => {
  it('a push after the versions were read hides approve and merge and offers a reload; no request names the new head', async () => {
    let viewed = B;
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}/3/versions`]: () => json(200, versions([version(1, { head_sha: viewed })])),
        [`GET ${MR_API}/3/status`]: () => json(200, at(C)),
        'GET /api/v1/projects/7/merge-settings': () => json(200, mergeSettings()),
        [`POST ${MR_API}/3/approve`]: () =>
          json(200, { iid: 3, outcome: 'approved', counted: true, uncounted_reason: null, approval: null }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    const region = await screen.findByRole('region', { name: 'Merge conditions' });
    expect(
      await within(region).findByText('A new version was pushed. Reload the merge request to see it before approving or merging.'),
    ).toBeTruthy();
    expect(within(region).queryByRole('button', { name: 'Approve' })).toBeNull();
    expect(within(region).queryByRole('button', { name: 'Merge…' })).toBeNull();
    // Reading again shows the new version; then approving names it.
    viewed = C;
    fireEvent.click(within(region).getByRole('button', { name: 'Reload the merge request' }));
    fireEvent.click(await within(region).findByRole('button', { name: 'Approve' }));
    await waitFor(() => {
      expect(server.to('POST', `${MR_API}/3/approve`)).toHaveLength(1);
    });
    expect(server.to('POST', `${MR_API}/3/approve`)[0]?.body).toEqual({ sha: C });
  });

  it('approve names the version on screen', async () => {
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}/3/versions`]: () => json(200, versions([version(1, { head_sha: B })])),
        [`GET ${MR_API}/3/status`]: () => json(200, at(B, [{ reason: 'unresolved_discussions', detail: 'count=1' }])),
        [`POST ${MR_API}/3/approve`]: () =>
          json(200, { iid: 3, outcome: 'approved', counted: true, uncounted_reason: null, approval: null }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    fireEvent.click(await screen.findByRole('button', { name: 'Approve' }));
    await waitFor(() => {
      expect(server.to('POST', `${MR_API}/3/approve`)[0]?.body).toEqual({ sha: B });
    });
  });

  it('the merge request names the head captured when the dialog opened', async () => {
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}/3/versions`]: () => json(200, versions([version(1, { head_sha: B })])),
        [`GET ${MR_API}/3/status`]: () => json(200, at(B)),
        'GET /api/v1/projects/7/merge-settings': () => json(200, mergeSettings()),
        [`POST ${MR_API}/3/merge`]: () => json(202, attempt('queued')),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    fireEvent.click(await screen.findByRole('button', { name: 'Merge…' }));
    const dialog = await screen.findByRole('dialog');
    await within(dialog).findAllByRole('radio');
    const merge = within(dialog).getByRole('button', { name: 'Merge' });
    fireEvent.click(merge);
    fireEvent.click(merge);
    await waitFor(() => {
      expect(server.to('POST', `${MR_API}/3/merge`).length).toBeGreaterThan(0);
    });
    // One request for two quick clicks, naming the head on screen.
    expect(server.to('POST', `${MR_API}/3/merge`)).toHaveLength(1);
    expect((server.to('POST', `${MR_API}/3/merge`)[0]?.body as { sha: string }).sha).toBe(B);
  });
});

describe('HIGH 2: approval rule names', () => {
  it('names still loading: no Save; once they arrive the form starts from them and keeps the approvers', async () => {
    let release: () => void = () => undefined;
    const late = new Promise<void>((r) => {
      release = r;
    });
    const server = mockServer(
      settingsRoutes({
        'GET /api/v1/projects/7/approval-rules': () => json(200, { rules: [RULE] }),
        'GET /api/v1/projects/7/members': async () => {
          await late;
          return json(200, {
            members: [{ user_id: 6, username: 'jiro', display_name: 'Jiro Suzuki', role: 'developer', active: true, created_at: '2026-10-02T00:00:00Z' }],
            next_after: null,
          });
        },
        'PUT /api/v1/projects/7/approval-rules/leads': () => json(200, RULE),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    const rules = await screen.findByRole('region', { name: 'Approval rules' });
    fireEvent.click(await within(rules).findByRole('button', { name: 'Change leads' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).queryByRole('button', { name: 'Save the rule' })).toBeNull();
    release();
    fireEvent.click(await within(dialog).findByRole('button', { name: 'Save the rule' }));
    await waitFor(() => {
      expect(server.to('PUT', '/api/v1/projects/7/approval-rules/leads')).toHaveLength(1);
    });
    expect(server.to('PUT', '/api/v1/projects/7/approval-rules/leads')[0]?.body).toEqual({
      required_approvals: 2,
      target_pattern: 'refs/heads/main',
      users: ['jiro'],
      groups: ['acme'],
    });
  });

  it('names that cannot be read: the reason and request id, and no Save', async () => {
    const server = mockServer(
      settingsRoutes({
        'GET /api/v1/projects/7/approval-rules': () => json(200, { rules: [RULE] }),
        'GET /api/v1/projects/7/members': () =>
          json(503, { error_code: 'FG-DB-0001', user_message: 'Members could not be read.', request_id: 'req-names-1' }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    const rules = await screen.findByRole('region', { name: 'Approval rules' });
    fireEvent.click(await within(rules).findByRole('button', { name: 'Change leads' }));
    const dialog = await screen.findByRole('dialog');
    const alert = await within(dialog).findByRole('alert');
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-names-1');
    expect(within(dialog).queryByRole('button', { name: 'Save the rule' })).toBeNull();
  });
});

describe('HIGH 3: Enter in the user search', () => {
  it('searches and does not submit the rule', async () => {
    const server = mockServer(
      settingsRoutes({
        'GET /api/v1/users': () => json(200, { items: [{ id: 6, username: 'jiro', display_name: 'Jiro Suzuki' }] }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    fireEvent.click(await screen.findByRole('button', { name: 'Add an approval rule' }));
    const dialog = await screen.findByRole('dialog');
    const field = await within(dialog).findByLabelText('Find a user');
    fireEvent.change(field, { target: { value: 'jir' } });
    // fireEvent returns false when the default (the form's implicit submit) was prevented.
    expect(fireEvent.keyDown(field, { key: 'Enter', code: 'Enter' })).toBe(false);
    expect(await within(dialog).findByRole('button', { name: 'Add Jiro Suzuki' })).toBeTruthy();
    expect(server.calls.filter((c) => c.method === 'PUT')).toHaveLength(0);
    // After adding, the search field has the focus again (the next person).
    fireEvent.click(within(dialog).getByRole('button', { name: 'Add Jiro Suzuki' }));
    await waitFor(() => {
      expect(document.activeElement).toBe(within(dialog).getByLabelText('Find a user'));
    });
  });
});

describe('focus (review MEDIUM)', () => {
  it('cancelling the rule dialog gives the focus back to the button that opened it', async () => {
    const server = mockServer(settingsRoutes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    const add = await screen.findByRole('button', { name: 'Add an approval rule' });
    add.focus();
    fireEvent.click(add);
    const dialog = await screen.findByRole('dialog');
    // The form (not the loading state) is there, and its first field has the focus.
    await waitFor(() => {
      expect(document.activeElement).toBe(within(dialog).getByLabelText('Name'));
    });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Add an approval rule' }));
    });
  });

  it('saving the merge settings keeps the focus on Save', async () => {
    const server = mockServer(
      settingsRoutes({
        'PUT /api/v1/projects/7/merge-settings': (c) =>
          json(200, mergeSettings({ ...(c.body as object), viewer: { can_manage: true, can_loosen: true, can_suspend_codeowners: true }, updated_at: '2026-11-03T09:00:00.000001Z' })),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    const merging = await screen.findByRole('region', { name: 'Merging' });
    fireEvent.click(await within(merging).findByLabelText('Delete the source branch after a merge'));
    const save = within(merging).getByRole('button', { name: 'Save merge settings' });
    save.focus();
    fireEvent.click(save);
    await within(merging).findByText('The merge settings were saved.');
    expect(document.activeElement).toBe(within(merging).getByRole('button', { name: 'Save merge settings' }));
    // A second save sends the stamp of the first one's answer.
    fireEvent.click(within(merging).getByRole('button', { name: 'Save merge settings' }));
    await waitFor(() => {
      expect(server.to('PUT', '/api/v1/projects/7/merge-settings')).toHaveLength(2);
    });
    expect(
      (server.to('PUT', '/api/v1/projects/7/merge-settings')[1]?.body as { expected_updated_at: string }).expected_updated_at,
    ).toBe('2026-11-03T09:00:00.000001Z');
  });
});
