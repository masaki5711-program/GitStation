// Plan 17 P17-7b: the AI asset releases screen. Each release with its
// version, tag, commit, bench and signing key; per stage what runs and the
// rollback it would make; a withdrawn release marked in words with its
// reason; the buttons (next stage, rollback, withdraw) only where the API
// says the caller may press them; a request links to the deployment screen
// where it is approved; loading, empty and error states with user_message
// and request_id. Synthetic data only.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { releaseLine } from '../src/features/aiReleases';
import { withdrawReasonValid, type Release } from '../src/api/aiReleases';
import { createI18n } from '../src/i18n';
import { PROJECT, SESSION, apiError, mockServer, type Handler } from './mock';
import { repoRoutes } from './repo-fixtures';
import { json, renderApp } from './render';

const LIST = '/api/v1/projects/7/ai-asset-releases';
const ROLLBACKS = '/api/v1/projects/7/ai-asset-rollbacks';
const PAGE = '/acme/forge-web/-/deployments/releases';

type Over = Record<string, unknown>;

const stages = (over: Partial<Record<'dev' | 'stg' | 'prod', Over>> = {}) =>
  (
    [
      ['pfs-ai-shadow', 'dev'],
      ['pfs-ai-canary', 'stg'],
      ['pfs-ai-prod', 'prod'],
    ] as const
  ).map(([environment, tier]) => ({
    environment,
    tier,
    deployment_id: null,
    status: null,
    approved_success: false,
    ...over[tier],
  }));

const release = (over: Over = {}) => ({
  id: 2,
  kind: 'pfs-ai-bundle',
  version: 'v2026.48.2',
  tag: 'assets/v2026.48.2',
  commit_sha: 'c'.repeat(40),
  tag_object: 'c'.repeat(40),
  digest: `sha256:${'0'.repeat(64)}`,
  key_id: '3',
  signature: `${'A'.repeat(86)}==`,
  files: 12,
  bench_check_run_id: 41,
  created_by: 5,
  created_at: '2026-11-27T00:00:00Z',
  manifest: null,
  withdrawal: null,
  key_revoked: false,
  stages: stages({ dev: { deployment_id: 30, status: 'succeeded', approved_success: true } }),
  next_stage: 'pfs-ai-canary',
  ...over,
});

const withdrawnOld = () =>
  release({
    id: 1,
    version: 'v2026.47.1',
    tag: 'assets/v2026.47.1',
    commit_sha: 'b'.repeat(40),
    withdrawal: {
      version: 'v2026.47.1',
      reason: 'Wrong threshold in playbook P-3',
      withdrawn_by: 5,
      withdrawn_at: '2026-11-27T01:00:00Z',
    },
    stages: stages({ dev: { deployment_id: 20, status: 'succeeded', approved_success: true } }),
    next_stage: null,
  });

const stageRows = (over: Partial<Record<'dev' | 'stg' | 'prod', Over>> = {}) =>
  (
    [
      ['pfs-ai-shadow', 'dev'],
      ['pfs-ai-canary', 'stg'],
      ['pfs-ai-prod', 'prod'],
    ] as const
  ).map(([environment, tier]) => ({
    environment,
    tier,
    running_version: null,
    rollback_version: null,
    ...over[tier],
  }));

const viewer = (may: boolean) => ({ can_release: may, can_request: may, can_withdraw: may });

const list = (over: Over = {}) => ({
  releases: [release(), withdrawnOld()],
  stages: stageRows({ dev: { running_version: 'v2026.48.2', rollback_version: 'v2026.46.1' } }),
  viewer: viewer(true),
  ...over,
});

const routes = (over: Record<string, Handler> = {}) =>
  mockServer(
    repoRoutes({
      'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role: 'maintainer' }),
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

describe('AI asset releases screen', () => {
  it('lists each release with its facts, its stages and a withdrawn one marked in words', async () => {
    const server = routes({ [`GET ${LIST}`]: () => json(200, list()) });
    const { container } = await renderApp({ fetch: server.fetch, path: PAGE, lang: 'en' });
    const item = await screen.findByTestId('ai-release-v2026.48.2');
    expect(within(item).getByRole('heading', { name: 'v2026.48.2' })).toBeTruthy();
    expect(within(item).getByText('assets/v2026.48.2')).toBeTruthy();
    expect(within(item).getByRole('link', { name: 'Open commit cccccccc' }).getAttribute('href')).toBe(
      `/acme/forge-web/-/commit/${'c'.repeat(40)}`,
    );
    expect(within(item).getByText('Passed (check run #41)')).toBeTruthy();
    expect(within(item).getByText('Key 3')).toBeTruthy();
    expect(item.textContent).toContain('Next: Canary (pfs-ai-canary).');
    expect(within(item).getByRole('link', { name: 'Deployment #30' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/deployments/30',
    );
    const old = screen.getByTestId('ai-release-v2026.47.1');
    expect(within(old).getByRole('heading', { name: /v2026\.47\.1\s*Withdrawn/ })).toBeTruthy();
    expect(within(old).getByTestId('ai-release-withdrawal').textContent).toContain(
      'Withdrawn because: Wrong threshold in playbook P-3',
    );
    // A withdrawn release offers no request and no second withdrawal.
    expect(within(old).queryByRole('button')).toBeNull();
    const shadow = screen.getByTestId('ai-stage-pfs-ai-shadow');
    expect(shadow.textContent).toContain('Running: v2026.48.2');
    expect(within(shadow).getByRole('button', { name: 'Roll back to v2026.46.1' })).toBeTruthy();
    expect(screen.getByTestId('ai-stage-pfs-ai-prod').textContent).toContain('Nothing runs here yet.');
    expect(await violations(container)).toEqual([]);
    expect(server.unmatched).toEqual([]);
  });

  it('shows no button the API says the caller may not press', async () => {
    const server = routes({ [`GET ${LIST}`]: () => json(200, list({ viewer: viewer(false) })) });
    await renderApp({ fetch: server.fetch, path: PAGE, lang: 'en' });
    await screen.findByTestId('ai-release-list');
    expect(screen.queryByRole('button', { name: /Request|Withdraw|Roll back/ })).toBeNull();
  });

  it('links a release waiting for its approval to the deployment screen', async () => {
    const waiting = release({
      stages: stages({
        dev: { deployment_id: 30, status: 'succeeded', approved_success: true },
        stg: { deployment_id: 31, status: 'pending_approval' },
      }),
      next_stage: null,
    });
    const server = routes({ [`GET ${LIST}`]: () => json(200, list({ releases: [waiting] })) });
    await renderApp({ fetch: server.fetch, path: PAGE, lang: 'en' });
    const item = await screen.findByTestId('ai-release-v2026.48.2');
    expect(item.textContent).toContain('Waiting for approval on Canary (pfs-ai-canary). Approve it on deployment #31.');
    expect(within(item).getByRole('link', { name: 'Approve on deployment #31' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/deployments/31',
    );
    expect(within(item).queryByRole('button', { name: /Request/ })).toBeNull();
  });

  it('requests the next stage behind a confirmation and links to where it is approved', async () => {
    const server = routes({
      [`GET ${LIST}`]: () => json(200, list()),
      [`POST ${LIST}/v2026.48.2/deployments`]: () =>
        json(201, { id: 44, environment: 'stg', sha: 'c'.repeat(40), status: 'pending_approval', approvals: 0 }),
    });
    await renderApp({ fetch: server.fetch, path: PAGE, lang: 'en' });
    const item = await screen.findByTestId('ai-release-v2026.48.2');
    fireEvent.click(within(item).getByRole('button', { name: 'Request Canary (pfs-ai-canary)' }));
    expect(server.to('POST', `${LIST}/v2026.48.2/deployments`)).toHaveLength(0);
    const confirm = within(item).getByRole('group');
    expect(confirm.textContent).toContain('Request v2026.48.2 on Canary (pfs-ai-canary)?');
    fireEvent.click(within(confirm).getByRole('button', { name: 'Request' }));
    await waitFor(() => {
      expect(server.to('POST', `${LIST}/v2026.48.2/deployments`)).toHaveLength(1);
    });
    const call = server.to('POST', `${LIST}/v2026.48.2/deployments`)[0];
    expect(call?.body).toEqual({ environment: 'pfs-ai-canary' });
    expect(call?.headers.get('x-gitstation-csrf')).toBe(SESSION.csrf_token);
    const link = await within(item).findByRole('link', { name: 'Approve on deployment #44' });
    expect(link.getAttribute('href')).toBe('/acme/forge-web/-/deployments/44');
  });

  it('rolls a stage back behind a confirmation', async () => {
    const server = routes({
      [`GET ${LIST}`]: () => json(200, list()),
      [`POST ${ROLLBACKS}`]: () =>
        json(201, {
          release: release({ version: 'v2026.46.1' }),
          deployment: { id: 45, environment: 'dev', sha: 'a'.repeat(40), status: 'pending_approval', approvals: 0 },
        }),
    });
    await renderApp({ fetch: server.fetch, path: PAGE, lang: 'en' });
    const shadow = await screen.findByTestId('ai-stage-pfs-ai-shadow');
    fireEvent.click(within(shadow).getByRole('button', { name: 'Roll back to v2026.46.1' }));
    fireEvent.click(within(shadow).getByRole('button', { name: 'Request the rollback' }));
    await waitFor(() => {
      expect(server.to('POST', ROLLBACKS)).toHaveLength(1);
    });
    expect(server.to('POST', ROLLBACKS)[0]?.body).toEqual({ environment: 'pfs-ai-shadow' });
    expect(
      (await within(shadow).findByRole('link', { name: 'Approve on deployment #45' })).getAttribute('href'),
    ).toBe('/acme/forge-web/-/deployments/45');
  });

  it('withdraws only with a reason, and shows a refusal with its request_id', async () => {
    let refuse = true;
    const server = routes({
      [`GET ${LIST}`]: () => json(200, list()),
      [`POST ${LIST}/v2026.48.2/withdrawal`]: (c) =>
        refuse
          ? apiError(409, 'This AI asset release cannot be withdrawn.', 'req-withdraw-1')
          : json(201, {
              version: 'v2026.48.2',
              reason: (c.body as { reason: string }).reason,
              withdrawn_by: 1,
              withdrawn_at: '2026-11-28T00:00:00Z',
            }),
    });
    await renderApp({ fetch: server.fetch, path: PAGE, lang: 'en' });
    const item = await screen.findByTestId('ai-release-v2026.48.2');
    fireEvent.click(within(item).getByRole('button', { name: 'Withdraw' }));
    const group = within(item).getByRole('group');
    fireEvent.click(within(group).getByRole('button', { name: 'Withdraw' }));
    expect(within(group).getByText('Write the reason on one line, up to 500 characters.')).toBeTruthy();
    expect(server.to('POST', `${LIST}/v2026.48.2/withdrawal`)).toHaveLength(0);
    fireEvent.change(within(group).getByLabelText('Reason for the withdrawal'), {
      target: { value: 'Bad model digest' },
    });
    fireEvent.click(within(group).getByRole('button', { name: 'Withdraw' }));
    expect(await within(item).findByText('req-withdraw-1')).toBeTruthy();
    expect(within(item).getByText('This AI asset release cannot be withdrawn.')).toBeTruthy();
    refuse = false;
    fireEvent.click(within(item).getByRole('button', { name: 'Withdraw' }));
    const again = within(item).getByRole('group');
    fireEvent.change(within(again).getByLabelText('Reason for the withdrawal'), {
      target: { value: 'Bad model digest' },
    });
    fireEvent.click(within(again).getByRole('button', { name: 'Withdraw' }));
    await waitFor(() => {
      expect(server.to('POST', `${LIST}/v2026.48.2/withdrawal`)).toHaveLength(2);
    });
    expect(server.to('POST', `${LIST}/v2026.48.2/withdrawal`)[1]?.body).toEqual({ reason: 'Bad model digest' });
    expect(await within(item).findByText('Withdrew v2026.48.2.')).toBeTruthy();
  });

  it('says how to make the first release when there is none', async () => {
    const server = routes({
      [`GET ${LIST}`]: () => json(200, list({ releases: [], stages: stageRows() })),
    });
    await renderApp({ fetch: server.fetch, path: PAGE, lang: 'en' });
    const empty = await screen.findByTestId('ai-releases-empty');
    expect(empty.textContent).toContain('gstn ai-asset release --project 7');
  });

  it('shows a failed read with its request_id', async () => {
    const server = routes({ [`GET ${LIST}`]: () => apiError(503, 'Temporarily unavailable.', 'req-rel-list') });
    await renderApp({ fetch: server.fetch, path: PAGE, lang: 'en' });
    expect(await screen.findByText('req-rel-list')).toBeTruthy();
  });

  it('is linked from the deployments screen when AI assets are on', async () => {
    const server = routes({
      'GET /api/v1/projects/7/deployments': () => json(200, { as_of: '2026-11-20T00:02:00Z', deployments: [] }),
      'GET /api/v1/projects/7/ai-assets': () =>
        json(200, { project_id: 7, enabled: true, changed_by: 5, changed_at: '2026-11-15T00:00:00Z', viewer: { can_change: false } }),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/deployments', lang: 'en' });
    const link = await screen.findByTestId('ai-releases-link');
    expect(link.getAttribute('href')).toBe(PAGE);
  });
});

describe('AI asset release words', () => {
  const base = release() as unknown as Release;

  it('says what comes next', async () => {
    const t = (await createI18n('en')).t;
    expect(releaseLine(t, base)).toBe('Next: Canary (pfs-ai-canary).');
    expect(releaseLine(t, { ...base, key_revoked: true, next_stage: null })).toContain('signing key was revoked');
    expect(releaseLine(t, withdrawnOld() as unknown as Release)).toContain('Withdrawn.');
    const failed = {
      ...base,
      stages: stages({
        dev: { deployment_id: 30, status: 'succeeded', approved_success: true },
        stg: { deployment_id: 32, status: 'failed' },
      }),
    } as unknown as Release;
    expect(releaseLine(t, failed)).toBe('It did not reach Canary (pfs-ai-canary). You can request it again.');
    expect(releaseLine(t, { ...base, next_stage: null })).toBe('It reached production.');
  });

  it('checks a reason as the server does', () => {
    expect(withdrawReasonValid('Bad model digest')).toBe(true);
    expect(withdrawReasonValid('   ')).toBe(false);
    expect(withdrawReasonValid('two\nlines')).toBe(false);
    expect(withdrawReasonValid('x'.repeat(500))).toBe(true);
    expect(withdrawReasonValid('x'.repeat(501))).toBe(false);
  });
});
