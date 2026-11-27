// Plan 16 A8: project settings → "Agent" (/{project}/-/settings/agent):
// the adapter, the recommended parts as checkboxes, the Dry-Run's diff,
// "Create the merge request" and the update band. The named test: nobody
// is shown a button the server would refuse — a Reporter reads the
// recommendation only; the Dry-Run and the merge request are a
// Developer's. axe checks the screen.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { summary } from './marketplace-fixtures';
import { apiError, mockServer, PROJECT, REFS_FILLED, RESOLVE_PROJECT, SESSION, signedIn, type Handler } from './mock';
import { json, renderApp } from './render';

const API = '/api/v1/projects/7';
const B = `${API}/agent-builder`;
const PATH = '/acme/forge-web/-/settings/agent';

const RECOMMEND = {
  source: 'rules',
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

const LOCK = JSON.stringify({
  schema_version: 1,
  parts: [
    { slug: 'claude-code-v2', version: '2.1.0' },
    { slug: 'review-skill', version: '1.1.0' },
  ],
});

const blob = (content: string) =>
  json(200, {
    binary: false,
    content,
    mode: '100644',
    name: 'agent.lock.json',
    oid: 'b'.repeat(40),
    path: '.gitstation/agent.lock.json',
    ref: { commit: 'a'.repeat(40), full_name: 'refs/heads/main', name: 'main', type: 'branch' },
    size: content.length,
    truncated: false,
    type: 'blob',
  });

function routes(role: string, over: Record<string, Handler> = {}): Record<string, Handler> {
  return {
    ...signedIn(),
    'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_PROJECT),
    [`GET ${API}`]: () => json(200, { ...PROJECT, role }),
    [`GET ${API}/repository/refs`]: () => json(200, REFS_FILLED),
    [`GET ${API}/repository/blob`]: () => blob(LOCK),
    [`GET ${B}`]: () => json(200, { builds: [] }),
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
    // The builds are a Developer's read: not asked.
    expect(server.to('GET', B)).toHaveLength(0);
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
    // The update band: one part has a newer stable version than the lock.
    const band = await screen.findByRole('region', { name: '更新があります' });
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
      [`GET ${API}/repository/blob`]: () => apiError(404, '見つかりません。', 'req-blob', 'FG-REPO-0005'),
    });
    expect(await screen.findByText('基本のモデルに stable の版がありません。')).toBeTruthy();
    expect(screen.getByText('req-rec-1')).toBeTruthy();
    expect(screen.queryByRole('region', { name: '更新があります' })).toBeNull();
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
