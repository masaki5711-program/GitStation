// Plan 14 M4: the CI pipeline screens. The list (status as a word, ref,
// short sha to the commit, source in plain words, who, paging, the empty
// state's next step); one pipeline (jobs by stage, the one line for a failed
// one with its diagnosis, Cancel / Retry only when the status and the role
// allow, a refusal's user_message and request_id); a job's log as text only
// (no HTML), with the truncation note; the sidebar item, the info panel and
// the MR check's link. Synthetic data only.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { pipelineIdOfCheck } from '../src/api/pipelines';
import { logText } from '../src/pages/repo/pipelines/JobLog';
import { groupByStage } from '../src/pages/repo/pipelines/PipelineScreen';
import { PROJECT, SESSION, apiError, mockServer, type Handler } from './mock';
import { repoRoutes } from './repo-fixtures';
import { json, renderApp } from './render';

const SHA = 'c'.repeat(40);
const API = '/api/v1/projects/7/pipelines';
const BASE = '/acme/forge-web/-/pipelines';

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
  protected: true,
  copy_mark: 'open',
  definition_hash: 'h',
  created_at: '2026-11-10T00:00:00Z',
  updated_at: '2026-11-10T00:01:05Z',
  finished_at: '2026-11-10T00:01:05Z',
  jobs: null,
  ...over,
});

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
  attempt: 1,
  error_code: null,
  created_at: '2026-11-10T00:00:00Z',
  started_at: '2026-11-10T00:00:01Z',
  finished_at: '2026-11-10T00:00:31Z',
  ...over,
});

const FAILED = pipeline({
  status: 'failed',
  jobs: [
    job(),
    job({ id: 101, name: 'unit', stage: 'test', wave: 1, needs: ['build'], status: 'failed', error_code: 'FG-CI-0009' }),
    job({ id: 102, name: 'lint', stage: 'test', wave: 1, needs: ['build'], status: 'canceled' }),
  ],
});

const DIAGNOSIS = {
  pipeline_id: 12,
  status: 'failed',
  error_code: null,
  failures: [
    {
      job: 'unit',
      job_id: 101,
      stage: 'test',
      attempt: 1,
      pending: false,
      fingerprint: 'fp_0123456789abcdef',
      error_codes: ['FG-CI-0009'],
      excerpt: 'test press_line::stops ... FAILED',
      source: { log_path: `${API}/12/jobs/101/log`, lines: [10, 12] },
      occurrences: 3,
      first_seen_at: '2026-11-01T00:00:00Z',
      known_fixes: [{ merge_request_id: 40, iid: 4, title: 'Fix the stop timer', fixed_at: '2026-11-02T00:00:00Z' }],
    },
  ],
};

const logPage = (over: Record<string, unknown> = {}) => ({
  pipeline_id: 12,
  job_id: 101,
  job: 'unit',
  job_status: 'failed',
  attempt: 1,
  current_attempt: 1,
  from: 0,
  to: 0,
  text: 'line one\n  indented\n',
  next_from: null,
  complete: true,
  ...over,
});

const routes = (over: Record<string, Handler> = {}) =>
  mockServer(repoRoutes({ ...over }));

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help}`);
}

describe('pipeline list', () => {
  it('shows status in words, ref, short sha to the commit, source and who; pages by next_before', async () => {
    const server = routes({
      [`GET ${API}`]: (c) =>
        c.query.get('before') === null
          ? json(200, {
              pipelines: [
                pipeline(),
                pipeline({ id: 11, status: 'failed', source: 'merge_request', triggered_by: 99, ref: 'refs/heads/feature/x' }),
              ],
              next_before: 11,
            })
          : json(200, { pipelines: [pipeline({ id: 3, source: 'schedule', triggered_by: null })], next_before: null }),
    });
    await renderApp({ fetch: server.fetch, path: BASE });
    const list = await screen.findByTestId('pipeline-list');
    const items = within(list).getAllByRole('listitem');
    expect(items).toHaveLength(2);
    const first = items[0] as HTMLElement;
    expect(within(first).getByText('成功')).toBeTruthy();
    expect(within(first).getByRole('link', { name: 'パイプライン #12' }).getAttribute('href')).toBe(`${BASE}/12`);
    expect(within(first).getByText('main')).toBeTruthy();
    expect(
      within(first).getByRole('link', { name: `コミット ${SHA.slice(0, 8)} を開く` }).getAttribute('href'),
    ).toBe(`/acme/forge-web/-/commit/${SHA}`);
    expect(within(first).getByText('push')).toBeTruthy();
    expect(within(first).getByText('あなた')).toBeTruthy();
    expect(within(first).getByText('所要 1分5秒')).toBeTruthy();
    const second = items[1] as HTMLElement;
    expect(within(second).getByText('失敗')).toBeTruthy();
    expect(within(second).getByText('MR')).toBeTruthy();
    expect(within(second).getByText('ユーザー #99')).toBeTruthy();
    expect(within(second).getByText('feature/x')).toBeTruthy();

    fireEvent.click(screen.getByRole('link', { name: 'さらに古いパイプライン' }));
    await screen.findByRole('link', { name: 'パイプライン #3' });
    expect(screen.getByText('定期実行')).toBeTruthy();
    expect(screen.getByText('自動')).toBeTruthy();
    expect(screen.getByTestId('location').textContent).toBe(`${BASE}?before=11`);
    expect(server.to('GET', API).at(-1)?.query.get('before')).toBe('11');
    expect(screen.queryByRole('link', { name: 'さらに古いパイプライン' })).toBeNull();
    expect(screen.getByRole('link', { name: '最新へ' }).getAttribute('href')).toBe(BASE);
    expect(server.unmatched).toEqual([]);
  });

  it('empty: says how to get one (.forge-ci.yml) and links to the files', async () => {
    const server = routes({ [`GET ${API}`]: () => json(200, { pipelines: [], next_before: null }) });
    await renderApp({ fetch: server.fetch, path: BASE });
    const empty = await screen.findByTestId('pipelines-empty');
    expect(empty.textContent).toContain('.forge-ci.yml');
    expect(within(empty).getByRole('link', { name: 'ファイルを開く' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/tree',
    );
  });

  it('a failed read shows the user_message and request_id', async () => {
    const server = routes({ [`GET ${API}`]: () => apiError(503, '一時的に読めません。', 'req-list-1') });
    await renderApp({ fetch: server.fetch, path: BASE });
    expect(await screen.findByText('一時的に読めません。')).toBeTruthy();
    expect(screen.getByText('req-list-1')).toBeTruthy();
  });

  it('an address that is not a pipeline is not found, without asking the API', async () => {
    const server = routes();
    await renderApp({ fetch: server.fetch, path: `${BASE}/012` });
    await screen.findByRole('heading', { level: 1 });
    expect(server.to('GET', `${API}/012`)).toEqual([]);
    expect(server.calls.some((c) => c.path.startsWith(API))).toBe(false);
  });
});

describe('one pipeline', () => {
  it('failed: one line naming the job and what to press, the diagnosis, jobs by stage; Retry for a Developer', async () => {
    const server = routes({
      [`GET ${API}/12`]: () => json(200, FAILED),
      [`GET ${API}/12/diagnosis`]: () => json(200, DIAGNOSIS),
    });
    const { container } = await renderApp({ fetch: server.fetch, path: `${BASE}/12` });
    await screen.findByRole('heading', { level: 1, name: /パイプライン #12/ });
    const line = screen.getByTestId('pipeline-status-line');
    expect(line.textContent).toContain('ジョブ「unit」が失敗したため止まりました');
    expect(line.textContent).toContain('「再実行」を押してください');
    expect(within(line).getByRole('link', { name: '失敗したジョブ「unit」のログを見る' }).getAttribute('href')).toBe(
      `${BASE}/12?job=101`,
    );
    const diagnosis = await screen.findByTestId('pipeline-failure');
    expect(diagnosis.textContent).toContain('test press_line::stops ... FAILED');
    expect(diagnosis.textContent).toContain('同じ失敗が 3 回');
    expect(within(diagnosis).getByRole('link', { name: '!4 Fix the stop timer' }).getAttribute('href')).toBe(
      '/acme/forge-web/-/pull/4',
    );
    // Error codes stay in the logs (contract 3).
    expect(container.textContent).not.toContain('FG-CI-0009');
    const jobs = screen.getAllByTestId('pipeline-job');
    expect(jobs.map((j) => within(j).getByRole('code').textContent)).toEqual(['build', 'unit', 'lint']);
    expect(screen.getByRole('heading', { name: 'ステージ build' })).toBeTruthy();
    expect(screen.getByRole('heading', { name: 'ステージ test' })).toBeTruthy();
    expect(within(jobs[1] as HTMLElement).getByText('待つジョブ: build')).toBeTruthy();
    expect(screen.getByRole('button', { name: '再実行' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: '取り消す' })).toBeNull();
    expect(screen.getAllByRole('status').some((s) => s.textContent === '状態: 失敗')).toBe(true);
    expect(await violations(container)).toEqual([]);
    expect(server.unmatched).toEqual([]);
  });

  it('a Reporter sees no buttons and no "press Retry"', async () => {
    const server = routes({
      'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role: 'reporter' }),
      [`GET ${API}/12`]: () => json(200, FAILED),
      [`GET ${API}/12/diagnosis`]: () => json(200, DIAGNOSIS),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/12` });
    const line = await screen.findByTestId('pipeline-status-line');
    expect(line.textContent).toContain('修正を push してください');
    expect(line.textContent).not.toContain('再実行');
    expect(screen.queryByRole('button', { name: '再実行' })).toBeNull();
    expect(screen.queryByTestId('pipeline-actions')).toBeNull();
  });

  it('running: Cancel only; pressing it shows the answer', async () => {
    let status = 'running';
    const server = routes({
      [`GET ${API}/12`]: () => json(200, pipeline({ status, finished_at: null, jobs: [job({ status: 'running', finished_at: null })] })),
      [`POST ${API}/12/cancel`]: () => {
        status = 'canceled';
        return json(200, pipeline({ status: 'canceled', jobs: [job({ status: 'canceled' })] }));
      },
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/12` });
    const cancel = await screen.findByRole('button', { name: '取り消す' });
    expect(screen.queryByRole('button', { name: '再実行' })).toBeNull();
    expect(screen.getByTestId('pipeline-status-line').textContent).toContain('実行中です');
    fireEvent.click(cancel);
    await waitFor(() => {
      expect(screen.getByTestId('pipeline-status-line').textContent).toContain('取り消されました');
    });
    expect(server.to('POST', `${API}/12/cancel`)).toHaveLength(1);
    // A canceled one can be retried.
    expect(screen.getByRole('button', { name: '再実行' })).toBeTruthy();
  });

  it('a refused retry shows the user_message and request_id, never the code', async () => {
    const server = routes({
      [`GET ${API}/12`]: () => json(200, FAILED),
      [`GET ${API}/12/diagnosis`]: () => json(200, DIAGNOSIS),
      [`POST ${API}/12/retry`]: () =>
        apiError(403, 'この操作の権限がありません。', 'req-retry-403', 'FG-AUTH-0001'),
    });
    const { container } = await renderApp({ fetch: server.fetch, path: `${BASE}/12` });
    fireEvent.click(await screen.findByRole('button', { name: '再実行' }));
    expect(await screen.findByText('この操作の権限がありません。')).toBeTruthy();
    expect(screen.getByText('req-retry-403')).toBeTruthy();
    expect(container.textContent).not.toContain('FG-AUTH-0001');
  });

  it('a stale retry (409) shows its message too', async () => {
    const server = routes({
      [`GET ${API}/12`]: () => json(200, FAILED),
      [`GET ${API}/12/diagnosis`]: () => json(200, DIAGNOSIS),
      [`POST ${API}/12/retry`]: () => apiError(409, 'すでに状態が変わっています。', 'req-retry-409', 'FG-CI-0003'),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/12` });
    fireEvent.click(await screen.findByRole('button', { name: '再実行' }));
    expect(await screen.findByText('すでに状態が変わっています。')).toBeTruthy();
    expect(screen.getByText('req-retry-409')).toBeTruthy();
  });

  it('invalid: the one line and each problem linked to its line of .forge-ci.yml', async () => {
    const server = routes({
      [`GET ${API}/12`]: () =>
        json(
          200,
          pipeline({
            status: 'invalid',
            jobs: [],
            problems: [
              { line: 4, reason: 'unknown_key', subject: 'scirpt' },
              { line: 0, reason: 'no_jobs', subject: null },
            ],
          }),
        ),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/12` });
    const line = await screen.findByTestId('pipeline-status-line');
    expect(line.textContent).toContain('.forge-ci.yml に誤りがある');
    expect(screen.getByRole('link', { name: '4 行目' }).getAttribute('href')).toBe(
      `/acme/forge-web/-/blob/${SHA}/.forge-ci.yml#L4`,
    );
    expect(screen.getByRole('link', { name: 'ファイル全体' })).toBeTruthy();
    expect(screen.getByText('unknown_key')).toBeTruthy();
    expect(screen.queryByTestId('pipeline-diagnosis')).toBeNull();
  });

  it('not found: the server message and a way back', async () => {
    const server = routes({
      [`GET ${API}/99`]: () => apiError(404, 'パイプラインが見つかりません。', 'req-404-p'),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/99` });
    expect(await screen.findByText('パイプラインが見つかりません。')).toBeTruthy();
    expect(screen.getByText('req-404-p')).toBeTruthy();
    expect(screen.getByRole('link', { name: 'パイプラインの一覧へ' })).toBeTruthy();
  });
});

describe('job log', () => {
  it('is text only: markup in the log is never HTML, whitespace is kept', async () => {
    const evil = '<img src=x onerror="alert(1)"><script>alert(2)</script>\n\tdone';
    const server = routes({
      [`GET ${API}/12`]: () => json(200, FAILED),
      [`GET ${API}/12/diagnosis`]: () => json(200, DIAGNOSIS),
      [`GET ${API}/12/jobs/101/log`]: () => json(200, logPage({ text: evil })),
    });
    const { container } = await renderApp({ fetch: server.fetch, path: `${BASE}/12?job=101` });
    const pre = await screen.findByTestId('job-log-text');
    expect(pre.tagName).toBe('PRE');
    expect(pre.textContent).toBe(evil);
    expect(pre.querySelector('img, script')).toBeNull();
    expect(container.querySelector('img[src="x"]')).toBeNull();
    expect(screen.getByRole('heading', { name: 'ジョブ「unit」のログ' })).toBeTruthy();
    // The open job is marked in the list.
    expect(screen.getByRole('link', { name: 'ジョブ「unit」のログ' }).getAttribute('aria-current')).toBe('true');
  });

  it('pages with next_from, and says so when the log is cut', async () => {
    const server = routes({
      [`GET ${API}/12`]: () => json(200, FAILED),
      [`GET ${API}/12/diagnosis`]: () => json(200, DIAGNOSIS),
      [`GET ${API}/12/jobs/101/log`]: (c) =>
        c.query.get('from') === null
          ? json(200, logPage({ text: 'first page\n', to: 255, next_from: 256, complete: false }))
          : json(200, logPage({ from: 256, to: 300, text: 'second page\n', next_from: null, complete: true })),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/12?job=101` });
    const pre = await screen.findByTestId('job-log-text');
    expect(pre.textContent).toBe('first page\n');
    fireEvent.click(screen.getByRole('button', { name: '続きを読む' }));
    await waitFor(() => {
      expect(screen.getByTestId('job-log-text').textContent).toBe('first page\nsecond page\n');
    });
    expect(server.to('GET', `${API}/12/jobs/101/log`).at(-1)?.query.get('from')).toBe('256');
    expect(screen.queryByRole('button', { name: '続きを読む' })).toBeNull();
    expect(screen.queryByText('ログが長いため、途中までを表示しています。')).toBeNull();
  });

  it('cuts a very long text with a note; drops terminal colour codes', () => {
    expect(logText('abcdef', 3)).toEqual({ text: 'abc', cut: true });
    const esc = String.fromCharCode(27);
    expect(logText(`${esc}[31mred${esc}[0m ok`)).toEqual({ text: 'red ok', cut: false });
  });

  it('a long log on screen carries the truncation note', async () => {
    const server = routes({
      [`GET ${API}/12`]: () => json(200, FAILED),
      [`GET ${API}/12/diagnosis`]: () => json(200, DIAGNOSIS),
      [`GET ${API}/12/jobs/101/log`]: () => json(200, logPage({ text: 'x'.repeat(400_001) })),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/12?job=101` });
    expect(await screen.findByText('ログが長いため、途中までを表示しています。')).toBeTruthy();
    expect(screen.getByTestId('job-log-text').textContent).toHaveLength(400_000);
  });

  it('a job that has not started yet says the log comes later', async () => {
    const server = routes({
      [`GET ${API}/12`]: () =>
        json(200, pipeline({ status: 'running', finished_at: null, jobs: [job({ status: 'pending', started_at: null, finished_at: null })] })),
      [`GET ${API}/12/jobs/100/log`]: () =>
        json(200, logPage({ job_id: 100, job: 'build', job_status: 'pending', from: null, to: null, text: '', complete: false })),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/12?job=100` });
    expect(await screen.findByText('まだログがありません。ジョブが始まると表示されます。')).toBeTruthy();
    expect(screen.getByText('実行中のため、数秒ごとに更新しています。')).toBeTruthy();
  });
});

describe('ways in', () => {
  it('groups jobs by stage in the order their first job runs', () => {
    const groups = groupByStage([
      job({ id: 3, stage: 'deploy', wave: 2 }),
      job({ id: 2, stage: 'test', wave: 1 }),
      job({ id: 1, stage: 'build', wave: 0 }),
      job({ id: 4, stage: 'test', wave: 1 }),
    ] as Parameters<typeof groupByStage>[0]);
    expect(groups.map((g) => [g.stage, g.jobs.map((j) => j.id)])).toEqual([
      ['build', [1]],
      ['test', [2, 4]],
      ['deploy', [3]],
    ]);
  });

  it('the sidebar and the project info panel lead to the pipelines; the sidebar marks it', async () => {
    const server = routes({ [`GET ${API}`]: () => json(200, { pipelines: [], next_before: null }) });
    await renderApp({ fetch: server.fetch, path: BASE });
    const nav = await screen.findByRole('navigation', { name: 'プロジェクトのメニュー' });
    const item = within(nav).getByRole('link', { name: 'パイプライン' });
    expect(item.getAttribute('href')).toBe(BASE);
    expect(item.getAttribute('aria-current')).toBe('page');
  });

  it('the project top info panel links to the pipelines', async () => {
    const server = routes();
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
    const info = await screen.findByRole('complementary', { name: 'プロジェクト情報' });
    expect(within(info).getByRole('link', { name: 'パイプライン' }).getAttribute('href')).toBe(BASE);
  });

  it('the ci/pipeline check names its pipeline; other checks do not', () => {
    expect(pipelineIdOfCheck({ name: 'ci/pipeline', summary: 'Pipeline #42 failed: 3 jobs, 1 succeeded, 1 failed, 1 canceled.' })).toBe(42);
    expect(pipelineIdOfCheck({ name: 'ci/pipeline', summary: 'nothing' })).toBeNull();
    expect(pipelineIdOfCheck({ name: 'forge/ci-lint', summary: 'Pipeline #42 failed: x' })).toBeNull();
  });
});
