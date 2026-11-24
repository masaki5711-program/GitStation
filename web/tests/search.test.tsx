// Plan 20 K5: the search box in the header and /-/search. The E2E-style
// journeys the plan names (`search_japanese_title`, `no_results_next_step`)
// run through the real client against the mock route table, as every page
// test here does; axe checks the screen.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { snippetPieces } from '../src/api/search';
import { apiError, mockServer, signedIn, type Call, type Handler } from './mock';
import { json, renderApp } from './render';

const PATH = '/api/v1/search';

const hit = (over: Record<string, unknown> = {}) => ({
  kind: 'merge_request',
  project_id: 7,
  project_path: 'acme/forge-web',
  title: '設計書の改訂',
  iid: 12,
  snippet: { text: '装置の設計書を改訂する', highlights: [{ start: 3, end: 6 }], cut_before: false, cut_after: true },
  updated_at: '2026-11-02T03:04:00Z',
  truncated: false,
  ...over,
});

const pageOf = (items: unknown[], next: string | null = null) =>
  json(200, { items, next_cursor: next, has_more: next !== null });

const routes = (search: Handler): Record<string, Handler> => ({ ...signedIn(), [`GET ${PATH}`]: search });

const searches = (calls: Call[]) => calls.filter((c) => c.path === PATH);

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`);
}

describe('search: the header box', () => {
  it('search_japanese_title: a Japanese query from the header finds the title, its path and the highlighted snippet', async () => {
    const server = mockServer(routes(() => pageOf([hit()])));
    const view = await renderApp({ fetch: server.fetch });
    const box = await screen.findByRole('searchbox', { name: 'プロジェクトと MR を検索' });
    fireEvent.change(box, { target: { value: '  設計書 ' } });
    fireEvent.submit(box.closest('form') as HTMLFormElement);

    const link = await screen.findByRole('link', { name: '設計書の改訂' });
    expect(link.getAttribute('href')).toBe('/acme/forge-web/-/pull/12');
    expect(screen.getByTestId('location').textContent).toBe(`/-/search?q=${encodeURIComponent('設計書')}`);
    const [call] = searches(server.calls);
    expect(call?.query.get('q')).toBe('設計書');
    expect(call?.query.get('scope')).toBeNull();
    expect(call?.query.get('cursor')).toBeNull();

    const item = link.closest('li') as HTMLElement;
    expect(within(item).getByText('acme/forge-web!12')).toBeTruthy();
    expect(within(item).getByText('MR')).toBeTruthy();
    const marks = item.querySelectorAll('mark');
    expect([...marks].map((m) => m.textContent)).toEqual(['設計書']);
    expect(screen.getByText('1 件を表示しています').getAttribute('role')).toBe('status');
    expect(await violations(view.container)).toEqual([]);
    expect(server.unmatched).toEqual([]);
  });

  it('"/" moves to the box, but not while typing in a field', async () => {
    const server = mockServer(routes(() => pageOf([])));
    await renderApp({ fetch: server.fetch, path: '/-/search' });
    const box = await screen.findByRole('searchbox', { name: 'プロジェクトと MR を検索' });
    expect(box.getAttribute('aria-keyshortcuts')).toBe('/');
    fireEvent.keyDown(document.body, { key: '/' });
    expect(document.activeElement).toBe(box);

    const page = screen.getByRole('searchbox', { name: '探す言葉' });
    page.focus();
    const typed = fireEvent.keyDown(page, { key: '/' });
    expect(typed).toBe(true);
    expect(document.activeElement).toBe(page);
  });

  it('"/" leaves the focus alone while a modal is open', async () => {
    const server = mockServer(routes(() => pageOf([])));
    await renderApp({ fetch: server.fetch, path: '/-/search' });
    const box = await screen.findByRole('searchbox', { name: 'プロジェクトと MR を検索' });
    const modal = document.createElement('div');
    modal.setAttribute('aria-modal', 'true');
    document.body.append(modal);
    try {
      expect(fireEvent.keyDown(document.body, { key: '/' })).toBe(true);
      expect(document.activeElement).not.toBe(box);
    } finally {
      modal.remove();
    }
  });

  it('an empty header search opens the screen that says what to type', async () => {
    const server = mockServer(routes(() => pageOf([])));
    await renderApp({ fetch: server.fetch });
    const box = await screen.findByRole('searchbox', { name: 'プロジェクトと MR を検索' });
    fireEvent.submit(box.closest('form') as HTMLFormElement);
    expect(await screen.findByText('探したい言葉を入れてください。日本語でも探せます。')).toBeTruthy();
    expect(screen.getByTestId('location').textContent).toBe('/-/search');
    expect(searches(server.calls)).toEqual([]);
  });

  it('is not shown to someone signed out', async () => {
    const server = mockServer({ 'GET /api/v1/session': () => apiError(401, 'ログインしてください。', 'req-anon') });
    await renderApp({ fetch: server.fetch });
    await screen.findAllByRole('button');
    expect(screen.queryByRole('search')).toBeNull();
  });
});

describe('search: the results screen', () => {
  it('no_results_next_step: nothing found says why and offers the next step', async () => {
    const server = mockServer(routes(() => pageOf([])));
    await renderApp({ fetch: server.fetch, path: '/-/search?q=%E3%81%AA%E3%81%84%E8%A8%80%E8%91%89' });
    expect(await screen.findByText('「ない言葉」に当てはまる物は見つかりませんでした。')).toBeTruthy();
    expect(screen.getByText('短い言葉や別の言い方で試してください。見てよい物だけが出ます。')).toBeTruthy();
    expect(screen.getByRole('link', { name: 'プロジェクトの一覧を見る' }).getAttribute('href')).toBe('/-/projects');
  });

  it('no_results_next_step: with a kind filter the next step searches every kind', async () => {
    const server = mockServer(routes(() => pageOf([])));
    await renderApp({ fetch: server.fetch, path: '/-/search?q=abc&scope=project' });
    const all = await screen.findByRole('link', { name: 'すべての種類で探す' });
    expect(all.getAttribute('href')).toBe('/-/search?q=abc');
    expect(searches(server.calls)[0]?.query.get('scope')).toBe('project');
  });

  it('the kind filter is in the address and marks the current kind', async () => {
    const server = mockServer(routes(() => pageOf([hit({ kind: 'project', iid: null, title: 'forge-web' })])));
    await renderApp({ fetch: server.fetch, path: '/-/search?q=forge' });
    const nav = await screen.findByRole('navigation', { name: '種類で絞る' });
    expect(within(nav).getByRole('link', { name: 'すべて' }).getAttribute('aria-current')).toBe('page');
    fireEvent.click(within(nav).getByRole('link', { name: 'プロジェクト' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe('/-/search?q=forge&scope=project');
    });
    await waitFor(() => {
      expect(searches(server.calls).at(-1)?.query.get('scope')).toBe('project');
    });
    expect(within(nav).getByRole('link', { name: 'プロジェクト' }).getAttribute('aria-current')).toBe('page');
    // A project hit links to the project.
    expect((await screen.findByRole('link', { name: 'forge-web' })).getAttribute('href')).toBe('/acme/forge-web');
  });

  it('no query, or one character, asks for words without a request', async () => {
    const server = mockServer(routes(() => pageOf([])));
    await renderApp({ fetch: server.fetch, path: '/-/search' });
    expect(await screen.findByText('探したい言葉を入れてください。日本語でも探せます。')).toBeTruthy();
    const page = screen.getByRole('searchbox', { name: '探す言葉' });
    fireEvent.change(page, { target: { value: '京' } });
    fireEvent.submit(page.closest('form') as HTMLFormElement);
    expect(await screen.findByText('2 文字以上で探してください。')).toBeTruthy();
    expect(searches(server.calls)).toEqual([]);
  });

  it('a failure shows the server message and request id only, with a retry', async () => {
    let fail = true;
    const server = mockServer(
      routes(() =>
        fail ? apiError(503, 'いま検索できません。', 'req-search-1', 'FG-KNOW-0001') : pageOf([hit()]),
      ),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: '/-/search?q=abc' });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('いま検索できません。');
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-search-1');
    expect(container.textContent).not.toContain('FG-KNOW-0001');
    fail = false;
    fireEvent.click(within(alert).getByRole('button', { name: 'もう一度試す' }));
    expect(await screen.findByRole('link', { name: '設計書の改訂' })).toBeTruthy();
  });

  it('"more" sends the cursor and adds the next page; an empty page with more says so', async () => {
    const server = mockServer(
      routes((c) => {
        const cursor = c.query.get('cursor');
        if (cursor === null) return pageOf([], 'c1');
        if (cursor === 'c1') return pageOf([hit()], 'c2');
        return pageOf([hit({ iid: 13, title: '二件目' })]);
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/search?q=abc' });
    expect(await screen.findByText('ここまでに当てはまる物はありません。続きを探せます。')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'さらに探す' }));
    expect(await screen.findByRole('link', { name: '設計書の改訂' })).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'さらに探す' }));
    expect(await screen.findByRole('link', { name: '二件目' })).toBeTruthy();
    expect(searches(server.calls).map((c) => c.query.get('cursor'))).toEqual([null, 'c1', 'c2']);
    expect(screen.queryByRole('button', { name: 'さらに探す' })).toBeNull();
    expect(screen.getByText('2 件を表示しています').getAttribute('role')).toBe('status');
  });

  it('reads in English', async () => {
    const server = mockServer(routes(() => pageOf([])));
    await renderApp({ fetch: server.fetch, path: '/-/search?q=zzz', lang: 'en' });
    expect(await screen.findByText('Nothing matches “zzz”.')).toBeTruthy();
    expect(screen.getByRole('searchbox', { name: 'Search projects and MRs' })).toBeTruthy();
  });
});

describe('snippetPieces', () => {
  it('cuts by code points, not UTF-16 units', () => {
    expect(snippetPieces({ text: '𠮷野家の牛丼', highlights: [{ start: 1, end: 3 }] })).toEqual([
      { text: '𠮷', mark: false },
      { text: '野家', mark: true },
      { text: 'の牛丼', mark: false },
    ]);
  });

  it('clamps overlapping and out-of-range highlights and skips empty ones', () => {
    expect(
      snippetPieces({
        text: 'abcdef',
        highlights: [
          { start: 0, end: 2 },
          { start: 1, end: 3 },
          { start: 4, end: 4 },
          { start: 5, end: 99 },
        ],
      }),
    ).toEqual([
      { text: 'ab', mark: true },
      { text: 'c', mark: true },
      { text: 'de', mark: false },
      { text: 'f', mark: true },
    ]);
  });

  it('never turns text into markup', () => {
    const pieces = snippetPieces({ text: '<b>x</b>', highlights: [] });
    expect(pieces).toEqual([{ text: '<b>x</b>', mark: false }]);
  });
});
