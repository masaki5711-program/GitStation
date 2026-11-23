// ADR-0038 §3 (step C): a selector in plain words, the MR panel's "who can
// approve" (names with their department, and the reason and next step when
// no one can), the personal delegation form (at most 30 days, checked before
// sending), and the deploy approvers of the project settings.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { localToRfc3339, periodProblem, type ApproverSelector } from '../src/api/approvers';
import { selectorText, unresolvedText } from '../src/features/approvers';
import { createI18n } from '../src/i18n';
import { mockServer, signedIn } from './mock';
import { settingsRoutes } from './merge-settings-fixtures';
import { MR_API, MR_SCREENS, mrRoutes, status } from './mr-fixtures';
import { json, renderApp } from './render';

describe('selector words', () => {
  it('describes each relation, qualifier and the escalation', async () => {
    const i18n = await createI18n('ja');
    const t = i18n.t.bind(i18n);
    const s = (x: ApproverSelector) => selectorText(t, x);
    expect(s({ relation: 'requester_section', min_rank: 55 })).toBe('申請者の課の 職位ランク 55 以上');
    expect(s({ relation: 'requester_department', positions: ['general_manager'], escalate: 'parent' })).toBe(
      '申請者の部（部に直接いる人）の 職位 general_manager（いなければ 1 つ上の組織）',
    );
    expect(s({ relation: 'unit', unit: 'QA-01', include_descendants: true })).toBe(
      '組織 QA-01（配下を含む）の 全員',
    );
    expect(unresolvedText(t, 'requester_org_unknown', 'stale_snapshot')).toContain('ログインし直して');
    expect(unresolvedText(t, 'unit_not_in_master', null)).toContain('Maintainer が規則を直して');
  });
});

describe('delegation period', () => {
  it('converts the local time with its offset and refuses more than 30 days', () => {
    expect(localToRfc3339('2026-11-21T09:30')).toMatch(/^2026-11-21T09:30:00[+-]\d{2}:\d{2}$/);
    expect(localToRfc3339('tomorrow')).toBeNull();
    const now = new Date(2026, 10, 21, 0, 0);
    expect(periodProblem('2026-11-21T09:00', '2026-12-01T09:00', now)).toBeNull();
    expect(periodProblem('2026-11-21T09:00', '2026-12-31T09:00', now)).toBe('tooLong');
    expect(periodProblem('2026-11-21T09:00', '2026-11-20T09:00', now)).toBe('order');
    expect(periodProblem('2026-11-01T09:00', '2026-11-10T09:00', now)).toBe('ended');
  });

  it('the form refuses a period longer than 30 days without sending it', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': () =>
        json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
      'GET /api/v1/user/ssh-keys': () => json(200, { items: [], ssh: null }),
      'GET /api/v1/users': () =>
        json(200, {
          items: [{ id: 9, username: 'jiro', display_name: 'Jiro', primary_org_code: null, primary_org: null }],
        }),
    });
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings?tab=ssh-keys', lang: 'en' });
    const section = await screen.findByRole('region', { name: 'Delegated approval' });
    expect(within(section).getByText('You have no delegate.')).toBeTruthy();
    fireEvent.change(within(section).getByLabelText('Find a user'), { target: { value: 'jiro' } });
    fireEvent.click(within(section).getByRole('button', { name: 'Search' }));
    fireEvent.click(await within(section).findByRole('button', { name: 'Add Jiro' }));
    fireEvent.change(within(section).getByLabelText('Start'), { target: { value: '2027-01-01T09:00' } });
    fireEvent.change(within(section).getByLabelText('End'), { target: { value: '2027-03-01T09:00' } });
    fireEvent.click(within(section).getByRole('button', { name: 'Save the delegate' }));
    expect(await within(section).findByText('The period is at most 30 days.')).toBeTruthy();
    expect(server.to('PUT', '/api/v1/user/approval-delegation')).toHaveLength(0);
  });
});

describe('who can approve (the MR panel)', () => {
  const ruleStatus = {
    ...status([{ reason: 'approvals_missing', detail: 'rule=qa required=1 counted=0' }]),
    approvals: {
      rules: [
        { name: 'qa', applies: true, required: 1, counted: 0 },
        { name: 'managers', applies: true, required: 1, counted: 0 },
      ],
      approvals: [],
      codeowners: [],
    },
  };
  const state = {
    iid: 3,
    head_sha: 'b'.repeat(40),
    target_branch: 'main',
    approvals: [],
    rules: [],
    codeowners: [],
    config_change_approved: null,
    blocked_by: [],
    conflict: false,
    eligible: [
      {
        rule: 'qa',
        people: [
          {
            id: 9,
            username: 'jiro',
            display_name: 'Jiro',
            via: 'user',
            primary_org: {
              unit: { code: 'S1', level: 'section', name_ja: '品質保証課', name_en: 'Quality Assurance', name_id: null },
              section: { code: 'S1', level: 'section', name_ja: '品質保証課', name_en: 'Quality Assurance', name_id: null },
              department: null,
              stale: false,
            },
          },
        ],
        more: false,
        unresolved: [],
        incomplete: false,
      },
      {
        rule: 'managers',
        people: [],
        more: false,
        unresolved: [{ reason: 'requester_org_unknown', detail: 'no_snapshot' }],
        incomplete: false,
      },
    ],
  };

  it('lists the people with their section and says why no one can approve', async () => {
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}/3/status`]: () => json(200, ruleStatus),
        [`GET ${MR_API}/3/approvals`]: () => json(200, state),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/3`, lang: 'en' });
    const list = await screen.findByTestId('mr-eligible');
    await waitFor(() => {
      expect(within(list).getByText('qa: Jiro (Quality Assurance)')).toBeTruthy();
    });
    expect(within(list).getByText('managers: no one can approve')).toBeTruthy();
    expect(within(list).getByText(/Ask the author to sign in to the web once more/)).toBeTruthy();
  });
});

describe('deploy approvers (project settings)', () => {
  it('lists each environment and who may approve it', async () => {
    const server = mockServer(
      settingsRoutes({
        'GET /api/v1/projects/7/deploy/approvers': () =>
          json(200, {
            environments: [
              {
                tier: 'prod',
                unrestricted: false,
                users: [{ id: 9, username: 'jiro', display_name: 'Jiro' }],
                groups: [],
                selectors: [{ relation: 'requester_department', min_rank: 55, escalate: 'none' }],
                approvals_required: 2,
                allow_delegation: false,
              },
              {
                tier: 'stg',
                unrestricted: true,
                users: [],
                groups: [],
                selectors: [],
                approvals_required: 1,
                allow_delegation: false,
              },
            ],
          }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en' });
    const section = await screen.findByRole('region', { name: 'Deployment approvers' });
    const table = await within(section).findByRole('table');
    expect(within(table).getByText('Production')).toBeTruthy();
    expect(
      within(table).getByText("Jiro, position rank 55 or higher in the requester's department (its own people)"),
    ).toBeTruthy();
    expect(within(table).getByText('Any Maintainer')).toBeTruthy();
    // A Maintainer (not the Owner) sees no change button.
    expect(within(section).queryByRole('button', { name: /Change the approvers/ })).toBeNull();
  });
});
