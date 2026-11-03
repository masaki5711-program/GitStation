// U2 phase 3: the project's merge settings. Methods (the default within the
// allowed ones), approvals on push, committers' approvals, CODEOWNERS
// suspension (1-1440 minutes, shows until) and enforce-now, approval rules
// with a user / group picker, and the merge conditions of protected branches
// (the required-checks warning stays on screen, with the "branch stops
// moving" one when direct pushes are banned too). `expected_updated_at` is the
// read string sent back verbatim; a 409 offers a reload. Edit controls come
// from the viewer flags only; without them the settings are read-only.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer, SESSION, type Handler } from './mock';
import { RULE, protectedRef, settingsRoutes } from './merge-settings-fixtures';
import { mergeSettings } from './mr-fixtures';
import { json, renderApp } from './render';

const CSRF = SESSION.csrf_token;
const API = '/api/v1/projects/7';
const PATH = '/acme/forge-web/-/settings';
const STAMP = '2026-11-03T01:02:03.123456Z';

const MANAGER = { can_manage: true, can_loosen: true, can_suspend_codeowners: true };
const DEVELOPER = { can_manage: false, can_loosen: false, can_suspend_codeowners: false };

async function open(over: Record<string, Handler> = {}) {
  const server = mockServer(settingsRoutes(over));
  const view = await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
  await screen.findByRole('heading', { level: 1, name: 'Settings' });
  return { server, ...view };
}

const section = (name: string) => screen.findByRole('region', { name });

describe('merge settings', () => {
  it('saves methods and options with expected_updated_at verbatim and the CSRF header', async () => {
    const { server } = await open({
      [`PUT ${API}/merge-settings`]: (c) =>
        json(200, mergeSettings({ ...(c.body as object), viewer: MANAGER, updated_at: '2026-11-03T02:00:00.000001Z' })),
    });
    const merging = await section('Merging');
    const methods = await within(merging).findByRole('group', { name: 'Allowed merge methods' });
    fireEvent.click(within(methods).getByLabelText('Merge commit'));
    fireEvent.click(within(methods).getByLabelText('Fast-forward'));
    const select = within(merging).getByLabelText<HTMLSelectElement>('Default method');
    // Only allowed methods can be the default; the old default left with merge commit.
    expect([...select.options].map((o) => o.textContent)).toEqual(['Squash', 'Fast-forward']);
    fireEvent.change(select, { target: { value: 'fast_forward' } });
    fireEvent.click(within(merging).getByLabelText('Delete the source branch after a merge'));
    fireEvent.click(within(merging).getByRole('button', { name: 'Save merge settings' }));
    await waitFor(() => {
      expect(server.to('PUT', `${API}/merge-settings`)).toHaveLength(1);
    });
    const call = server.to('PUT', `${API}/merge-settings`)[0];
    expect(call?.body).toEqual({
      allowed_methods: ['squash', 'fast_forward'],
      default_method: 'fast_forward',
      delete_source_default: true,
      reset_approvals_on_push: true,
      committer_approvals_excluded: true,
      expected_updated_at: STAMP,
    });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await within(merging).findByText('The merge settings were saved.')).toBeTruthy();
  });

  it('the defaults (never stored) send expected_updated_at null', async () => {
    const { server } = await open({
      [`GET ${API}/merge-settings`]: () =>
        json(200, mergeSettings({ viewer: MANAGER, stored: false, updated_at: null })),
      [`PUT ${API}/merge-settings`]: () => json(200, mergeSettings({ viewer: MANAGER })),
    });
    const merging = await section('Merging');
    fireEvent.click(await within(merging).findByLabelText('Delete the source branch after a merge'));
    fireEvent.click(within(merging).getByRole('button', { name: 'Save merge settings' }));
    await waitFor(() => {
      expect(server.to('PUT', `${API}/merge-settings`)).toHaveLength(1);
    });
    expect((server.to('PUT', `${API}/merge-settings`)[0]?.body as { expected_updated_at: unknown }).expected_updated_at).toBeNull();
  });

  it('no method chosen is refused before sending', async () => {
    const { server } = await open();
    const merging = await section('Merging');
    const methods = await within(merging).findByRole('group', { name: 'Allowed merge methods' });
    fireEvent.click(within(methods).getByLabelText('Merge commit'));
    fireEvent.click(within(methods).getByLabelText('Squash'));
    fireEvent.click(within(merging).getByRole('button', { name: 'Save merge settings' }));
    expect(await within(merging).findByText('Choose at least one merge method.')).toBeTruthy();
    expect(server.to('PUT', `${API}/merge-settings`)).toHaveLength(0);
  });

  it('a 409 (changed meanwhile) shows the reason, the request id and a reload that reads them again', async () => {
    let stamp = STAMP;
    const { server } = await open({
      [`GET ${API}/merge-settings`]: () => json(200, mergeSettings({ viewer: MANAGER, updated_at: stamp })),
      [`PUT ${API}/merge-settings`]: () => {
        stamp = '2026-11-03T03:00:00.000002Z';
        return apiError(409, 'The merge settings were changed by someone else. Read them again.', 'req-ms-409', 'FG-REVIEW-0017');
      },
    });
    const merging = await section('Merging');
    fireEvent.click(await within(merging).findByLabelText('Delete the source branch after a merge'));
    fireEvent.click(within(merging).getByRole('button', { name: 'Save merge settings' }));
    const alert = await within(merging).findByRole('alert');
    expect(within(alert).getByText('The merge settings were changed by someone else. Read them again.')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-ms-409');
    expect(alert.textContent).not.toContain('FG-');
    const reads = server.to('GET', `${API}/merge-settings`).length;
    fireEvent.click(within(merging).getByRole('button', { name: 'Reload the settings' }));
    await waitFor(() => {
      expect(server.to('GET', `${API}/merge-settings`).length).toBeGreaterThan(reads);
    });
    // The form starts again from what was read (the change is not kept over theirs).
    await waitFor(() => {
      expect(within(merging).getByLabelText<HTMLInputElement>('Delete the source branch after a merge').checked).toBe(false);
    });
    expect(within(merging).queryByRole('alert')).toBeNull();
  });

  it('loosening needs can_loosen: the switches that are on cannot be turned off without it', async () => {
    await open({
      [`GET ${API}/merge-settings`]: () =>
        json(200, mergeSettings({ viewer: { can_manage: true, can_loosen: false, can_suspend_codeowners: false } })),
    });
    const merging = await section('Merging');
    const reset = await within(merging).findByLabelText<HTMLInputElement>('New commits end the approvals');
    const committers = within(merging).getByLabelText<HTMLInputElement>("Committers' approvals do not count");
    expect(reset.disabled).toBe(true);
    expect(committers.disabled).toBe(true);
    expect(within(merging).getAllByText(/needs a role at or above every protected branch/).length).toBeGreaterThan(0);
  });

  it('a Developer sees the settings read-only, with no edit control in any merge section', async () => {
    await open({
      [`GET ${API}/merge-settings`]: () => json(200, mergeSettings({ viewer: DEVELOPER })),
      [`GET ${API}/approval-rules`]: () => json(200, { rules: [RULE] }),
      [`GET ${API}/protected-refs`]: () =>
        json(200, {
          rules: [
            protectedRef(1, 'refs/heads/main', {
              merge_rule: { required_approvals: 1, required_checks: [], require_resolved_discussions: true, direct_push_forbidden: false },
            }),
          ],
        }),
    });
    const merging = await section('Merging');
    expect(await within(merging).findByText('Merge commit, Squash')).toBeTruthy();
    expect(within(merging).queryByRole('checkbox')).toBeNull();
    expect(within(merging).queryByRole('button')).toBeNull();
    expect(within(merging).getByText('CODEOWNERS approval is enforced.')).toBeTruthy();
    const rules = await section('Approval rules');
    await within(rules).findByText('leads');
    expect(within(rules).queryByRole('button')).toBeNull();
    const conditions = await section('Merge conditions of protected branches');
    await within(conditions).findByText('refs/heads/main');
    expect(within(conditions).queryByRole('button')).toBeNull();
    expect(within(conditions).getByText('Threads must be resolved')).toBeTruthy();
  });
});

describe('CODEOWNERS enforcement', () => {
  it('suspends for 1 to 1440 minutes and then shows until when', async () => {
    const { server } = await open({
      [`PUT ${API}/merge-settings/codeowners-enforcement`]: () =>
        json(
          200,
          mergeSettings({
            viewer: MANAGER,
            codeowners: { enforced: false, suspended_at: '2026-11-03T01:00:00Z', suspended_until: '2026-11-03T01:30:00Z' },
          }),
        ),
    });
    const merging = await section('Merging');
    const minutes = await within(merging).findByLabelText('Suspend for (minutes)');
    for (const bad of ['0', '1441', '2.5', '']) {
      fireEvent.change(minutes, { target: { value: bad } });
      fireEvent.click(within(merging).getByRole('button', { name: 'Suspend CODEOWNERS' }));
      expect(await within(merging).findByText('Enter a whole number of minutes from 1 to 1440.')).toBeTruthy();
    }
    expect(server.to('PUT', `${API}/merge-settings/codeowners-enforcement`)).toHaveLength(0);
    fireEvent.change(minutes, { target: { value: '30' } });
    fireEvent.click(within(merging).getByRole('button', { name: 'Suspend CODEOWNERS' }));
    await waitFor(() => {
      expect(server.to('PUT', `${API}/merge-settings/codeowners-enforcement`)).toHaveLength(1);
    });
    const call = server.to('PUT', `${API}/merge-settings/codeowners-enforcement`)[0];
    expect(call?.body).toEqual({ enforced: false, for_minutes: 30 });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    expect(await within(merging).findByText(/CODEOWNERS approval is suspended until/)).toBeTruthy();
    expect(within(merging).getByRole('button', { name: 'Enforce CODEOWNERS now' })).toBeTruthy();
  });

  it('enforce now on a suspension', async () => {
    const { server } = await open({
      [`GET ${API}/merge-settings`]: () =>
        json(
          200,
          mergeSettings({
            viewer: { can_manage: true, can_loosen: false, can_suspend_codeowners: false },
            codeowners: { enforced: false, suspended_at: '2026-11-03T01:00:00Z', suspended_until: '2026-11-03T01:30:00Z' },
          }),
        ),
      [`PUT ${API}/merge-settings/codeowners-enforcement`]: () => json(200, mergeSettings({ viewer: MANAGER })),
    });
    const merging = await section('Merging');
    fireEvent.click(await within(merging).findByRole('button', { name: 'Enforce CODEOWNERS now' }));
    await waitFor(() => {
      expect(server.to('PUT', `${API}/merge-settings/codeowners-enforcement`)).toHaveLength(1);
    });
    expect(server.to('PUT', `${API}/merge-settings/codeowners-enforcement`)[0]?.body).toEqual({ enforced: true });
    expect(await within(merging).findByText('CODEOWNERS approval is enforced.')).toBeTruthy();
  });

  it('without can_suspend_codeowners there is no suspend control', async () => {
    await open({
      [`GET ${API}/merge-settings`]: () =>
        json(200, mergeSettings({ viewer: { can_manage: true, can_loosen: true, can_suspend_codeowners: false } })),
    });
    const merging = await section('Merging');
    await within(merging).findByText('CODEOWNERS approval is enforced.');
    expect(within(merging).queryByLabelText('Suspend for (minutes)')).toBeNull();
  });
});

describe('approval rules', () => {
  it('lists rules with approvers by name; an id that cannot be named is shown as a number', async () => {
    await open({
      [`GET ${API}/approval-rules`]: () =>
        json(200, { rules: [RULE, { ...RULE, approval_rule_id: 2, name: 'any', target_pattern: null, user_ids: [99], group_ids: [] }] }),
    });
    const rules = await section('Approval rules');
    const row = (await within(rules).findByText('leads')).closest('tr') as HTMLElement;
    expect(within(row).getByText('refs/heads/main')).toBeTruthy();
    expect(within(row).getByText(/jiro/)).toBeTruthy();
    expect(within(row).getByText(/acme/)).toBeTruthy();
    const other = within(rules).getByText('any').closest('tr') as HTMLElement;
    expect(within(other).getByText('Every target branch')).toBeTruthy();
    expect(within(other).getByText(/user #99/)).toBeTruthy();
  });

  it('adds a rule with a user and a group from the pickers', async () => {
    const { server } = await open({
      'GET /api/v1/users': () => json(200, { items: [{ id: 6, username: 'jiro', display_name: 'Jiro Suzuki' }] }),
      [`PUT ${API}/approval-rules/release-leads`]: () => json(200, { ...RULE, name: 'release-leads' }),
    });
    const rules = await section('Approval rules');
    fireEvent.click(await within(rules).findByRole('button', { name: 'Add an approval rule' }));
    const dialog = await screen.findByRole('dialog');
    // The form appears once the approver names were read.
    fireEvent.change(await within(dialog).findByLabelText('Name'), { target: { value: 'release-leads' } });
    fireEvent.change(within(dialog).getByLabelText('Target branches (optional)'), { target: { value: 'refs/heads/release/*' } });
    fireEvent.change(within(dialog).getByLabelText('Approvals needed'), { target: { value: '2' } });
    fireEvent.change(within(dialog).getByLabelText('Find a user'), { target: { value: 'jir' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Search' }));
    fireEvent.click(await within(dialog).findByRole('button', { name: 'Add Jiro Suzuki' }));
    expect(within(dialog).getByRole('button', { name: 'Remove jiro' })).toBeTruthy();
    fireEvent.change(await within(dialog).findByLabelText('Group'), { target: { value: 'acme' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Add the group' }));
    expect(within(dialog).getByRole('button', { name: 'Remove acme' })).toBeTruthy();
    fireEvent.click(within(dialog).getByRole('button', { name: 'Save the rule' }));
    await waitFor(() => {
      expect(server.to('PUT', `${API}/approval-rules/release-leads`)).toHaveLength(1);
    });
    const call = server.to('PUT', `${API}/approval-rules/release-leads`)[0];
    expect(call?.body).toEqual({
      required_approvals: 2,
      target_pattern: 'refs/heads/release/*',
      users: ['jiro'],
      groups: ['acme'],
    });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).toBeNull();
    });
  });

  it('a bad name or count is refused before sending', async () => {
    const { server } = await open();
    const rules = await section('Approval rules');
    fireEvent.click(await within(rules).findByRole('button', { name: 'Add an approval rule' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(await within(dialog).findByLabelText('Name'), { target: { value: 'Leads Team' } });
    fireEvent.change(within(dialog).getByLabelText('Approvals needed'), { target: { value: '21' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Save the rule' }));
    expect(
      await within(dialog).findByText('Use lower-case letters, digits, "_", "." and "-" (64 at most), starting with a letter or digit.'),
    ).toBeTruthy();
    expect(within(dialog).getByText('Enter a number from 1 to 20.')).toBeTruthy();
    expect(server.calls.filter((c) => c.method === 'PUT')).toHaveLength(0);
  });

  it('a rule whose approvers cannot all be named cannot be changed here (it would drop them)', async () => {
    await open({
      [`GET ${API}/approval-rules`]: () => json(200, { rules: [{ ...RULE, user_ids: [6, 99] }] }),
    });
    const rules = await section('Approval rules');
    fireEvent.click(await within(rules).findByRole('button', { name: 'Change leads' }));
    const dialog = await screen.findByRole('dialog');
    expect(await within(dialog).findByText(/Some approvers of this rule cannot be shown by name/)).toBeTruthy();
    expect(within(dialog).queryByRole('button', { name: 'Save the rule' })).toBeNull();
  });

  it('deletes a rule after asking', async () => {
    const { server } = await open({
      [`GET ${API}/approval-rules`]: () => json(200, { rules: [RULE] }),
      [`DELETE ${API}/approval-rules/leads`]: () => new Response(null, { status: 204 }),
    });
    const rules = await section('Approval rules');
    fireEvent.click(await within(rules).findByRole('button', { name: 'Delete leads' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Delete the rule' }));
    await waitFor(() => {
      expect(server.to('DELETE', `${API}/approval-rules/leads`)).toHaveLength(1);
    });
    expect(server.to('DELETE', `${API}/approval-rules/leads`)[0]?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
  });
});

describe('merge conditions of protected branches', () => {
  const BLOCKED = protectedRef(1, 'refs/heads/main', {
    merge_rule: { required_approvals: 1, required_checks: ['ci/gates'], require_resolved_discussions: false, direct_push_forbidden: true },
    warnings: ['required_checks_unreportable'],
  });

  it('keeps the required-checks warning on screen, and the "branch stops moving" one with the push ban', async () => {
    await open({
      [`GET ${API}/protected-refs`]: () => json(200, { rules: [BLOCKED, protectedRef(2, 'refs/tags/v*')] }),
    });
    const conditions = await section('Merge conditions of protected branches');
    const notes = await within(conditions).findAllByRole('note');
    const text = notes.map((n) => n.textContent).join(' ');
    expect(text).toContain('No check can report a result yet, so every merge into this branch is blocked.');
    expect(text).toContain('Direct pushes are forbidden too, so this branch cannot change at all.');
    // Tags have no merge conditions.
    expect(within(conditions).queryByText('refs/tags/v*')).toBeNull();
    // Work items (I1) are not offered yet.
    expect(document.body.textContent).not.toMatch(/work item/i);
  });

  it('changes the conditions: PUT with exactly the four fields, CSRF, and warns while typing checks', async () => {
    const { server } = await open({
      [`GET ${API}/protected-refs`]: () => json(200, { rules: [protectedRef(1, 'refs/heads/main')] }),
      [`PUT ${API}/protected-refs/1/merge-rule`]: () => json(200, BLOCKED),
    });
    const conditions = await section('Merge conditions of protected branches');
    expect(await within(conditions).findByText('No merge conditions.')).toBeTruthy();
    fireEvent.click(within(conditions).getByRole('button', { name: 'Change the conditions of refs/heads/main' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByLabelText('Approvals needed'), { target: { value: '2' } });
    fireEvent.change(within(dialog).getByLabelText('Required checks (one per line)'), { target: { value: 'ci/gates\n' } });
    expect(within(dialog).getByText(/No check can report a result yet/)).toBeTruthy();
    fireEvent.click(within(dialog).getByLabelText('Forbid direct pushes (merges only)'));
    expect(within(dialog).getByText(/this branch cannot change at all/)).toBeTruthy();
    fireEvent.click(within(dialog).getByLabelText('Threads must be resolved'));
    fireEvent.click(within(dialog).getByRole('button', { name: 'Save the conditions' }));
    await waitFor(() => {
      expect(server.to('PUT', `${API}/protected-refs/1/merge-rule`)).toHaveLength(1);
    });
    const call = server.to('PUT', `${API}/protected-refs/1/merge-rule`)[0];
    expect(call?.body).toEqual({
      required_approvals: 2,
      required_checks: ['ci/gates'],
      require_resolved_discussions: true,
      direct_push_forbidden: true,
    });
    expect(call?.headers.get('X-GitStation-CSRF')).toBe(CSRF);
  });

  it('a check outside ci/, kanflow/ and forge/ is refused before sending', async () => {
    const { server } = await open({
      [`GET ${API}/protected-refs`]: () => json(200, { rules: [protectedRef(1, 'refs/heads/main')] }),
    });
    const conditions = await section('Merge conditions of protected branches');
    fireEvent.click(await within(conditions).findByRole('button', { name: 'Change the conditions of refs/heads/main' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByLabelText('Required checks (one per line)'), { target: { value: 'ext/lint' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Save the conditions' }));
    expect(await within(dialog).findByText(/Each check name starts with ci\/, kanflow\/ or forge\//)).toBeTruthy();
    expect(server.to('PUT', `${API}/protected-refs/1/merge-rule`)).toHaveLength(0);
  });

  it('removes the conditions after asking', async () => {
    const { server } = await open({
      [`GET ${API}/protected-refs`]: () => json(200, { rules: [BLOCKED] }),
      [`DELETE ${API}/protected-refs/1/merge-rule`]: () => new Response(null, { status: 204 }),
    });
    const conditions = await section('Merge conditions of protected branches');
    fireEvent.click(await within(conditions).findByRole('button', { name: 'Remove the conditions of refs/heads/main' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Remove the conditions' }));
    await waitFor(() => {
      expect(server.to('DELETE', `${API}/protected-refs/1/merge-rule`)).toHaveLength(1);
    });
  });
});
