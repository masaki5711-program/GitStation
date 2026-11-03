// axe (WCAG 2.2 AA tags) on the U2 screens and states, in every language:
// the conditions panel with its actions, the merge dialog, the merge progress
// and its end lines, a refused approval, and the merge sections of the
// project settings (editable, read-only, the rule and conditions dialogs).
// jsdom has no layout: contrast is tokens.test.ts' and the journey's job.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import type { Language } from '../src/i18n';
import { RULE, protectedRef, settingsRoutes } from './merge-settings-fixtures';
import { apiError, mockServer, type Handler } from './mock';
import { MR_API, MR_SCREENS, attempt, mergeSettings, mr, mrRoutes, status } from './mr-fixtures';
import { json, renderApp } from './render';

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map(
    (v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`,
  );
}

const settle = async () => {
  await screen.findAllByRole('heading', { level: 1 });
  await waitFor(() => {
    expect(screen.queryByText(/読み込んでいます|Loading|Memuat/)).toBeNull();
  });
};

const ALL = { can_approve: true, can_unapprove: true, can_merge: true, can_mark_ready: true, approved_by_me: false };

const panelStatus = {
  ...status([
    { reason: 'draft', detail: null },
    { reason: 'approvals_missing', detail: 'rule=@protected_branch required=1 counted=0' },
    { reason: 'required_check_missing', detail: 'name=ci/gates' },
  ]),
  approvals: {
    rules: [{ name: '@protected_branch', applies: true, required: 1, counted: 0 }],
    approvals: [{ approver_id: 9, username: 'saburo', counted: false, uncounted_reason: 'committer' }],
    codeowners: [],
  },
  checks: [{ name: 'ci/gates', status: null, conclusion: null }],
  codeowners: { enforced: false, suspended_until: '2026-11-03T05:00:00Z' },
  viewer: ALL,
};

const MR_SCREENS_U2: [string, Record<string, Handler>][] = [
  [
    'conditions panel with every action',
    {
      [`GET ${MR_API}/3`]: () => json(200, mr(3, { draft: true })),
      [`GET ${MR_API}/3/status`]: () => json(200, panelStatus),
    },
  ],
  [
    'mergeable with the merge button',
    {
      [`GET ${MR_API}/3/status`]: () => json(200, { ...status([]), viewer: ALL }),
      [`GET /api/v1/projects/7/merge-settings`]: () => json(200, mergeSettings()),
    },
  ],
  [
    'a merge that ended superseded',
    {
      [`GET ${MR_API}/3/status`]: () =>
        json(200, { ...status([{ reason: 'merge_in_progress', detail: null }]), viewer: ALL }),
      [`GET ${MR_API}/3/merge`]: () =>
        json(200, { iid: 3, merge: attempt('superseded', { reason: 'target_moved', error_code: 'FG-REVIEW-0003' }) }),
    },
  ],
];

const SETTINGS_SCREENS: [string, Record<string, Handler>][] = [
  [
    'settings: maintainer with rules and blocked conditions',
    {
      'GET /api/v1/projects/7/approval-rules': () => json(200, { rules: [RULE] }),
      'GET /api/v1/projects/7/protected-refs': () =>
        json(200, {
          rules: [
            protectedRef(1, 'refs/heads/main', {
              merge_rule: { required_approvals: 1, required_checks: ['ci/gates'], require_resolved_discussions: true, direct_push_forbidden: true },
              warnings: ['required_checks_unreportable'],
            }),
          ],
        }),
    },
  ],
  [
    'settings: read-only, CODEOWNERS suspended',
    {
      'GET /api/v1/projects/7/merge-settings': () =>
        json(
          200,
          mergeSettings({
            codeowners: { enforced: false, suspended_at: '2026-11-03T01:00:00Z', suspended_until: '2026-11-03T02:00:00Z' },
          }),
        ),
      'GET /api/v1/projects/7/approval-rules': () => json(200, { rules: [RULE] }),
    },
  ],
];

describe('axe: U2 screens', () => {
  for (const lang of ['ja', 'en', 'id'] as Language[]) {
    it.each(MR_SCREENS_U2)(`${lang}: %s`, async (_name, over) => {
      const server = mockServer(mrRoutes(over));
      const { container } = await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang });
      await settle();
      await screen.findByRole('region', { name: /マージの条件|Merge conditions|Syarat merge/ });
      expect(await violations(container)).toEqual([]);
    });

    it.each(SETTINGS_SCREENS)(`${lang}: %s`, async (_name, over) => {
      const server = mockServer(settingsRoutes(over));
      const { container } = await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang });
      await settle();
      expect(await violations(container)).toEqual([]);
    });
  }

  it('the merge dialog open', async () => {
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}/3/status`]: () => json(200, { ...status([]), viewer: ALL }),
        [`GET /api/v1/projects/7/merge-settings`]: () => json(200, mergeSettings({ allowed_methods: ['merge_commit', 'squash', 'fast_forward'] })),
      }),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    await settle();
    fireEvent.click(await screen.findByRole('button', { name: 'Merge…' }));
    await within(await screen.findByRole('dialog')).findAllByRole('radio');
    expect(await violations(container)).toEqual([]);
  });

  it('a refused approval with the reload button', async () => {
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}/3/status`]: () => json(200, { ...status([]), viewer: ALL }),
        [`GET /api/v1/projects/7/merge-settings`]: () => json(200, mergeSettings()),
        [`POST ${MR_API}/3/approve`]: () => apiError(409, 'A new version was pushed.', 'req-a11y-409'),
      }),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    await settle();
    fireEvent.click(await screen.findByRole('button', { name: 'Approve' }));
    await screen.findByRole('button', { name: 'Reload the merge request' });
    expect(await violations(container)).toEqual([]);
  });

  it('the approval rule dialog with a search result, and the conditions dialog with its warnings', async () => {
    const server = mockServer(
      settingsRoutes({
        'GET /api/v1/users': () => json(200, { items: [{ id: 6, username: 'jiro', display_name: 'Jiro Suzuki' }] }),
        'GET /api/v1/projects/7/protected-refs': () => json(200, { rules: [protectedRef(1, 'refs/heads/main')] }),
      }),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    await settle();
    fireEvent.click(await screen.findByRole('button', { name: 'Add an approval rule' }));
    let dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByLabelText('Find a user'), { target: { value: 'jir' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Search' }));
    await within(dialog).findByRole('button', { name: 'Add Jiro Suzuki' });
    expect(await violations(container)).toEqual([]);
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).toBeNull();
    });
    fireEvent.click(screen.getByRole('button', { name: 'Change the conditions of refs/heads/main' }));
    dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByLabelText('Required checks (one per line)'), { target: { value: 'ci/gates' } });
    fireEvent.click(within(dialog).getByLabelText('Forbid direct pushes (merges only)'));
    await within(dialog).findByText(/cannot change at all/);
    expect(await violations(container)).toEqual([]);
  });
});
