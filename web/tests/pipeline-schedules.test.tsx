// M4 C12: manual jobs and pipeline schedules. A manual job waiting for a
// person shows a "run" button for a Developer (one line says so) and none
// for a Reporter; pressing it starts it. The schedules page lists each
// schedule with its next run and one line on its last run (why nothing
// started and what to do); a Maintainer adds one, a Developer is told whom
// to ask. Synthetic data only.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { lastRunSentence, offsetLabel } from '../src/pages/repo/pipelines/SchedulesScreen';
import { PROJECT, SESSION, mockServer, type Handler } from './mock';
import { repoRoutes } from './repo-fixtures';
import { json, renderApp } from './render';

const SHA = 'c'.repeat(40);
const API = '/api/v1/projects/7/pipelines';
const SCHEDULES = '/api/v1/projects/7/pipeline-schedules';
const BASE = '/acme/forge-web/-/pipelines';

const job = (over: Record<string, unknown> = {}) => ({
  id: 100,
  name: 'build',
  stage: 'build',
  wave: 0,
  needs: [],
  image: 'registry.local/rust@sha256:' + 'd'.repeat(64),
  timeout_s: 600,
  tags: [],
  secrets: [],
  status: 'succeeded',
  manual: false,
  attempt: 1,
  error_code: null,
  created_at: '2026-11-10T00:00:00Z',
  started_at: '2026-11-10T00:00:01Z',
  finished_at: '2026-11-10T00:00:31Z',
  ...over,
});

const pipeline = (over: Record<string, unknown> = {}) => ({
  id: 12,
  project_id: 7,
  ref: 'refs/heads/main',
  sha: SHA,
  source: 'push',
  merge_request_id: null,
  triggered_by: SESSION.user.id,
  status: 'succeeded',
  error_code: null,
  problems: [],
  protected: false,
  copy_mark: 'open',
  definition_hash: 'h',
  created_at: '2026-11-10T00:00:00Z',
  updated_at: '2026-11-10T00:01:05Z',
  finished_at: '2026-11-10T00:01:05Z',
  jobs: [
    job(),
    job({
      id: 101,
      name: 'deploy',
      stage: 'deploy',
      wave: 1,
      needs: ['build'],
      status: 'manual',
      manual: true,
      started_at: null,
      finished_at: null,
    }),
  ],
  ...over,
});

const schedule = (over: Record<string, unknown> = {}) => ({
  id: 3,
  project_id: 7,
  description: 'Nightly build',
  ref: 'refs/heads/main',
  cron: '0 2 * * *',
  utc_offset_minutes: 540,
  active: true,
  owner_id: 99,
  next_run_at: '2026-11-14T17:00:00Z',
  last_run_at: '2026-11-13T17:00:00Z',
  last_outcome: 'refused',
  last_pipeline_id: null,
  last_error_code: 'FG-CI-0026',
  created_at: '2026-11-01T00:00:00Z',
  updated_at: '2026-11-01T00:00:00Z',
  ...over,
});

const routes = (over: Record<string, Handler> = {}) => mockServer(repoRoutes({ ...over }));

describe('manual jobs', () => {
  it('a Developer sees one line and a "run" button; pressing it starts the job', async () => {
    let started = false;
    const server = routes({
      [`GET ${API}/12`]: () =>
        json(
          200,
          started
            ? pipeline({ status: 'running', jobs: [job(), job({ id: 101, name: 'deploy', stage: 'deploy', status: 'pending', manual: true })] })
            : pipeline(),
        ),
      [`POST ${API}/12/jobs/101/play`]: () => {
        started = true;
        return json(200, pipeline({ status: 'running', jobs: [job(), job({ id: 101, name: 'deploy', stage: 'deploy', status: 'pending', manual: true })] }));
      },
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/12` });
    const line = await screen.findByTestId('pipeline-manual-line');
    expect(line.textContent).toContain('ジョブ「deploy」は手動で始めるジョブです');
    expect(line.textContent).toContain('「実行」を押してください');
    const run = await screen.findByRole('button', { name: 'ジョブ「deploy」を実行' });
    fireEvent.click(run);
    await waitFor(() => {
      expect(server.to('POST', `${API}/12/jobs/101/play`)).toHaveLength(1);
    });
    await waitFor(() => {
      expect(screen.queryByRole('button', { name: 'ジョブ「deploy」を実行' })).toBeNull();
    });
    expect(screen.getByText('手動で開始')).toBeTruthy();
  });

  it('a Reporter sees no "run" button and whom to ask', async () => {
    const server = routes({
      'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role: 'reporter' }),
      [`GET ${API}/12`]: () => json(200, pipeline()),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/12` });
    const line = await screen.findByTestId('pipeline-manual-line');
    expect(line.textContent).toContain('Developer 以上のメンバーに依頼してください');
    expect(screen.queryByRole('button', { name: /を実行/ })).toBeNull();
  });
});

describe('manual jobs of a protected pipeline', () => {
  it('a Developer sees no "run" button and is told to ask a Maintainer; a Maintainer gets it', async () => {
    const dev = routes({
      [`GET ${API}/12`]: () => json(200, pipeline({ protected: true })),
    });
    await renderApp({ fetch: dev.fetch, path: `${BASE}/12` });
    const line = await screen.findByTestId('pipeline-manual-line');
    expect(line.textContent).toContain('Maintainer 以上のメンバーに依頼してください');
    expect(screen.queryByRole('button', { name: /を実行/ })).toBeNull();
  });

  it('a Maintainer gets the "run" button', async () => {
    const server = routes({
      'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role: 'maintainer' }),
      [`GET ${API}/12`]: () => json(200, pipeline({ protected: true })),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/12` });
    expect(await screen.findByRole('button', { name: 'ジョブ「deploy」を実行' })).toBeTruthy();
  });
});

describe('pipeline schedules', () => {
  it('lists schedules with the next run and why the last run started nothing', async () => {
    const server = routes({
      'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role: 'maintainer' }),
      [`GET ${SCHEDULES}`]: () =>
        json(200, {
          schedules: [
            schedule(),
            schedule({ id: 4, description: 'Weekly', last_outcome: 'created', last_pipeline_id: 12, last_error_code: null, owner_id: SESSION.user.id }),
          ],
        }),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/schedules` });
    await screen.findByRole('heading', { level: 1, name: '定期実行' });
    const items = await screen.findAllByTestId('schedule-item');
    expect(items).toHaveLength(2);
    const first = items[0] as HTMLElement;
    expect(first.textContent).toContain('Nightly build');
    expect(first.textContent).toContain('0 2 * * *');
    expect(first.textContent).toContain('UTC+09:00 (JST)');
    expect(first.textContent).toContain('「編集」から有効にして保存すると、あなたの権限で動きます');
    // Codes stay out of the page (contract 3) unless it is a failure to report.
    expect(first.textContent).not.toContain('FG-CI-0026');
    const second = items[1] as HTMLElement;
    expect(second.textContent).toContain('持ち主: あなた');
    expect(within(second).getByRole('link', { name: 'パイプライン #12' }).getAttribute('href')).toBe(`${BASE}/12`);
    expect(screen.getByRole('button', { name: '定期実行を追加' })).toBeTruthy();
  });

  it('a Maintainer adds one; the form sends the rule and the offset', async () => {
    const server = routes({
      'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role: 'maintainer' }),
      [`GET ${SCHEDULES}`]: () => json(200, { schedules: [] }),
      [`POST ${SCHEDULES}`]: () => json(201, schedule({ last_outcome: null, last_error_code: null })),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/schedules` });
    fireEvent.click(await screen.findByRole('button', { name: '定期実行を追加' }));
    const form = await screen.findByTestId('schedule-form');
    fireEvent.change(within(form).getByLabelText('説明'), { target: { value: 'Nightly build' } });
    fireEvent.click(within(form).getByRole('button', { name: '保存' }));
    await waitFor(() => {
      expect(server.to('POST', SCHEDULES)).toHaveLength(1);
    });
    const sent = server.to('POST', SCHEDULES)[0]?.body as Record<string, unknown>;
    expect(sent).toEqual({
      description: 'Nightly build',
      ref: 'main',
      cron: '0 2 * * *',
      utc_offset_minutes: 540,
      active: true,
    });
  });

  it('a Developer gets the empty state with whom to ask, and no add button', async () => {
    const server = routes({
      [`GET ${SCHEDULES}`]: () => json(200, { schedules: [] }),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/schedules` });
    expect(await screen.findByText('まだ定期実行がありません。')).toBeTruthy();
    expect(screen.getByText(/Maintainer 以上のメンバーに依頼してください/)).toBeTruthy();
    expect(screen.queryByRole('button', { name: '定期実行を追加' })).toBeNull();
  });

  it('the pipelines page links to the schedules', async () => {
    const server = routes({
      [`GET ${API}`]: () => json(200, { pipelines: [], next_before: null }),
    });
    await renderApp({ fetch: server.fetch, path: BASE });
    expect((await screen.findByRole('link', { name: '定期実行を見る' })).getAttribute('href')).toBe(`${BASE}/schedules`);
  });

  it('offsets and last-run lines in words', () => {
    expect(offsetLabel(540)).toBe('UTC+09:00 (JST)');
    expect(offsetLabel(-330)).toBe('UTC-05:30');
    const t = ((key: string) => key) as unknown as Parameters<typeof lastRunSentence>[0];
    expect(lastRunSentence(t, schedule({ last_outcome: null }) as never, true)).toBe('pipelines.schedules.last.never');
    expect(lastRunSentence(t, schedule({ last_outcome: 'refused' }) as never, false)).toBe(
      'pipelines.schedules.last.refusedAsk',
    );
    expect(lastRunSentence(t, schedule({ last_outcome: 'busy' }) as never, true)).toBe('pipelines.schedules.last.busy');
    expect(lastRunSentence(t, schedule({ last_outcome: 'capped' }) as never, true)).toBe(
      'pipelines.schedules.last.capped',
    );
  });
});
