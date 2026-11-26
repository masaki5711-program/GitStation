// Plan 19 §3.4 / L5-6: the deployment screens. The list (status in words,
// environment, commit, the empty state's next step); one deployment's
// approval screen: the facts always listed in full (also with no summary,
// and when the summary leaves them out), the AI summary apart from them and
// labelled as advice (model, prompt version, time; stale with its reason;
// less trustworthy; withheld; none), "unknown" not "none" when git could not
// read the range, and Approve offered only to someone who may approve —
// never depending on the summary — with a refusal's user_message and
// request_id. Synthetic data only.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { summaryPollInterval, SUMMARY_POLL_MS, type ApprovalSummary } from '../src/api/deployments';
import { approverStanding, listState } from '../src/features/deployApproval';
import { PROJECT, SESSION, apiError, mockServer, type Handler } from './mock';
import { repoRoutes } from './repo-fixtures';
import { json, renderApp } from './render';

const SHA = 'a'.repeat(40);
const BASE_SHA = 'b'.repeat(40);
const LIST = '/api/v1/projects/7/deployments';
const SUMMARY = `${LIST}/21/approval-summary`;
const APPROVE = `${LIST}/21/approve`;
const APPROVERS = '/api/v1/projects/7/deploy/approvers';
const PAGE = '/acme/forge-web/-/deployments';

const deployment = (over: Record<string, unknown> = {}) => ({
  id: 21,
  environment: 'prod',
  sha: SHA,
  status: 'pending_approval',
  approvals: 0,
  requested_by: 99,
  commit_subject: 'Add the press line timer',
  created_at: '2026-11-20T00:00:00Z',
  ...over,
});

const facts = (over: Record<string, unknown> = {}) => ({
  base_sha: BASE_SHA,
  range: 'read',
  commits: 3,
  commits_truncated: false,
  merge_requests: [{ iid: 4, title: 'Press line timer', risk: 'medium' }],
  migrations: ['0042_press_timer.sql'],
  protected_paths: [{ path: '.forge-ci.yml', rule: 'ci_config' }],
  ci_config: ['.forge-ci.yml'],
  permissions: [],
  failed_checks: [{ name: 'ci/lint', conclusion: 'failure' }],
  freeze_windows: [],
  frozen_now: false,
  lists_truncated: false,
  risk: 'medium',
  ...over,
});

const NONE = { state: 'none', label: 'AI の助言（提案）', note: 'x', stale: false };
const SHOWN = {
  state: 'shown',
  label: 'AI の助言（提案）',
  text: 'タイマーの変更です。<b>危険</b>は小さいです。',
  stale: false,
  degraded: false,
  input_class: 'internal',
  advice_id: 9,
  model: 'qwen-synthetic',
  prompt_version: 1,
  created_at: '2026-11-20T00:01:00Z',
};

const body = (over: { deployment?: object; facts?: object; summary?: object } = {}) => ({
  deployment: deployment(over.deployment as Record<string, unknown> | undefined),
  facts: facts(over.facts as Record<string, unknown> | undefined),
  summary: over.summary ?? NONE,
});

const unrestricted = (tier = 'prod') => ({
  tier,
  unrestricted: true,
  users: [],
  groups: [],
  selectors: [],
  approvals_required: 1,
  allow_delegation: false,
});

const routes = (role: string, over: Record<string, Handler> = {}) =>
  mockServer(
    repoRoutes({
      'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role }),
      [`GET ${APPROVERS}`]: () => json(200, { environments: [unrestricted()] }),
      ...over,
    }),
  );

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help}`);
}

describe('deployment list', () => {
  it('shows status in words, the environment, the commit and links to the approval screen', async () => {
    const server = routes('developer', {
      [`GET ${LIST}`]: () =>
        json(200, {
          as_of: '2026-11-20T00:02:00Z',
          deployments: [deployment(), deployment({ id: 20, environment: 'stg', status: 'succeeded' })],
        }),
    });
    await renderApp({ fetch: server.fetch, path: PAGE });
    const list = await screen.findByTestId('deployment-list');
    const items = within(list).getAllByRole('listitem');
    expect(items).toHaveLength(2);
    const first = items[0] as HTMLElement;
    expect(within(first).getByText('承認待ち')).toBeTruthy();
    expect(within(first).getByText('本番 (prod)')).toBeTruthy();
    expect(within(first).getByRole('link', { name: 'デプロイ #21' }).getAttribute('href')).toBe(`${PAGE}/21`);
    expect(within(items[1] as HTMLElement).getByText('成功')).toBeTruthy();
    expect(server.unmatched).toEqual([]);
  });

  it('says what to do when there is none', async () => {
    const server = routes('developer', {
      [`GET ${LIST}`]: () => json(200, { as_of: '2026-11-20T00:02:00Z', deployments: [] }),
    });
    await renderApp({ fetch: server.fetch, path: PAGE });
    const empty = await screen.findByTestId('deployments-empty');
    expect(within(empty).getByRole('link').getAttribute('href')).toBe('/acme/forge-web/-/pipelines');
  });

  it('shows a failure with its request_id', async () => {
    const server = routes('developer', {
      [`GET ${LIST}`]: () => apiError(503, '一時的に読めません。', 'req-dep-list'),
    });
    await renderApp({ fetch: server.fetch, path: PAGE });
    expect(await screen.findByText('req-dep-list')).toBeTruthy();
  });

  it('is an item of the project sidebar', async () => {
    const server = routes('developer', {
      [`GET ${LIST}`]: () => json(200, { as_of: '2026-11-20T00:02:00Z', deployments: [] }),
    });
    await renderApp({ fetch: server.fetch, path: PAGE });
    await screen.findByTestId('deployments-empty');
    const links = screen.getAllByRole('link', { name: /デプロイ/ });
    expect(links.some((a) => a.getAttribute('href') === PAGE)).toBe(true);
  });
});

describe('deployment approval screen', () => {
  it('approves without a summary: the facts are listed, the advice says none, the POST carries nothing of it', async () => {
    let approved = false;
    const server = routes('maintainer', {
      [`GET ${SUMMARY}`]: () =>
        json(200, approved ? body({ deployment: { status: 'queued', approvals: 1 } }) : body()),
      [`POST ${APPROVE}`]: () => {
        approved = true;
        return json(200, deployment({ status: 'queued', approvals: 1 }));
      },
    });
    await renderApp({ fetch: server.fetch, path: `${PAGE}/21` });
    const factsBox = await screen.findByTestId('deploy-facts');
    expect(within(factsBox).getByText('0042_press_timer.sql')).toBeTruthy();
    expect(within(factsBox).getByText('ci/lint')).toBeTruthy();
    expect(screen.getByTestId('advice-none').textContent).toContain('承認には要りません');
    expect(screen.getByTestId('deploy-status-line').textContent).toContain('1');

    fireEvent.click(screen.getByRole('button', { name: '承認する' }));
    const confirm = await screen.findByTestId('approve-confirm');
    expect(confirm.textContent).toContain('aaaaaaaa');
    expect(confirm.textContent).toContain('本番 (prod)');
    expect(server.to('POST', APPROVE)).toHaveLength(0);
    fireEvent.click(within(confirm).getByRole('button', { name: '承認する' }));
    await waitFor(() => {
      expect(server.to('POST', APPROVE)).toHaveLength(1);
    });
    const call = server.to('POST', APPROVE)[0];
    expect(call?.body ?? null).toBeNull();
    expect(call?.query.toString()).toBe('');
    expect(call?.headers.get('x-gitstation-csrf')).toBe(SESSION.csrf_token);
    await waitFor(() => {
      expect(screen.getByTestId('deploy-status-line').textContent).toContain('順番を待っています');
    });
    expect(server.unmatched).toEqual([]);
  });

  it('lists the facts even when the summary leaves them out, and keeps the advice apart and labelled', async () => {
    const server = routes('maintainer', {
      [`GET ${SUMMARY}`]: () => json(200, body({ summary: SHOWN })),
    });
    const { container } = await renderApp({ fetch: server.fetch, path: `${PAGE}/21` });
    const advice = await screen.findByTestId('deploy-advice');
    expect(within(advice).getByText('AI の助言（提案）')).toBeTruthy();
    expect(advice.textContent).toContain('qwen-synthetic');
    expect(advice.textContent).toContain('v1');
    // Plain text: the markup in the words is shown, never rendered.
    const text = screen.getByTestId('advice-text');
    expect(text.textContent).toContain('<b>危険</b>');
    expect(text.querySelector('b')).toBeNull();
    // The summary never names the migration; the facts still do.
    const factsBox = screen.getByTestId('deploy-facts');
    expect(within(factsBox).getByText('0042_press_timer.sql')).toBeTruthy();
    expect(advice.contains(factsBox)).toBe(false);
    // The advice has no controls at all.
    expect(within(advice).queryAllByRole('button')).toHaveLength(0);
    // The button is the same with a summary as without one.
    expect(screen.getByRole('button', { name: '承認する' })).toBeTruthy();
    expect(await violations(container)).toEqual([]);
  });

  it('marks a stale summary with its reason and a less trustworthy one, and still offers the approval', async () => {
    const server = routes('maintainer', {
      [`GET ${SUMMARY}`]: () =>
        json(200, body({ summary: { ...SHOWN, stale: true, stale_reason: 'newer_commit', degraded: true } })),
    });
    await renderApp({ fetch: server.fetch, path: `${PAGE}/21` });
    const stale = await screen.findByTestId('advice-stale');
    expect(stale.textContent).toContain('古い要約です');
    expect(stale.textContent).toContain('新しいコミット');
    expect(screen.getByTestId('advice-degraded')).toBeTruthy();
    expect(screen.getByRole('button', { name: '承認する' })).toBeTruthy();
  });

  it('says a withheld summary is not shown', async () => {
    const withheld = { ...SHOWN, state: 'withheld', text: null, input_class: 'confidential' };
    const server = routes('maintainer', { [`GET ${SUMMARY}`]: () => json(200, body({ summary: withheld })) });
    await renderApp({ fetch: server.fetch, path: `${PAGE}/21` });
    expect((await screen.findByTestId('advice-withheld')).textContent).toContain('機密');
    expect(screen.queryByTestId('advice-text')).toBeNull();
  });

  it('says "unknown", never "none", when git could not read the range', async () => {
    const server = routes('maintainer', {
      [`GET ${SUMMARY}`]: () =>
        json(
          200,
          body({
            facts: {
              range: 'unavailable',
              range_error_code: 'FG-GIT-0001',
              commits: 0,
              merge_requests: [],
              migrations: [],
              protected_paths: [],
              ci_config: [],
              failed_checks: [],
            },
          }),
        ),
    });
    await renderApp({ fetch: server.fetch, path: `${PAGE}/21` });
    expect((await screen.findByTestId('facts-range-unavailable')).textContent).toContain('FG-GIT-0001');
    for (const id of ['facts-mrs', 'facts-migrations', 'facts-protected', 'facts-ci', 'facts-permissions']) {
      expect(screen.getByTestId(id).getAttribute('data-state')).toBe('unknown');
      expect(screen.getByTestId(id).textContent).toBe('不明');
    }
    // The checks are of the commit itself: none is known.
    expect(screen.getByTestId('facts-checks').getAttribute('data-state')).toBe('none');
  });

  it('never offers the approval to its requester, and says why', async () => {
    const server = routes('maintainer', {
      [`GET ${SUMMARY}`]: () => json(200, body({ deployment: { requested_by: SESSION.user.id } })),
    });
    await renderApp({ fetch: server.fetch, path: `${PAGE}/21` });
    expect((await screen.findByTestId('deploy-status-line')).textContent).toContain('依頼した人は承認できません');
    expect(screen.queryByRole('button', { name: '承認する' })).toBeNull();
  });

  it('does not offer it to a Developer where any Maintainer approves', async () => {
    const server = routes('developer', { [`GET ${SUMMARY}`]: () => json(200, body()) });
    await renderApp({ fetch: server.fetch, path: `${PAGE}/21` });
    expect((await screen.findByTestId('deploy-status-line')).textContent).toContain('この環境の承認者');
    expect(screen.queryByRole('button', { name: '承認する' })).toBeNull();
    // The facts and the advice are there for every reader.
    expect(screen.getByTestId('deploy-facts')).toBeTruthy();
    expect(screen.getByTestId('deploy-advice')).toBeTruthy();
  });

  it('offers it to a named approver from Reporter', async () => {
    const named = { ...unrestricted(), unrestricted: false, users: [{ id: SESSION.user.id, username: 'taro', display_name: '山田 太郎' }] };
    const server = routes('reporter', {
      [`GET ${SUMMARY}`]: () => json(200, body()),
      [`GET ${APPROVERS}`]: () => json(200, { environments: [named] }),
    });
    await renderApp({ fetch: server.fetch, path: `${PAGE}/21` });
    expect(await screen.findByRole('button', { name: '承認する' })).toBeTruthy();
  });

  it('shows a refused approval with its user_message and request_id', async () => {
    const server = routes('maintainer', {
      [`GET ${SUMMARY}`]: () => json(200, body()),
      [`POST ${APPROVE}`]: () =>
        apiError(403, 'このコミットに関わった人は承認できません。', 'req-approve-403', 'FG-DEPLOY-0009'),
    });
    await renderApp({ fetch: server.fetch, path: `${PAGE}/21` });
    fireEvent.click(await screen.findByRole('button', { name: '承認する' }));
    fireEvent.click(within(await screen.findByTestId('approve-confirm')).getByRole('button', { name: '承認する' }));
    expect(await screen.findByText('req-approve-403')).toBeTruthy();
    expect(screen.getByText('このコミットに関わった人は承認できません。')).toBeTruthy();
  });

  it('shows no approval for a deployment that is not waiting', async () => {
    const server = routes('maintainer', {
      [`GET ${SUMMARY}`]: () =>
        json(200, body({ deployment: { status: 'failed', error_code: 'FG-DEPLOY-0004', failed_target: 'web-1' } })),
    });
    await renderApp({ fetch: server.fetch, path: `${PAGE}/21` });
    const line = await screen.findByTestId('deploy-status-line');
    expect(line.textContent).toContain('FG-DEPLOY-0004');
    expect(line.textContent).toContain('web-1');
    expect(screen.queryByTestId('deploy-approve')).toBeNull();
  });

  it('shows a failed read with its request_id and a way back', async () => {
    const server = routes('maintainer', {
      [`GET ${SUMMARY}`]: () => apiError(404, '見つかりません。', 'req-dep-404', 'FG-REPO-0005'),
    });
    await renderApp({ fetch: server.fetch, path: `${PAGE}/21` });
    expect(await screen.findByText('req-dep-404')).toBeTruthy();
    expect(screen.getByRole('link', { name: 'デプロイの一覧に戻る' })).toBeTruthy();
  });

  it('is not found for an address that is not a deployment', async () => {
    const server = routes('maintainer');
    await renderApp({ fetch: server.fetch, path: `${PAGE}/x1` });
    await waitFor(() => {
      expect(server.to('GET', SUMMARY)).toHaveLength(0);
    });
    expect(await screen.findByRole('heading', { level: 1 })).toBeTruthy();
  });

  it('reads in English', async () => {
    const server = routes('maintainer', { [`GET ${SUMMARY}`]: () => json(200, body()) });
    await renderApp({ fetch: server.fetch, path: `${PAGE}/21`, lang: 'en' });
    expect(await screen.findByRole('button', { name: 'Approve' })).toBeTruthy();
    expect(screen.getByTestId('advice-none').textContent).toContain('does not need one');
  });
});

describe('the approval decisions', () => {
  const d = deployment() as unknown as ApprovalSummary['deployment'];

  it('never reads the summary: the standing depends on the deployment, the caller and the approvers', () => {
    const env = unrestricted() as unknown as Parameters<typeof approverStanding>[3];
    expect(approverStanding(d, 5, 'maintainer', env)).toBe('approver');
    expect(approverStanding(d, 5, 'developer', env)).toBe('not_approver');
    expect(approverStanding({ ...d, requested_by: 5 }, 5, 'owner', env)).toBe('requester');
    expect(approverStanding({ ...d, status: 'queued' }, 5, 'owner', env)).toBe('not_waiting');
    expect(approverStanding(d, 5, 'maintainer', undefined)).toBe('approver');
    const groups = { ...unrestricted(), unrestricted: false, groups: [{ id: 1, path: 'ops', display_name: 'Ops' }] };
    expect(approverStanding(d, 5, 'maintainer', groups as unknown as typeof env)).toBe('maybe');
    expect(approverStanding(d, 5, 'reporter', groups as unknown as typeof env)).toBe('not_approver');
  });

  it('reads again only while it waits and no summary has come', () => {
    const b = (status: string, state: string) =>
      ({ deployment: { ...d, status }, facts: facts(), summary: { ...NONE, state } }) as unknown as ApprovalSummary;
    expect(summaryPollInterval(b('pending_approval', 'none'))).toBe(SUMMARY_POLL_MS);
    expect(summaryPollInterval(b('pending_approval', 'shown'))).toBe(false);
    expect(summaryPollInterval(b('queued', 'none'))).toBe(false);
    expect(summaryPollInterval(undefined)).toBe(false);
  });

  it('a list is unknown only when the range is unavailable', () => {
    const f = facts() as unknown as ApprovalSummary['facts'];
    expect(listState(f, [])).toBe('none');
    expect(listState({ ...f, range: 'unavailable' }, [])).toBe('unknown');
    expect(listState({ ...f, range: 'unavailable' }, ['x'])).toBe('listed');
  });
});
