// Plan 20 N2: /-/notes — "Notes" in My work, the quick note (one commit of
// N-YYYYMMDD-HHMMSS.md on main, the same file `gstn note new` writes), my
// notes newest first, and the search over them. The journey the plan names
// (`quick_note_saved_and_found`) runs through the real client against the
// mock route table, as every page test here does; axe checks the screen.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { defaultTitle, noteId, noteIdOfPath, quickNote, quoted } from '../src/features/noteText';
import { noteTime } from '../src/pages/notes/NotesScreen';
import { apiError, mockServer, signedIn, type Call, type Handler } from './mock';
import { json, renderApp } from './render';

const NOTES = { project_id: 42, path: 'taro/notes', owner: 'taro', default_branch: 'main' };
const HEAD = 'a'.repeat(40);
const COMMIT = 'c'.repeat(40);
// 2026-11-26T01:15:00Z
const NOW = new Date(Date.UTC(2026, 10, 26, 1, 15, 0));
const ID = 'N-20261126-011500';

const doc = (id: string, title: string | null, classification = 'internal') => ({
  id,
  path: `${id}.md`,
  doctype: 'note',
  title,
  classification,
  translations: [],
});

const docPage = (documents: unknown[], next: string | null = null) =>
  json(200, {
    documents,
    next_after: next,
    incomplete: null,
    withheld: null,
    ref: { commit: HEAD, full_name: 'refs/heads/main', name: 'main', type: 'branch' },
  });

const live = (over: Record<string, Handler> = {}): Record<string, Handler> => ({
  ...signedIn(),
  'GET /api/v1/user/notes': () => json(200, NOTES),
  'GET /api/v1/projects/42/documents': () => docPage([]),
  ...over,
});

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`);
}

const of = (calls: Call[], method: string, path: string) =>
  calls.filter((c) => c.method === method && c.path === path);

describe('noteText: the file gstn note new writes', () => {
  it('ids are UTC seconds', () => {
    expect(noteId(new Date(0))).toBe('N-19700101-000000');
    expect(noteId(NOW)).toBe(ID);
    expect(noteId(new Date(Date.UTC(2000, 1, 29, 23, 59, 59)))).toBe('N-20000229-235959');
    expect(noteId(new Date(Date.UTC(-100, 0, 1)))).toBeNull();
    expect(noteTime(ID)).toBe('2026-11-26T01:15:00Z');
    expect(noteIdOfPath(`${ID}.md`)).toBe(ID);
    expect(noteIdOfPath('README.md')).toBeNull();
    expect(noteIdOfPath(`dir/${ID}.md`)).toBeNull();
  });

  it('the note is forge-doc quick_note byte for byte; nothing typed changes the front matter', () => {
    expect(quickNote(ID, 'メモ', 'taro', '本文\n\n')).toBe(
      `---\ndoctype: note\nid: ${ID}\ntitle: "メモ"\nowner: "taro"\nclassification: internal\n---\n\n本文\n`,
    );
    expect(quoted('line\nowner: bob "q" \\')).toBe('"line\\u000aowner: bob \\"q\\" \\\\"');
    expect(quoted('nel\u0085x bom﻿x')).toBe('"nel\\u0085x bom\\ufeffx"');
    expect(defaultTitle('\n# 打ち合わせ\nbody', 'N-1')).toBe('打ち合わせ');
    expect(defaultTitle('  \n', 'N-1')).toBe('N-1');
    expect(Array.from(defaultTitle('あ'.repeat(300), 'N-1'))).toHaveLength(200);
  });
});

describe('notes: the screen', () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ['Date'] });
    vi.setSystemTime(NOW);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('quick_note_saved_and_found: a quick note is one commit on main, listed at once and found by the search', async () => {
    let saved = false;
    const server = mockServer(
      live({
        'GET /api/v1/projects/42/documents': () => docPage(saved ? [doc('N-20261120-080000', '古いメモ'), doc(ID, '打ち合わせ')] : []),
        'GET /api/v1/projects/42/repository/resolve': () =>
          json(200, { path: '', ref: { commit: HEAD, full_name: 'refs/heads/main', name: 'main', type: 'branch' } }),
        'GET /api/v1/projects/42/repository/blob': () => apiError(404, '見つかりません', 'req-404', 'FG-REPO-0005'),
        'POST /api/v1/projects/42/repository/commits/single-file': () => {
          saved = true;
          return json(201, {
            blob_sha: 'b'.repeat(40),
            branch: 'main',
            branch_created: false,
            commit_sha: COMMIT,
            parent_sha: HEAD,
            path: `${ID}.md`,
          });
        },
        'GET /api/v1/search': () =>
          json(200, {
            items: [
              {
                kind: 'document',
                project_id: 42,
                project_path: 'taro/notes',
                path: `${ID}.md`,
                iid: null,
                title: '打ち合わせ',
                snippet: { text: '設計書の打ち合わせ', highlights: [{ start: 0, end: 3 }], cut_before: false, cut_after: false },
                updated_at: '2026-11-26T01:15:00Z',
                truncated: false,
              },
            ],
            next_cursor: null,
            has_more: false,
          }),
      }),
    );
    const view = await renderApp({ fetch: server.fetch, path: '/' });

    // "Notes" is in My work.
    const nav = await screen.findByRole('navigation', { name: 'マイワーク' });
    const item = within(nav).getByRole('link', { name: 'ノート' });
    fireEvent.click(item);
    await screen.findByRole('heading', { level: 1, name: '自分のノート' });
    expect(screen.getByTestId('location').textContent).toBe('/-/notes');
    await screen.findByText('まだメモがありません。すばやいメモに書いて保存してください。');
    expect(await violations(view.container)).toEqual([]);

    // Write and save with Ctrl+Enter; the title comes from the first line.
    const body = screen.getByRole('textbox', { name: '本文' });
    fireEvent.change(body, { target: { value: '# 打ち合わせ\n設計書の打ち合わせ\n' } });
    fireEvent.keyDown(body, { key: 'Enter', ctrlKey: true });

    const status = await screen.findByText('「打ち合わせ」を保存しました。');
    expect(status.closest('[role="status"]')).not.toBeNull();
    expect(within(status.closest('p') as HTMLElement).getByRole('link', { name: '開く' }).getAttribute('href')).toBe(
      `/taro/notes/-/documents/${ID}`,
    );
    const [commit] = of(server.calls, 'POST', '/api/v1/projects/42/repository/commits/single-file');
    expect(commit?.body).toEqual({
      branch: 'main',
      start_sha: HEAD,
      path: `${ID}.md`,
      content: `---\ndoctype: note\nid: ${ID}\ntitle: "打ち合わせ"\nowner: "taro"\nclassification: internal\n---\n\n# 打ち合わせ\n設計書の打ち合わせ\n`,
      message: `Add note ${ID}`,
    });
    expect(of(server.calls, 'GET', '/api/v1/projects/42/repository/blob')[0]?.query.get('ref')).toBe(HEAD);
    expect((body as HTMLTextAreaElement).value).toBe('');

    // Listed at once, newest first.
    const list = await waitFor(() => {
      const links = screen.getAllByRole('link').filter((a) => a.getAttribute('href')?.startsWith('/taro/notes/-/documents/'));
      expect(links.length).toBeGreaterThanOrEqual(3);
      return links;
    });
    const listed = list.filter((a) => a.closest('ol') !== null).map((a) => a.textContent);
    expect(listed).toEqual(['打ち合わせ', '古いメモ']);
    expect(of(server.calls, 'GET', '/api/v1/projects/42/documents').at(-1)?.query.get('doctype')).toBe('note');

    // Found by the search, on the notes project's documents only.
    const box = screen.getByRole('searchbox', { name: 'ノートを検索' });
    fireEvent.change(box, { target: { value: '設計書' } });
    fireEvent.submit(box.closest('form') as HTMLFormElement);
    const hit = await screen.findByRole('heading', { level: 2, name: 'ノートの検索' });
    expect(hit).toBeTruthy();
    const [search] = of(server.calls, 'GET', '/api/v1/search');
    expect(search?.query.get('q')).toBe('設計書');
    expect(search?.query.get('scope')).toBe('document');
    expect(search?.query.get('project')).toBe('42');
    const found = await screen.findByText('1 件を表示しています');
    const section = found.closest('section') as HTMLElement;
    expect([...section.querySelectorAll('mark')].map((m) => m.textContent)).toEqual(['設計書']);
    expect(within(section).getByRole('link', { name: '打ち合わせ' }).getAttribute('href')).toBe(
      `/taro/notes/-/documents/${ID}`,
    );
    expect(await violations(view.container)).toEqual([]);
    expect(server.unmatched).toEqual([]);
  });

  it('an empty text is not saved; the field says why', async () => {
    const server = mockServer(live());
    await renderApp({ fetch: server.fetch, path: '/-/notes' });
    const save = await screen.findByRole('button', { name: '保存' });
    fireEvent.click(save);
    expect(await screen.findByText('本文を書いてください。')).toBeTruthy();
    expect(screen.getByRole('textbox', { name: '本文' }).getAttribute('aria-invalid')).toBe('true');
    expect(of(server.calls, 'POST', '/api/v1/projects/42/repository/commits/single-file')).toEqual([]);
  });

  it('a note of the same second is never replaced', async () => {
    const server = mockServer(
      live({
        'GET /api/v1/projects/42/repository/resolve': () =>
          json(200, { path: '', ref: { commit: HEAD, full_name: 'refs/heads/main', name: 'main', type: 'branch' } }),
        'GET /api/v1/projects/42/repository/blob': () => json(200, { path: `${ID}.md` }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/notes' });
    const body = await screen.findByRole('textbox', { name: '本文' });
    fireEvent.change(body, { target: { value: 'メモ' } });
    fireEvent.click(screen.getByRole('button', { name: '保存' }));
    expect(await screen.findByRole('alert')).toHaveProperty(
      'textContent',
      '同じ時刻のメモがすでにあります。もう一度「保存」を押してください。',
    );
    expect(of(server.calls, 'POST', '/api/v1/projects/42/repository/commits/single-file')).toEqual([]);
    expect((body as HTMLTextAreaElement).value).toBe('メモ');
  });

  it('a note of the same second written meanwhile (FG-GIT-0027) says to save again', async () => {
    const server = mockServer(
      live({
        'GET /api/v1/projects/42/repository/resolve': () =>
          json(200, { path: '', ref: { commit: HEAD, full_name: 'refs/heads/main', name: 'main', type: 'branch' } }),
        'GET /api/v1/projects/42/repository/blob': () => apiError(404, '見つかりません', 'req-404', 'FG-REPO-0005'),
        'POST /api/v1/projects/42/repository/commits/single-file': () =>
          apiError(409, '変更が重なりました', 'req-409', 'FG-GIT-0027'),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/notes' });
    fireEvent.change(await screen.findByRole('textbox', { name: '本文' }), { target: { value: 'メモ' } });
    fireEvent.click(screen.getByRole('button', { name: '保存' }));
    expect(await screen.findByText('同じ時刻のメモがすでにあります。もう一度「保存」を押してください。')).toBeTruthy();
  });

  it('a refused save shows the server message and request id, and keeps the text', async () => {
    const server = mockServer(
      live({
        'GET /api/v1/projects/42/repository/resolve': () =>
          json(200, { path: '', ref: { commit: HEAD, full_name: 'refs/heads/main', name: 'main', type: 'branch' } }),
        'GET /api/v1/projects/42/repository/blob': () => apiError(404, '見つかりません', 'req-404', 'FG-REPO-0005'),
        'POST /api/v1/projects/42/repository/commits/single-file': () =>
          apiError(422, '秘密の情報が含まれています', 'req-secret', 'FG-GIT-0022'),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/notes' });
    const body = await screen.findByRole('textbox', { name: '本文' });
    fireEvent.change(body, { target: { value: 'token=xyz' } });
    fireEvent.click(screen.getByRole('button', { name: '保存' }));
    expect(await screen.findByText('秘密の情報が含まれています')).toBeTruthy();
    expect(screen.getByText('req-secret')).toBeTruthy();
    expect(screen.queryByText('FG-GIT-0022')).toBeNull();
    expect((body as HTMLTextAreaElement).value).toBe('token=xyz');
  });

  it('no notes yet: one line and "start my notes" creates them', async () => {
    let created = false;
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/user/notes': () =>
        created ? json(200, NOTES) : apiError(404, '見つかりません', 'req-none', 'FG-REPO-0005'),
      'POST /api/v1/user/notes': () => {
        created = true;
        return json(201, { ...NOTES, commit: COMMIT, replaced_project_id: null });
      },
      'GET /api/v1/projects/42/documents': () => docPage([]),
    });
    const view = await renderApp({ fetch: server.fetch, path: '/-/notes' });
    expect(await screen.findByText('ノートはまだありません。')).toBeTruthy();
    expect(screen.queryByText('req-none')).toBeNull();
    expect(await violations(view.container)).toEqual([]);
    fireEvent.click(screen.getByRole('button', { name: 'ノートを始める' }));
    expect(await screen.findByRole('textbox', { name: '本文' })).toBeTruthy();
    expect(of(server.calls, 'POST', '/api/v1/user/notes')).toHaveLength(1);
    expect(screen.getByRole('link', { name: 'プロジェクトとして開く' }).getAttribute('href')).toBe('/taro/notes');
  });

  it('notes moved or deleted (FG-KNOW-0005): the request id and "create them again"', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/user/notes': () => apiError(404, 'ノートが見つかりません', 'req-moved', 'FG-KNOW-0005'),
      'POST /api/v1/user/notes': () => apiError(409, 'この名前はすでに使われています', 'req-taken', 'FG-KNOW-0006'),
    });
    await renderApp({ fetch: server.fetch, path: '/-/notes' });
    expect(await screen.findByText('ノートの置き場所が見つかりません（移動か削除されました）。')).toBeTruthy();
    expect(screen.getByText('問い合わせ番号: req-moved')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'ノートを作り直す' }));
    expect(await screen.findByText('この名前はすでに使われています')).toBeTruthy();
    expect(screen.getByText('req-taken')).toBeTruthy();
  });

  it('another failure reading the notes is the error notice with a retry', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/user/notes': () => apiError(500, 'いま読めません', 'req-500', 'FG-SYS-0001'),
    });
    await renderApp({ fetch: server.fetch, path: '/-/notes' });
    expect(await screen.findByText('いま読めません')).toBeTruthy();
    expect(screen.getByText('req-500')).toBeTruthy();
    expect(screen.getByRole('button', { name: /もう一度/ })).toBeTruthy();
  });

  it('nothing found: says the search lags a fresh note', async () => {
    const server = mockServer(
      live({
        'GET /api/v1/projects/42/documents': () => docPage([doc(ID, 'メモ', 'confidential')]),
        'GET /api/v1/search': () => json(200, { items: [], next_cursor: null, has_more: false }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/notes' });
    expect(await screen.findByText('機密')).toBeTruthy();
    const box = screen.getByRole('searchbox', { name: 'ノートを検索' });
    fireEvent.change(box, { target: { value: 'x' } });
    fireEvent.submit(box.closest('form') as HTMLFormElement);
    expect(await screen.findByText('2 文字以上入力してください。')).toBeTruthy();
    expect(of(server.calls, 'GET', '/api/v1/search')).toEqual([]);
    fireEvent.change(box, { target: { value: '会議室' } });
    fireEvent.submit(box.closest('form') as HTMLFormElement);
    expect(await screen.findByText('「会議室」に合うノートはありませんでした。')).toBeTruthy();
    expect(screen.getByText('保存したばかりのメモは、検索に出るまで 1 分ほどかかります。')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: '一覧に戻る' }));
    expect(await screen.findByRole('link', { name: 'メモ' })).toBeTruthy();
  });
});
