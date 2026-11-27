// Plan 16 A8: project settings → "Agent" (/{project}/-/settings/agent):
// the adapter, the recommended parts as checkboxes, the Dry-Run's diff,
// "Create the merge request" and the update band. The named test: nobody
// is shown a button the server would refuse — a Reporter reads the
// recommendation only; the Dry-Run and the merge request are a
// Developer's. axe checks the screen.
//
// Plan 16 A12: the update band reads `GET …/agent-builder/updates` (revoked
// and dropped parts, "Create the update merge request" only when the API
// says `can_update`), and the recommendation shows the local LLM's
// reasons — labelled 「AI の助言（提案）」 in words, plain text, the
// suggested parts never pre-selected and with their capability marks, and
// a note when there are no reasons now.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { summary } from './marketplace-fixtures';
import { apiError, mockServer, PROJECT, REFS_FILLED, RESOLVE_PROJECT, SESSION, signedIn, type Handler } from './mock';
import { json, renderApp } from './render';

const API = '/api/v1/projects/7';
const B = `${API}/agent-builder`;
const PATH = '/acme/forge-web/-/settings/agent';

const NO_LLM = { state: 'ready', dropped: 0 };

const RECOMMEND = {
  source: 'rules',
  degraded: false,
  llm: NO_LLM,
  suggested: [],
  unavailable: [],
  parts: [
    { kind: 'adapter', slug: 'claude-code-v2', version: '2.1.0', selected_by: 'rule', reason: 'base_model' },
    { kind: 'skill', slug: 'review-skill', version: '1.2.0', selected_by: 'rule', reason: 'current_lock' },
    { kind: 'hook', slug: 'fmt-hook', version: '0.2.0', selected_by: 'rule', reason: 'required_by=review-skill' },
  ],
};

const PREVIEW = {
  base_sha: 'a'.repeat(40),
  target_branch: 'main',
  lock: '{}',
  lock_digest: `sha256:${'1'.repeat(64)}`,
  changes: [
    { change: 'added', path: '.claude/hooks/fmt.js' },
    { change: 'modified', path: '.claude/settings.json' },
    { change: 'removed', path: '.claude/skills/old/SKILL.md' },
  ],
  parts: [
    { slug: 'claude-code-v2', kind: 'adapter', version: '2.1.0', channel: 'stable', capabilities: [] },
    { slug: 'review-skill', kind: 'skill', version: '1.2.0', channel: 'stable', capabilities: [] },
    { slug: 'fmt-hook', kind: 'hook', version: '0.2.0', channel: 'stable', capabilities: ['exec', 'permissions'] },
  ],
};

const UP_TO_DATE = { available: false, reason: 'up_to_date', parts: [], preview: null, can_update: true };

const UPDATES = {
  available: true,
  reason: null,
  can_update: true,
  preview: PREVIEW,
  parts: [
    { slug: 'claude-code-v2', kind: 'adapter', locked_version: '2.1.0', locked_revoked: false, next_version: '2.1.0' },
    { slug: 'fmt-hook', kind: 'hook', locked_version: '0.1.0', locked_revoked: true, next_version: '0.2.0' },
    { slug: 'lint-skill', kind: 'skill', locked_version: null, locked_revoked: false, next_version: '1.0.0' },
    { slug: 'old-rule', kind: 'rule', locked_version: '1.0.0', locked_revoked: true, next_version: null },
    { slug: 'review-skill', kind: 'skill', locked_version: '1.1.0', locked_revoked: false, next_version: '1.2.0' },
  ],
};

function routes(role: string, over: Record<string, Handler> = {}): Record<string, Handler> {
  return {
    ...signedIn(),
    'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_PROJECT),
    [`GET ${API}`]: () => json(200, { ...PROJECT, role }),
    [`GET ${API}/repository/refs`]: () => json(200, REFS_FILLED),
    [`GET ${B}`]: () => json(200, { builds: [] }),
    [`GET ${B}/updates`]: () => json(200, UP_TO_DATE),
    [`POST ${B}/recommend`]: () => json(200, RECOMMEND),
    'GET /api/v1/agent-parts': () =>
      json(200, {
        parts: [
          summary({ slug: 'claude-code-v2', kind: 'adapter' }, { version: '2.1.0' }),
          summary({ slug: 'tmux-adapter', kind: 'adapter' }, { version: '1.0.0', channel: 'beta' }),
          summary({ slug: 'review-skill' }, { version: '1.2.0' }),
          summary({ slug: 'fmt-hook', kind: 'hook' }, { version: '0.2.0', capabilities: ['exec', 'permissions'] }),
          summary({ slug: 'kanflow-mcp', kind: 'mcp' }, { version: '2.0.0', capabilities: ['network'] }),
        ],
      }),
    ...over,
  };
}

async function open(role: string, over: Record<string, Handler> = {}) {
  const server = mockServer(routes(role, over));
  const view = await renderApp({ fetch: server.fetch, path: PATH, lang: 'ja' });
  await screen.findByRole('heading', { level: 1, name: 'エージェント' });
  return { server, ...view };
}

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`);
}

describe('agent builder', () => {
  it('a Reporter reads the recommendation but is shown no Dry-Run or merge request button', async () => {
    const { server } = await open('reporter');
    expect(await screen.findByText('review-skill')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Dry-Run で差分を見る' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'MR を作る' })).toBeNull();
    expect(screen.queryByRole('checkbox')).toBeNull();
    expect(screen.getByText('部品を選んで MR を作れるのは Developer 以上の人です。')).toBeTruthy();
    // The builds and the updates are a Developer's read: not asked.
    expect(server.to('GET', B)).toHaveLength(0);
    expect(server.to('GET', `${B}/updates`)).toHaveLength(0);
    expect(await violations(document.body)).toEqual([]);
  });

  it('a Guest is told who can, and nothing is asked', async () => {
    const { server } = await open('guest');
    expect(await screen.findByText('エージェントの設定を見られるのは Reporter 以上の人です。')).toBeTruthy();
    expect(server.to('POST', `${B}/recommend`)).toHaveLength(0);
    expect(screen.queryByRole('button', { name: 'Dry-Run で差分を見る' })).toBeNull();
  });

  it('a Developer: adapter, recommended checkboxes, Dry-Run diff with the marks, then the merge request', async () => {
    const { server } = await open('developer', {
      [`GET ${B}/updates`]: () => json(200, UPDATES),
      [`POST ${B}/preview`]: () => json(200, PREVIEW),
      [`POST ${B}/compile`]: () =>
        json(201, {
          branch: 'agent-builder/1111',
          commit_sha: 'c'.repeat(40),
          preview: PREVIEW,
          merge_request: { iid: 12, title: 'Build the agent configuration' },
          merge_request_error: null,
        }),
    });
    // The update band (GET …/updates): revoked parts lead its title.
    const band = await screen.findByRole('region', { name: '取り消された部品があります' });
    expect(band.textContent).toContain('review-skill 1.1.0 → 1.2.0');
    // The adapter: one choice, the recommended one picked.
    const adapter = screen.getByRole('radiogroup', { name: 'アダプター（基本のモデル）' });
    expect(within(adapter).getByRole<HTMLInputElement>('radio', { name: /claude-code-v2/ }).checked).toBe(true);
    // The recommended parts are checked, with the reason in words.
    const review = screen.getByRole<HTMLInputElement>('checkbox', { name: /review-skill/ });
    expect(review.checked).toBe(true);
    expect(screen.getByText('いまの lock にある部品')).toBeTruthy();
    expect(screen.getByText('review-skill が必要とする部品')).toBeTruthy();
    const mcp = screen.getByRole<HTMLInputElement>('checkbox', { name: /kanflow-mcp/ });
    expect(mcp.checked).toBe(false);
    // No merge request before the Dry-Run.
    expect(screen.queryByRole('button', { name: 'MR を作る' })).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Dry-Run で差分を見る' }));
    await waitFor(() => {
      expect(server.to('POST', `${B}/preview`)).toHaveLength(1);
    });
    expect(server.to('POST', `${B}/preview`)[0]?.body).toEqual({
      selection: [
        { slug: 'claude-code-v2', version: '2.1.0', selected_by: 'rule', reason: 'base_model' },
        { slug: 'review-skill', version: '1.2.0', selected_by: 'rule', reason: 'current_lock' },
        { slug: 'fmt-hook', version: '0.2.0', selected_by: 'rule', reason: 'required_by=review-skill' },
      ],
    });
    const diff = await screen.findByRole('region', { name: 'Dry-Run の差分' });
    expect(within(diff).getByText('.claude/hooks/fmt.js')).toBeTruthy();
    expect(within(diff).getByText('追加')).toBeTruthy();
    expect(within(diff).getByText('変更')).toBeTruthy();
    expect(within(diff).getByText('削除')).toBeTruthy();
    // Widening permissions is called out in words, not only in red.
    expect(within(diff).getByText('fmt-hook は権限の設定を広げます。MR のレビューで確かめてください。')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'MR を作る' }));
    await waitFor(() => {
      expect(server.to('POST', `${B}/compile`)).toHaveLength(1);
    });
    const compile = server.to('POST', `${B}/compile`)[0];
    expect(compile?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    const link = await screen.findByRole('link', { name: 'MR !12 を開く' });
    expect(link.getAttribute('href')).toBe('/acme/forge-web/-/pull/12');
    expect(await violations(document.body)).toEqual([]);
  });

  it('a changed choice clears the Dry-Run: the merge request needs a new one', async () => {
    await open('developer', { [`POST ${B}/preview`]: () => json(200, PREVIEW) });
    fireEvent.click(await screen.findByRole('button', { name: 'Dry-Run で差分を見る' }));
    await screen.findByRole('button', { name: 'MR を作る' });
    fireEvent.click(screen.getByRole('checkbox', { name: /kanflow-mcp/ }));
    expect(screen.queryByRole('button', { name: 'MR を作る' })).toBeNull();
  });

  it('a refused Dry-Run shows the server message and the request id', async () => {
    await open('developer', {
      [`POST ${B}/preview`]: () => apiError(422, 'この組み合わせは作れません。', 'req-build-1', 'FG-AGENT-0008'),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Dry-Run で差分を見る' }));
    expect(await screen.findByText('この組み合わせは作れません。')).toBeTruthy();
    expect(screen.getByText('req-build-1')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'MR を作る' })).toBeNull();
  });

  it('a failed recommendation shows its message; no lock means no band', async () => {
    await open('developer', {
      [`POST ${B}/recommend`]: () => apiError(422, '基本のモデルに stable の版がありません。', 'req-rec-1', 'FG-AGENT-0008'),
      [`GET ${B}/updates`]: () => json(200, { ...UP_TO_DATE, reason: 'no_lock' }),
    });
    expect(await screen.findByText('基本のモデルに stable の版がありません。')).toBeTruthy();
    expect(screen.getByText('req-rec-1')).toBeTruthy();
    expect(await screen.findByText('まだ lock がありません。下で部品を選び、最初の MR を作ってください。')).toBeTruthy();
    expect(screen.queryByRole('region', { name: '更新があります' })).toBeNull();
    expect(screen.queryByRole('button', { name: '更新の MR を作る' })).toBeNull();
  });

  it('the builds history lists each build with its request id', async () => {
    await open('developer', {
      [`GET ${B}`]: () =>
        json(200, {
          builds: [
            {
              id: 3,
              created_at: '2026-11-26T01:00:00Z',
              dry_run: false,
              error_code: null,
              lock_digest: `sha256:${'1'.repeat(64)}`,
              merge_request_iid: 12,
              request_id: 'req-build-3',
              requested_by: 5,
              selection: {},
              state: 'succeeded',
            },
            {
              id: 2,
              created_at: '2026-11-25T01:00:00Z',
              dry_run: true,
              error_code: 'FG-AGENT-0008',
              lock_digest: null,
              merge_request_iid: null,
              request_id: 'req-build-2',
              requested_by: 5,
              selection: {},
              state: 'failed',
            },
          ],
        }),
    });
    const table = await screen.findByRole('table', { name: 'これまでの組み立て' });
    expect(within(table).getByRole('link', { name: 'MR !12' })).toBeTruthy();
    expect(within(table).getByText('req-build-2')).toBeTruthy();
    expect(within(table).getByText('失敗')).toBeTruthy();
  });

  it('the settings page links to the agent screen', async () => {
    const server = mockServer(
      routes('developer', {
        [`GET ${API}/protected-refs`]: () => json(200, { rules: [] }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'ja' });
    const link = await screen.findByRole('link', { name: 'エージェントの設定を開く' });
    expect(link.getAttribute('href')).toBe(PATH);
  });
});

describe('agent builder: the update band (A12)', () => {
  it('names revoked and dropped parts in words and asks for the update merge request', async () => {
    const { server } = await open('developer', {
      [`GET ${B}/updates`]: () => json(200, UPDATES),
      [`POST ${B}/update`]: () =>
        json(202, { queued: true, job_id: 41, reason: null, merge_request_iid: null, parts: UPDATES.parts, preview: PREVIEW }),
    });
    const band = await screen.findByRole('region', { name: '取り消された部品があります' });
    const items = within(band)
      .getAllByRole('listitem')
      .map((li) => li.textContent);
    // Revoked parts first; unchanged parts are not listed; each line says
    // what happens in words.
    expect(items).toEqual([
      '取り消し済み fmt-hook 0.1.0 は取り消されました → 0.2.0',
      '取り消し済み old-rule 1.0.0 は取り消されました。置き換える版がないので外します',
      'lint-skill 1.0.0 を足します（必要になった部品）',
      'review-skill 1.1.0 → 1.2.0',
    ]);
    expect(within(band).getByText('3 件のファイルが変わります。')).toBeTruthy();
    fireEvent.click(within(band).getByRole('button', { name: '更新の MR を作る' }));
    await waitFor(() => {
      expect(server.to('POST', `${B}/update`)).toHaveLength(1);
    });
    const asked = server.to('POST', `${B}/update`)[0];
    expect(asked?.body).toEqual({});
    expect(asked?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    expect(
      await within(band).findByText('更新の MR を頼みました。できたら通知が届きます（MR は Maintainer の承認を待ちます）。'),
    ).toBeTruthy();
    expect(within(band).queryByRole('button', { name: '更新の MR を作る' })).toBeNull();
    expect(await violations(document.body)).toEqual([]);
  });

  it('shows no update button when the API says the caller may not', async () => {
    await open('developer', { [`GET ${B}/updates`]: () => json(200, { ...UPDATES, can_update: false }) });
    const band = await screen.findByRole('region', { name: '取り消された部品があります' });
    expect(within(band).queryByRole('button', { name: '更新の MR を作る' })).toBeNull();
    expect(
      within(band).getByText('今は更新の MR を頼めません。下で部品を選び、Dry-Run から MR を作れます。'),
    ).toBeTruthy();
  });

  it('a newer version only is "Updates available"; an open update links to its merge request', async () => {
    await open('developer', {
      [`GET ${B}/updates`]: () =>
        json(200, { ...UPDATES, parts: UPDATES.parts.filter((p) => !p.locked_revoked && p.locked_version !== null) }),
      [`POST ${B}/update`]: () =>
        json(202, { queued: false, job_id: null, reason: 'already_open', merge_request_iid: 15, parts: [], preview: PREVIEW }),
    });
    const band = await screen.findByRole('region', { name: '更新があります' });
    expect(within(band).queryByText('取り消し済み')).toBeNull();
    fireEvent.click(within(band).getByRole('button', { name: '更新の MR を作る' }));
    const link = await within(band).findByRole('link', { name: 'MR !15 を開く' });
    expect(link.getAttribute('href')).toBe('/acme/forge-web/-/pull/15');
    expect(within(band).getByText('同じ更新の MR !15 がもう開いています。')).toBeTruthy();
  });

  it('a refused update shows the server message and the request id', async () => {
    await open('developer', {
      [`GET ${B}/updates`]: () => json(200, UPDATES),
      [`POST ${B}/update`]: () => apiError(409, '人が閉じた更新の MR は開き直しません。', 'req-upd-1', 'FG-AGENT-0016'),
    });
    const band = await screen.findByRole('region', { name: '取り消された部品があります' });
    fireEvent.click(within(band).getByRole('button', { name: '更新の MR を作る' }));
    expect(await within(band).findByText('人が閉じた更新の MR は開き直しません。')).toBeTruthy();
    expect(within(band).getByText('req-upd-1')).toBeTruthy();
  });

  it('a failed updates read shows its message and request id', async () => {
    await open('developer', {
      [`GET ${B}/updates`]: () => apiError(500, '更新を確かめられませんでした。', 'req-upds-1', 'FG-SYS-0001'),
    });
    expect(await screen.findByText('更新を確かめられませんでした。')).toBeTruthy();
    expect(screen.getByText('req-upds-1')).toBeTruthy();
  });

  it('up to date: one quiet line, no band', async () => {
    await open('developer');
    expect(await screen.findByText('部品はすべて最新です。')).toBeTruthy();
    expect(screen.queryByRole('region', { name: '更新があります' })).toBeNull();
  });
});

describe("agent builder: the AI's reasons (A11 on screen)", () => {
  const WITH_LLM = {
    ...RECOMMEND,
    source: 'rules+llm',
    llm: { state: 'ready', dropped: 0, model: 'synthetic:1b', summary: '<b>仕様書</b>はレビューと整形を求めています。' },
    parts: [
      RECOMMEND.parts[0],
      { ...RECOMMEND.parts[1], llm_reason: '仕様書がレビューの手順を求めています。' },
      RECOMMEND.parts[2],
    ],
    suggested: [
      {
        slug: 'kanflow-mcp',
        kind: 'mcp',
        version: '2.0.0',
        selected_by: 'llm',
        reason: 'カンバンの課題を読むため。',
        capabilities: ['network'],
      },
    ],
  };

  it('labels the reasons in words, never pre-selects a suggestion, and shows its marks', async () => {
    await open('developer', { [`POST ${B}/recommend`]: () => json(200, WITH_LLM) });
    const review = await screen.findByRole<HTMLInputElement>('checkbox', { name: /review-skill/ });
    const reviewRow = review.closest('li') as HTMLElement;
    expect(within(reviewRow).getByText('AI の助言（提案）')).toBeTruthy();
    expect(within(reviewRow).getByText('仕様書がレビューの手順を求めています。')).toBeTruthy();
    // The suggestion: unchecked, labelled, with its capability in words.
    const mcp = screen.getByRole<HTMLInputElement>('checkbox', { name: /kanflow-mcp/ });
    expect(mcp.checked).toBe(false);
    const mcpRow = mcp.closest('li') as HTMLElement;
    expect(within(mcpRow).getByText('AI の提案（まだ選んでいません）')).toBeTruthy();
    expect(within(mcpRow).getByText('カンバンの課題を読むため。')).toBeTruthy();
    expect(within(mcpRow).getByText('ネットワークを使う')).toBeTruthy();
    // The summary is plain text: markup stays characters.
    const note = screen.getByRole('region', { name: 'AI の助言（提案）' });
    expect(note.textContent).toContain('<b>仕様書</b>はレビューと整形を求めています。');
    expect(note.querySelector('b')).toBeNull();
    expect(await violations(document.body)).toEqual([]);
  });

  it('a Reporter reads the reasons and the suggestions too', async () => {
    await open('reporter', { [`POST ${B}/recommend`]: () => json(200, WITH_LLM) });
    expect(await screen.findByText('仕様書がレビューの手順を求めています。')).toBeTruthy();
    expect(screen.getByText('カンバンの課題を読むため。')).toBeTruthy();
    expect(screen.queryByRole('checkbox')).toBeNull();
  });

  it('no reasons now: the state in words, and "read again" asks again', async () => {
    const { server } = await open('developer', {
      [`POST ${B}/recommend`]: () =>
        json(200, { ...RECOMMEND, degraded: true, llm: { state: 'pending', dropped: 0, error_code: null } }),
    });
    const note = await screen.findByRole('region', { name: 'AI の助言（提案）' });
    expect(note.textContent).toContain('AI の理由は今ありません');
    expect(note.textContent).toContain('AI が理由を考えています');
    expect(note.textContent).toContain('おすすめは規則だけで選んでいます。');
    expect(server.to('POST', `${B}/recommend`)).toHaveLength(1);
    fireEvent.click(within(note).getByRole('button', { name: 'もう一度読む' }));
    await waitFor(() => {
      expect(server.to('POST', `${B}/recommend`)).toHaveLength(2);
    });
  });

  it.each([
    ['unavailable', 'AI が答えませんでした', true],
    ['paused', 'AI は一時停止中です', false],
    ['refused', '開いた仕様書がないため、AI には何も送っていません', false],
    ['withheld', 'この助言は見せられません', false],
  ])('state %s is said in words', async (state, words, again) => {
    await open('developer', {
      [`POST ${B}/recommend`]: () =>
        json(200, { ...RECOMMEND, degraded: true, llm: { state, dropped: 0, error_code: 'FG-AGENT-0011' } }),
    });
    const note = await screen.findByRole('region', { name: 'AI の助言（提案）' });
    expect(note.textContent).toContain('AI の理由は今ありません');
    expect(note.textContent).toContain(words);
    expect(note.textContent).toContain('このまま部品を選べます。');
    // Read again where the answer may change by itself (the LLM may come back).
    expect(within(note).queryByRole('button', { name: 'もう一度読む' }) !== null).toBe(again);
  });

  it('ready but degraded says the advice is partial', async () => {
    await open('developer', {
      [`POST ${B}/recommend`]: () => json(200, { ...WITH_LLM, degraded: true, llm: { state: 'ready', dropped: 2 } }),
    });
    const note = await screen.findByRole('region', { name: 'AI の助言（提案）' });
    expect(note.textContent).toContain('AI の助言の一部は使えませんでした');
  });

  it('a picked suggestion is sent as the person\'s choice', async () => {
    const { server } = await open('developer', {
      [`POST ${B}/recommend`]: () => json(200, WITH_LLM),
      [`POST ${B}/preview`]: () => json(200, PREVIEW),
    });
    fireEvent.click(await screen.findByRole('checkbox', { name: /kanflow-mcp/ }));
    fireEvent.click(screen.getByRole('button', { name: 'Dry-Run で差分を見る' }));
    await waitFor(() => {
      expect(server.to('POST', `${B}/preview`)).toHaveLength(1);
    });
    const sent = server.to('POST', `${B}/preview`)[0]?.body as { selection: { slug: string; selected_by: string }[] };
    expect(sent.selection.find((s) => s.slug === 'kanflow-mcp')?.selected_by).toBe('user');
  });
});
