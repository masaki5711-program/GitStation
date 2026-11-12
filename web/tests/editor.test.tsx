// The simple editor (E2): entry points on the file and folder screens, the
// two panes and the narrow tabs, Ctrl+S, the save dialog (new branch + MR by
// default, direct commit, protected branch), the refusals the editor words
// itself (conflict, secret, rate limit), the unsaved-changes guard, the
// result notice, and axe on the editor with its dialog open.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { PROJECT, apiError, mockServer, type Handler } from './mock';
import { C1, README, repoRoutes } from './repo-fixtures';
import { json, renderApp } from './render';
import { branchNameFor, countText, directCommitBlock, patternMatches, slugOf } from '../src/features/editorModel';

const BASE = '/acme/forge-web/-';
const COMMIT_PATH = '/api/v1/projects/7/repository/commits/single-file';
const RULES_PATH = '/api/v1/projects/7/protected-refs';

const committed = (over: Record<string, unknown> = {}) =>
  json(201, {
    commit_sha: 'c'.repeat(40),
    parent_sha: C1,
    branch: 'edit/readme-20261112-101500',
    branch_created: true,
    path: 'README.md',
    blob_sha: 'd'.repeat(40),
    merge_request: { iid: 12 },
    merge_request_error: null,
    ...over,
  });

function routes(over: Record<string, Handler> = {}) {
  return repoRoutes({
    [`GET ${RULES_PATH}`]: () => json(200, { rules: [] }),
    [`POST ${COMMIT_PATH}`]: () => committed(),
    ...over,
  });
}

async function openEditor(over: Record<string, Handler> = {}, path = `${BASE}/edit/main/README.md`) {
  const server = mockServer(routes(over));
  await renderApp({ fetch: server.fetch, path });
  const text = await screen.findByRole<HTMLTextAreaElement>('textbox', { name: 'README.md の内容' });
  return { server, text };
}

function location() {
  return screen.getByTestId('location').textContent;
}

function pressCtrlS() {
  fireEvent.keyDown(window, { key: 's', ctrlKey: true });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('editor model', () => {
  it('counts lines, characters and bytes as the server does', () => {
    expect(countText('')).toEqual({ lines: 0, chars: 0, bytes: 0 });
    expect(countText('a\nb\n')).toEqual({ lines: 2, chars: 4, bytes: 4 });
    expect(countText('仕様\n書')).toEqual({ lines: 2, chars: 4, bytes: 10 });
    expect(countText('😀').chars).toBe(1);
  });

  it('generates a readable branch name', () => {
    expect(slugOf('docs/Press Line.md')).toBe('press-line');
    expect(slugOf('docs/仕様書.md')).toBe('file');
    expect(branchNameFor('README.md', new Date(2026, 10, 12, 9, 5, 7))).toBe('edit/readme-20261112-090507');
  });

  it('reads protected-branch rules like the server (case, one star)', () => {
    expect(patternMatches('refs/heads/Main', 'refs/heads/main')).toBe(true);
    expect(patternMatches('refs/heads/release/*', 'refs/heads/release/1.0')).toBe(true);
    expect(patternMatches('refs/heads/release/*', 'refs/heads/main')).toBe(false);
    const rule = (over: Record<string, unknown>) => ({
      id: 1,
      pattern: 'refs/heads/main',
      push_min_role: 'developer',
      merge_rule: null,
      created_at: '',
      updated_at: '',
      unreportable_checks: [],
      warnings: [],
      ...over,
    });
    expect(directCommitBlock([rule({})], 'main', 'developer', false)).toBeNull();
    expect(directCommitBlock([rule({ push_min_role: 'maintainer' })], 'main', 'developer', false)).toBe('role');
    expect(directCommitBlock([rule({ push_min_role: 'maintainer' })], 'main', 'developer', true)).toBeNull();
    expect(
      directCommitBlock(
        [rule({ merge_rule: { direct_push_forbidden: true, required_approvals: 0, required_checks: [], require_resolved_discussions: false, require_work_item_link: false, updated_at: '' } })],
        'main',
        'owner',
        true,
      ),
    ).toBe('forbidden');
    expect(directCommitBlock(undefined, 'main', 'developer', false)).toBeNull();
  });
});

describe('entry points', () => {
  it('the file screen offers 編集 for a text file to a Developer', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/README.md` });
    const edit = await screen.findByRole('link', { name: '編集' });
    expect(edit.getAttribute('href')).toBe(`${BASE}/edit/main/README.md`);
  });

  it('no 編集 for a binary file, a tag, or a Reporter', async () => {
    const server = mockServer(routes());
    const first = await renderApp({ fetch: server.fetch, path: `${BASE}/blob/main/data.bin` });
    await screen.findByRole('heading', { level: 1, name: 'data.bin' });
    expect(screen.queryByRole('link', { name: '編集' })).toBeNull();
    first.unmount();

    const reporter = mockServer(routes({ 'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role: 'reporter' }) }));
    const second = await renderApp({ fetch: reporter.fetch, path: `${BASE}/blob/main/README.md` });
    await screen.findByRole('heading', { level: 1, name: 'README.md' });
    expect(screen.queryByRole('link', { name: '編集' })).toBeNull();
    expect(screen.queryByRole('link', { name: '新しいファイル' })).toBeNull();
    second.unmount();

    const commit = mockServer(routes());
    await renderApp({ fetch: commit.fetch, path: `${BASE}/blob/${C1}/README.md` });
    await screen.findByRole('heading', { level: 1, name: 'README.md' });
    expect(screen.queryByRole('link', { name: '編集' })).toBeNull();
  });

  it('the folder screen offers 新しいファイル in the folder shown', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/tree/main/src` });
    const link = await screen.findByRole('link', { name: '新しいファイル' });
    expect(link.getAttribute('href')).toBe(`${BASE}/new/main/src`);
  });
});

describe('the editor', () => {
  it('shows the file, its status, the counts and a preview that follows the text', async () => {
    const { text } = await openEditor();
    expect(screen.getByRole('heading', { level: 1, name: 'README.md' })).toBeTruthy();
    expect(text.value).toBe(README);
    expect(screen.getByText('保存済み')).toBeTruthy();
    expect(screen.getByText(`${countText(README).lines} 行 · ${countText(README).chars} 文字`)).toBeTruthy();
    const preview = screen.getByTestId('editor-preview');
    expect(await within(preview).findByRole('heading', { name: 'Press Line Spec' })).toBeTruthy();
    expect(screen.getByRole('heading', { level: 2, name: '編集' })).toBeTruthy();
    expect(screen.getByRole('heading', { level: 2, name: 'プレビュー' })).toBeTruthy();

    fireEvent.change(text, { target: { value: '# Changed title\n\nbody' } });
    expect(screen.getByText('未保存の変更あり')).toBeTruthy();
    expect(await within(preview).findByRole('heading', { name: 'Changed title' })).toBeTruthy();
    expect(screen.getByText('3 行 · 21 文字')).toBeTruthy();
  });

  it('reads the file at the resolved commit (the start_sha)', async () => {
    const { server } = await openEditor();
    const read = server.to('GET', '/api/v1/projects/7/repository/blob')[0];
    expect(read?.query.get('ref')).toBe(C1);
  });

  it('Ctrl+S opens the save dialog with a default message; Escape-like cancel returns the focus', async () => {
    const { text } = await openEditor();
    fireEvent.change(text, { target: { value: 'x' } });
    pressCtrlS();
    const dialog = await screen.findByRole('dialog', { name: '変更を保存' });
    const message = within(dialog).getByRole<HTMLInputElement>('textbox', { name: 'コミットメッセージ' });
    expect(message.value).toBe('README.md を更新');
    expect(document.activeElement).toBe(message);
    expect(within(dialog).getByRole<HTMLInputElement>('radio', { name: '新しいブランチを作って変更を申請（MR）' }).checked).toBe(true);
    // The Modal's cancel event (Esc in a browser) closes it; here: Cancel.
    fireEvent.click(within(dialog).getByRole('button', { name: 'キャンセル' }));
    await waitFor(() => {
      expect(screen.queryByRole('dialog')).toBeNull();
    });
    expect(document.activeElement).toBe(screen.getByRole('button', { name: '変更を保存' }));
  });

  it('by default commits on a new branch with a merge request and opens it', async () => {
    const { server, text } = await openEditor();
    fireEvent.change(text, { target: { value: '# New\n' } });
    fireEvent.click(screen.getByRole('button', { name: '変更を保存' }));
    const dialog = await screen.findByRole('dialog', { name: '変更を保存' });
    fireEvent.click(within(dialog).getByRole('button', { name: '説明を追加' }));
    fireEvent.change(within(dialog).getByRole('textbox', { name: '説明（任意）' }), {
      target: { value: 'Why: synthetic' },
    });
    fireEvent.click(within(dialog).getByRole('button', { name: '保存して MR を作成' }));
    await waitFor(() => {
      expect(location()).toBe(`${BASE}/pull/12`);
    });
    const body = server.to('POST', COMMIT_PATH)[0]?.body as Record<string, unknown>;
    expect(body).toMatchObject({
      branch: 'refs/heads/main',
      start_sha: C1,
      path: 'README.md',
      content: '# New\n',
      message: 'README.md を更新\n\nWhy: synthetic',
      open_merge_request: true,
    });
    expect(body.create_branch).toMatch(/^edit\/readme-\d{8}-\d{6}$/);
    expect(server.to('POST', COMMIT_PATH)[0]?.headers.get('X-GitStation-CSRF')).toBe('c'.repeat(64));
    expect((await screen.findByTestId('file-saved')).textContent).toContain('マージリクエストを作成しました');
  });

  it('commits directly to the branch when chosen and shows the file', async () => {
    const { server, text } = await openEditor({
      [`POST ${COMMIT_PATH}`]: () =>
        committed({ branch: 'main', branch_created: false, merge_request: null }),
    });
    fireEvent.change(text, { target: { value: 'direct\n' } });
    pressCtrlS();
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('radio', { name: 'このブランチ（main）に直接コミット' }));
    expect(within(dialog).queryByRole('textbox', { name: '新しいブランチの名前' })).toBeNull();
    fireEvent.click(within(dialog).getByRole('button', { name: 'コミットする' }));
    await waitFor(() => {
      expect(location()).toBe(`${BASE}/blob/main/README.md`);
    });
    const body = server.to('POST', COMMIT_PATH)[0]?.body as Record<string, unknown>;
    expect(body).not.toHaveProperty('create_branch');
    expect(body).not.toHaveProperty('open_merge_request');
    expect((await screen.findByTestId('file-saved')).textContent).toBe('main に変更をコミットしました。');
  });

  it('a protected branch with direct pushes banned offers only the new branch, with the reason', async () => {
    const { text } = await openEditor({
      [`GET ${RULES_PATH}`]: () =>
        json(200, {
          rules: [
            {
              id: 1,
              pattern: 'refs/heads/main',
              push_min_role: 'developer',
              merge_rule: {
                direct_push_forbidden: true,
                required_approvals: 1,
                required_checks: [],
                require_resolved_discussions: false,
                require_work_item_link: false,
                updated_at: '2026-11-01T00:00:00Z',
              },
              created_at: '2026-11-01T00:00:00Z',
              updated_at: '2026-11-01T00:00:00Z',
              unreportable_checks: [],
              warnings: [],
            },
          ],
        }),
    });
    fireEvent.change(text, { target: { value: 'x' } });
    pressCtrlS();
    const dialog = await screen.findByRole('dialog');
    const direct = await within(dialog).findByRole<HTMLInputElement>('radio', { name: 'このブランチ（main）に直接コミット' });
    await waitFor(() => {
      expect(direct.disabled).toBe(true);
    });
    expect(within(dialog).getByText('このブランチは保護されていて、直接コミットできません。')).toBeTruthy();
  });

  it('a conflict keeps the text and offers the copy and the latest version', async () => {
    const { text } = await openEditor({
      [`POST ${COMMIT_PATH}`]: () =>
        apiError(409, '編集を始めた後に、ほかの人がこのファイルかブランチを変更しました。', 'req-conflict', 'FG-GIT-0027'),
    });
    fireEvent.change(text, { target: { value: 'mine\n' } });
    pressCtrlS();
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: '保存して MR を作成' }));
    const conflict = await within(dialog).findByTestId('editor-conflict');
    expect(within(conflict).getByRole('heading', { name: 'ほかの人の変更と重なりました' })).toBeTruthy();
    expect(within(conflict).getByTestId('request-id').textContent).toBe('req-conflict');
    expect(within(conflict).getByRole('button', { name: '自分の編集をコピー' })).toBeTruthy();
    const latest = within(conflict).getByRole('link', { name: '最新の内容を開く（新しいタブ）' });
    expect(latest.getAttribute('href')).toBe(`${BASE}/blob/main/README.md`);
    expect(latest.getAttribute('target')).toBe('_blank');
    expect(text.value).toBe('mine\n');
    expect(location()).toBe(`${BASE}/edit/main/README.md`);
  });

  it('a secret is refused with what to do about it', async () => {
    const { text } = await openEditor({
      [`POST ${COMMIT_PATH}`]: () => apiError(422, '秘密情報らしい内容が含まれていました。', 'req-secret', 'FG-GIT-0022'),
    });
    fireEvent.change(text, { target: { value: 'token\n' } });
    pressCtrlS();
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: '保存して MR を作成' }));
    const secret = await within(dialog).findByTestId('editor-secret');
    expect(within(secret).getByRole('heading', { name: '秘密情報らしい内容があるため保存できません' })).toBeTruthy();
    expect(within(secret).getByText(/鍵・トークン・パスワードなどを本文から取り除いて/)).toBeTruthy();
    expect(within(secret).getByTestId('request-id').textContent).toBe('req-secret');
  });

  it('the rate limit says to wait a minute', async () => {
    const { text } = await openEditor({
      [`POST ${COMMIT_PATH}`]: () => apiError(429, '短い間に要求が多すぎます。', 'req-rate', 'FG-WEB-0010'),
    });
    fireEvent.change(text, { target: { value: 'x' } });
    pressCtrlS();
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: '保存して MR を作成' }));
    const rate = await within(dialog).findByTestId('editor-rate-limited');
    expect(within(rate).getByText(/1 分ほど待ってから/)).toBeTruthy();
  });

  it('any other refusal shows the server message and the request id', async () => {
    const { text } = await openEditor({
      [`POST ${COMMIT_PATH}`]: () => apiError(422, 'ファイルの内容が変わっていないため、コミットしませんでした。', 'req-same', 'FG-GIT-0031'),
    });
    fireEvent.change(text, { target: { value: 'x' } });
    pressCtrlS();
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: '保存して MR を作成' }));
    const alert = await within(dialog).findByRole('alert');
    expect(alert.textContent).toContain('ファイルの内容が変わっていないため');
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-same');
  });

  it('a blank message is refused before sending', async () => {
    const { server, text } = await openEditor();
    fireEvent.change(text, { target: { value: 'x' } });
    pressCtrlS();
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByRole('textbox', { name: 'コミットメッセージ' }), { target: { value: '  ' } });
    fireEvent.click(within(dialog).getByRole('button', { name: '保存して MR を作成' }));
    expect(await within(dialog).findByText('コミットメッセージを入力してください。')).toBeTruthy();
    expect(server.to('POST', COMMIT_PATH)).toEqual([]);
  });

  it('unsaved changes: leaving by a link asks first; reload is warned', async () => {
    const { text } = await openEditor();
    // Nothing changed: no warning.
    const quiet = new Event('beforeunload', { cancelable: true });
    window.dispatchEvent(quiet);
    expect(quiet.defaultPrevented).toBe(false);

    fireEvent.change(text, { target: { value: 'unsaved' } });
    const unload = new Event('beforeunload', { cancelable: true });
    window.dispatchEvent(unload);
    expect(unload.defaultPrevented).toBe(true);

    const crumbs = screen.getByRole('navigation', { name: 'ファイルの場所' });
    fireEvent.click(within(crumbs).getByRole('link', { name: 'main' }));
    const ask = await screen.findByRole('dialog', { name: '保存していない変更があります' });
    expect(location()).toBe(`${BASE}/edit/main/README.md`);
    fireEvent.click(within(ask).getByRole('button', { name: 'キャンセル' }));
    await waitFor(() => {
      expect(screen.queryByRole('dialog', { name: '保存していない変更があります' })).toBeNull();
    });
    expect(text.value).toBe('unsaved');

    fireEvent.click(within(crumbs).getByRole('link', { name: 'main' }));
    fireEvent.click(await screen.findByRole('button', { name: '変更を捨てて移動' }));
    await waitFor(() => {
      expect(location()).toBe(`${BASE}/tree/main`);
    });
  });

  it('below 48rem the panes are tabs', async () => {
    vi.stubGlobal('matchMedia', (query: string) => ({
      matches: query === '(max-width: 47.99rem)',
      media: query,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
    }));
    await openEditor();
    const tabs = screen.getAllByRole('tab');
    expect(tabs.map((t) => t.textContent)).toEqual(['編集', 'プレビュー']);
    expect(tabs[0]?.getAttribute('aria-selected')).toBe('true');
    expect(screen.getByTestId('editor-preview').hidden).toBe(true);
    fireEvent.click(tabs[1] as HTMLElement);
    expect(screen.getByTestId('editor-preview').hidden).toBe(false);
    expect(screen.getByRole('tabpanel', { name: 'プレビュー' })).toBeTruthy();
  });

  it('a new file: the path is required, then sent with an "add" message', async () => {
    const server = mockServer(
      routes({ [`POST ${COMMIT_PATH}`]: () => committed({ path: 'src/notes.md' }) }),
    );
    await renderApp({ fetch: server.fetch, path: `${BASE}/new/main/src` });
    expect(await screen.findByRole('heading', { level: 1, name: '新しいファイル' })).toBeTruthy();
    const path = screen.getByRole<HTMLInputElement>('textbox', { name: 'ファイルのパス' });
    expect(path.value).toBe('src/');
    fireEvent.click(screen.getByRole('button', { name: '変更を保存' }));
    expect(await screen.findByText('パスの最後にファイル名を入力してください（例: docs/spec.md）。')).toBeTruthy();
    expect(document.activeElement).toBe(path);
    expect(screen.queryByRole('dialog')).toBeNull();

    fireEvent.change(path, { target: { value: 'src/notes.md' } });
    fireEvent.change(screen.getByRole('textbox', { name: 'src/notes.md の内容' }), {
      target: { value: '# Notes\n' },
    });
    pressCtrlS();
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByRole<HTMLInputElement>('textbox', { name: 'コミットメッセージ' }).value).toBe(
      'src/notes.md を追加',
    );
    fireEvent.click(within(dialog).getByRole('button', { name: '保存して MR を作成' }));
    await waitFor(() => {
      expect(location()).toBe(`${BASE}/pull/12`);
    });
    expect(server.to('POST', COMMIT_PATH)[0]?.body).toMatchObject({ path: 'src/notes.md', content: '# Notes\n' });
  });

  it('a tag cannot be edited: says so', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/edit/${C1}/README.md` });
    expect(await screen.findByRole('heading', { level: 1, name: 'ブランチでだけ編集できます' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'ファイルに戻る' })).toBeTruthy();
  });

  it('a binary file cannot be edited: says so', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: `${BASE}/edit/main/data.bin` });
    expect(await screen.findByRole('heading', { level: 1, name: 'このファイルはエディターで編集できません' })).toBeTruthy();
  });

  it('axe: the editor and its dialog have no violations', async () => {
    const { text } = await openEditor();
    const preview = screen.getByTestId('editor-preview');
    await within(preview).findByRole('heading', { name: 'Press Line Spec' });
    const run = async () => {
      const result = await axe.run(document.body, {
        runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
        rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
      });
      return result.violations.map((v) => `${v.id}: ${v.nodes.map((n) => n.target.join(' ')).join(', ')}`);
    };
    expect(await run()).toEqual([]);
    fireEvent.change(text, { target: { value: 'x' } });
    pressCtrlS();
    await screen.findByRole('dialog');
    expect(await run()).toEqual([]);
  });
});
