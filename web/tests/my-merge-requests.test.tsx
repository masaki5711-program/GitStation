// Plan 15 MR 7: Home's count cards, /-/merge-requests and the sidebar badge.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { MR_COUNTS_PATH, apiError, mockServer, page, signedIn, type Handler } from './mock';
import { json, renderApp } from './render';

const LIST_PATH = '/api/v1/user/merge-requests';

const count = (n: number, capped = false) => ({ count: n, capped });
const counts = (attention: number, approvable: number, authored: number, cappedApprovable = false) => ({
  attention: count(attention),
  approvable: count(approvable, cappedApprovable),
  authored: count(authored),
});

const mr = (id: number, over: Record<string, unknown> = {}) => ({
  id,
  project_id: 7,
  project_path: 'acme/forge-web',
  iid: id + 100,
  title: `Change ${String(id)}`,
  state: 'open',
  draft: false,
  source_branch: 'feature/x',
  target_branch: 'main',
  author_id: 5,
  author: { username: 'taro', display_name: '山田 太郎' },
  head_sha: 'a'.repeat(40),
  attention: [],
  created_at: '2026-11-01T00:00:00Z',
  updated_at: '2026-11-02T03:04:00Z',
  ...over,
});

const listOf = (scope: string, rows: unknown[], next: number | null = null) =>
  json(200, { scope, merge_requests: rows, next_before: next });

const routes = (over: Record<string, Handler> = {}): Record<string, Handler> => ({
  ...signedIn(),
  [`GET ${MR_COUNTS_PATH}`]: () => json(200, counts(1, 3, 2)),
  [`GET ${LIST_PATH}`]: (c) => listOf(c.query.get('scope') ?? '', []),
  ...over,
});

/** The text a link's aria-describedby points at ('' without one). */
const description = (el: HTMLElement) => {
  const id = el.getAttribute('aria-describedby');
  return id === null ? '' : (document.getElementById(id)?.textContent ?? '');
};

const nav = () => screen.findByRole('navigation', { name: 'マイワーク' });

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`);
}

describe('home: count cards', () => {
  it('each count is a link to its list, named by its label and number', async () => {
    const server = mockServer(
      routes({ 'GET /api/v1/user/notifications/unread-count': () => json(200, { count: 4, capped: false }) }),
    );
    await renderApp({ fetch: server.fetch });
    const attention = await screen.findByRole('link', { name: '対応が必要な自分の MR 1 件' });
    expect(attention.getAttribute('href')).toBe('/-/merge-requests?scope=attention');
    expect(screen.getByRole('link', { name: '承認を待っている MR 3 件' }).getAttribute('href')).toBe(
      '/-/merge-requests?scope=approvable',
    );
    expect(screen.getByRole('link', { name: '自分の MR 2 件' }).getAttribute('href')).toBe(
      '/-/merge-requests?scope=authored',
    );
    expect((await screen.findByRole('link', { name: '未読の通知 4 件' })).getAttribute('href')).toBe(
      '/-/notifications?unread=1',
    );
    // One request for the counts, shared by the cards and the sidebar badge.
    expect(server.to('GET', MR_COUNTS_PATH)).toHaveLength(1);
    expect(server.unmatched).toEqual([]);
  });

  it('a count of 0 says the next step in one line (the description), not a drawn "0"', async () => {
    const server = mockServer(routes({ [`GET ${MR_COUNTS_PATH}`]: () => json(200, counts(0, 0, 0)) }));
    await renderApp({ fetch: server.fetch });
    await screen.findByText('ブランチを push して MR を作るとここに出ます。');
    // The link's name is its label and number only; the next step describes it.
    const authored = screen.getByRole('link', { name: '自分の MR 0 件' });
    expect(description(authored)).toBe('ブランチを push して MR を作るとここに出ます。');
    expect(authored.querySelector('[aria-hidden="true"]')).toBeNull();
    expect(description(screen.getByRole('link', { name: '承認を待っている MR 0 件' }))).toBe(
      '承認を頼まれた MR はここに出ます。',
    );
    expect(description(screen.getByRole('link', { name: '対応が必要な自分の MR 0 件' }))).toBe(
      '衝突や失敗したチェックはありません。',
    );
    expect(await screen.findByText('新しい通知はありません。')).toBeTruthy();
  });

  it('a card that waits on me stands out when not 0; the others stay quiet', async () => {
    const server = mockServer(routes({ [`GET ${MR_COUNTS_PATH}`]: () => json(200, counts(2, 0, 5)) }));
    await renderApp({ fetch: server.fetch });
    const card = async (name: string) => (await screen.findByRole('link', { name })).closest('li') as HTMLElement;
    expect((await card('対応が必要な自分の MR 2 件')).className).toContain('cardWaiting');
    expect((await card('承認を待っている MR 0 件')).className).not.toContain('cardWaiting');
    expect((await card('自分の MR 5 件')).className).not.toContain('cardWaiting');
    // A number has no description: the name says it all.
    expect(screen.getByRole('link', { name: '自分の MR 5 件' }).hasAttribute('aria-describedby')).toBe(false);
  });

  it('the count has its unit in every language (en)', async () => {
    const server = mockServer(routes({ [`GET ${MR_COUNTS_PATH}`]: () => json(200, counts(1, 3, 0)) }));
    await renderApp({ fetch: server.fetch, lang: 'en' });
    expect(await screen.findByRole('link', { name: 'Your merge requests that need action 1 item' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Merge requests waiting for your approval 3 items' })).toBeTruthy();
  });

  it('a failed unread count shows its reason with the request id and a retry', async () => {
    let fail = true;
    const server = mockServer(
      routes({
        'GET /api/v1/user/notifications/unread-count': () =>
          fail ? apiError(503, '通知の数を読めません。', 'req-unread-1') : json(200, { count: 2, capped: false }),
      }),
    );
    await renderApp({ fetch: server.fetch });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('通知の数を読めません。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-unread-1');
    expect(description(screen.getByRole('link', { name: /^未読の通知/ }))).toBe('数を読めませんでした。');
    fail = false;
    fireEvent.click(screen.getByRole('button', { name: 'もう一度試す' }));
    expect(await screen.findByRole('link', { name: '未読の通知 2 件' })).toBeTruthy();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('a capped count shows "99+" (the server stops counting at 100)', async () => {
    const server = mockServer(
      routes({ [`GET ${MR_COUNTS_PATH}`]: () => json(200, counts(0, 100, 0, true)) }),
    );
    await renderApp({ fetch: server.fetch });
    expect(await screen.findByRole('link', { name: '承認を待っている MR 99+ 件' })).toBeTruthy();
  });

  it('failed counts: one alert with user_message and request_id, a retry, and the cards say so', async () => {
    let fail = true;
    const server = mockServer(
      routes({
        [`GET ${MR_COUNTS_PATH}`]: () =>
          fail ? apiError(500, '数を読めませんでした（サーバー）。', 'req-counts-1', 'FG-DB-0001') : json(200, counts(1, 0, 0)),
      }),
    );
    const { container } = await renderApp({ fetch: server.fetch });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('数を読めませんでした（サーバー）。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-counts-1');
    expect(container.textContent).not.toContain('FG-DB-0001');
    expect(description(screen.getByRole('link', { name: /^自分の MR/ }))).toBe('数を読めませんでした。');
    fail = false;
    fireEvent.click(screen.getByRole('button', { name: 'もう一度試す' }));
    expect(await screen.findByRole('link', { name: '対応が必要な自分の MR 1 件' })).toBeTruthy();
  });

  it('axe: no violations with the cards', async () => {
    const server = mockServer(routes());
    const { container } = await renderApp({ fetch: server.fetch });
    await screen.findByRole('link', { name: '自分の MR 2 件' });
    expect(await violations(container)).toEqual([]);
  });
});

describe('/-/merge-requests', () => {
  it('shows a labelled section per scope, rows with title, project, reasons and time, linking to the MR', async () => {
    const server = mockServer(
      routes({
        [`GET ${LIST_PATH}`]: (c) => {
          const scope = c.query.get('scope') ?? '';
          if (scope === 'attention') {
            return listOf(scope, [mr(9, { attention: ['conflict', 'required_check_failed', 'something_new'] })]);
          }
          if (scope === 'approvable') return listOf(scope, [mr(8, { draft: true, title: 'Review me' })]);
          return listOf(scope, []);
        },
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/merge-requests' });
    expect(await screen.findByRole('heading', { level: 1, name: 'マージリクエスト' })).toBeTruthy();
    const attention = screen.getByRole('region', { name: /^対応が必要な自分の MR/ });
    const link = await within(attention).findByRole('link', { name: 'Change 9' });
    expect(link.getAttribute('href')).toBe('/acme/forge-web/-/pull/109');
    const row = link.closest('li') as HTMLElement;
    expect(row.textContent).toContain('acme/forge-web!109');
    expect(row.textContent).toContain('衝突があります');
    expect(row.textContent).toContain('必須のチェックが失敗しました');
    expect(row.textContent).toContain('対応が必要です');
    expect(row.textContent).not.toContain('required_check_failed');
    // Relative time; the moment itself in the title.
    const time = row.querySelector('time') as HTMLElement;
    expect(time.getAttribute('datetime')).toBe('2026-11-02T03:04:00Z');
    expect(time.textContent).toMatch(/^更新 /);
    expect(time.getAttribute('title')).toMatch(/2026/);
    const approvable = screen.getByRole('region', { name: /^承認を待っている MR/ });
    expect(await within(approvable).findByRole('link', { name: 'Review me' })).toBeTruthy();
    expect(within(approvable).getByText('下書き')).toBeTruthy();
    // The section is named by its summary (plain text, the count with it);
    // no heading inside the disclosure button, no second list name.
    expect(screen.getByRole('region', { name: '承認を待っている MR 3 件' })).toBe(approvable);
    expect(approvable.querySelector('summary h2')).toBeNull();
    expect(approvable.querySelector('ul')?.hasAttribute('aria-label')).toBe(false);
    // An empty section says the next step (plan 08), as Home's card does.
    const authored = screen.getByRole('region', { name: /^自分の MR/ });
    expect(await within(authored).findByText('ブランチを push して MR を作るとここに出ます。')).toBeTruthy();
    // No empty status line is left in the sections.
    const statuses = [...document.querySelectorAll('section details [role="status"]')];
    expect(statuses.filter((e) => e.textContent === '')).toEqual([]);
    expect(server.unmatched).toEqual([]);
  });

  it('?scope= opens that section alone; the others open (and read) on demand', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/-/merge-requests?scope=approvable' });
    await screen.findByText('承認を頼まれた MR はここに出ます。');
    const details = (name: RegExp) =>
      screen.getByRole('region', { name }).querySelector('details') as HTMLDetailsElement;
    expect(details(/^承認を待っている MR/).open).toBe(true);
    expect(details(/^対応が必要な自分の MR/).open).toBe(false);
    expect(details(/^自分の MR/).open).toBe(false);
    expect(server.to('GET', LIST_PATH).map((c) => c.query.get('scope'))).toEqual(['approvable']);
    // The summary is the native disclosure button (keyboard: Enter / Space).
    const authored = details(/^自分の MR/);
    authored.open = true;
    fireEvent(authored, new Event('toggle'));
    expect(await screen.findByText('ブランチを push して MR を作るとここに出ます。')).toBeTruthy();
    expect(server.to('GET', LIST_PATH).map((c) => c.query.get('scope'))).toEqual(['approvable', 'authored']);
  });

  it('reads more with next_before ("さらに読む") and says how many were added', async () => {
    const server = mockServer(
      routes({
        [`GET ${LIST_PATH}`]: (c) =>
          c.query.get('before') === null
            ? listOf('authored', [mr(30), mr(29)], 29)
            : listOf('authored', [mr(12)], null),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/merge-requests?scope=authored' });
    await screen.findByRole('link', { name: 'Change 29' });
    fireEvent.click(screen.getByRole('button', { name: 'さらに読む' }));
    expect(await screen.findByRole('link', { name: 'Change 12' })).toBeTruthy();
    const calls = server.to('GET', LIST_PATH);
    expect(calls.map((c) => [c.query.get('scope'), c.query.get('before'), c.query.get('limit')])).toEqual([
      ['authored', null, '20'],
      ['authored', '29', '20'],
    ]);
    expect(screen.queryByRole('button', { name: 'さらに読む' })).toBeNull();
    expect(screen.getByText('1 件を足しました。')).toBeTruthy();
  });

  it('a failed section shows user_message and request_id with a retry; the others still show', async () => {
    let fail = true;
    const server = mockServer(
      routes({
        [`GET ${LIST_PATH}`]: (c) => {
          const scope = c.query.get('scope') ?? '';
          if (scope === 'approvable' && fail) return apiError(500, '一覧を読めませんでした。', 'req-mrs-1', 'FG-DB-0001');
          return listOf(scope, scope === 'approvable' ? [mr(5)] : []);
        },
      }),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: '/-/merge-requests' });
    const approvable = await screen.findByRole('region', { name: /^承認を待っている MR/ });
    const alert = await within(approvable).findByRole('alert');
    expect(alert.textContent).toContain('一覧を読めませんでした。');
    expect(within(approvable).getByTestId('request-id').textContent).toBe('req-mrs-1');
    expect(container.textContent).not.toContain('FG-DB-0001');
    expect(await screen.findByText('ブランチを push して MR を作るとここに出ます。')).toBeTruthy();
    fail = false;
    fireEvent.click(within(approvable).getByRole('button', { name: 'もう一度試す' }));
    expect(await within(approvable).findByRole('link', { name: 'Change 5' })).toBeTruthy();
  });

  it('a failed later page keeps the rows and retries that page', async () => {
    let fail = true;
    const server = mockServer(
      routes({
        [`GET ${LIST_PATH}`]: (c) => {
          if (c.query.get('before') === null) return listOf('authored', [mr(30)], 30);
          return fail ? apiError(503, 'いまは読めません。', 'req-page-2') : listOf('authored', [mr(10)]);
        },
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/merge-requests?scope=authored' });
    fireEvent.click(await screen.findByRole('button', { name: 'さらに読む' }));
    expect(await screen.findByText('いまは読めません。')).toBeTruthy();
    expect(screen.getByTestId('request-id').textContent).toBe('req-page-2');
    expect(screen.getByRole('link', { name: 'Change 30' })).toBeTruthy();
    fail = false;
    fireEvent.click(screen.getByRole('button', { name: 'もう一度試す' }));
    expect(await screen.findByRole('link', { name: 'Change 10' })).toBeTruthy();
    expect(server.to('GET', LIST_PATH).filter((c) => c.query.get('before') === null)).toHaveLength(1);
  });

  it('failed counts: said once for the page (request id, retry); each summary marks the count unknown', async () => {
    let fail = true;
    const server = mockServer(
      routes({
        [`GET ${MR_COUNTS_PATH}`]: () =>
          fail ? apiError(500, '件数を読めませんでした。', 'req-counts-9') : json(200, counts(0, 3, 0)),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/merge-requests?scope=authored' });
    const main = within(await screen.findByRole('main'));
    const alert = await main.findByRole('alert');
    expect(main.getAllByRole('alert')).toHaveLength(1);
    expect(alert.textContent).toContain('件数を読めませんでした。');
    expect(main.getByTestId('request-id').textContent).toBe('req-counts-9');
    expect(screen.getByRole('region', { name: '承認を待っている MR 件数不明' })).toBeTruthy();
    expect(screen.getByRole('region', { name: '自分の MR 件数不明' })).toBeTruthy();
    fail = false;
    fireEvent.click(main.getByRole('button', { name: 'もう一度試す' }));
    expect(await screen.findByRole('region', { name: '承認を待っている MR 3 件' })).toBeTruthy();
    expect(main.queryByRole('alert')).toBeNull();
  });

  it('axe: no violations with rows and a closed section', async () => {
    const server = mockServer(
      routes({ [`GET ${LIST_PATH}`]: (c) => listOf(c.query.get('scope') ?? '', [mr(3, { attention: ['conflict'] })]) }),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: '/-/merge-requests?scope=attention' });
    await screen.findByRole('link', { name: 'Change 3' });
    expect(await violations(container)).toEqual([]);
  });
});

describe('sidebar: merge requests badge', () => {
  it('names what waits on me (attention + approvable) and marks the page current', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/-/merge-requests' });
    // jsdom puts a space between the name and the hidden text; browsers may not.
    const link = await within(await nav()).findByRole('link', { name: /^マージリクエスト\s?、対応 4 件$/ });
    expect(link.getAttribute('href')).toBe('/-/merge-requests');
    expect(link.getAttribute('aria-current')).toBe('page');
    // The drawn number is hidden from assistive tech (the name carries it).
    expect(within(link).getByText('4').getAttribute('aria-hidden')).toBe('true');
  });

  it('a capped count reads "99+"', async () => {
    const server = mockServer(routes({ [`GET ${MR_COUNTS_PATH}`]: () => json(200, counts(2, 100, 0, true)) }));
    await renderApp({ fetch: server.fetch });
    expect(await within(await nav()).findByRole('link', { name: /^マージリクエスト\s?、対応 99\+ 件$/ })).toBeTruthy();
  });

  it('no badge when nothing waits (or the count failed)', async () => {
    const server = mockServer(
      routes({ [`GET ${MR_COUNTS_PATH}`]: () => json(200, counts(0, 0, 5)), 'GET /api/v1/groups': () => page([]) }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/groups' });
    await screen.findByRole('heading', { level: 1, name: 'グループ' });
    await waitFor(() => {
      expect(server.to('GET', MR_COUNTS_PATH)).toHaveLength(1);
    });
    expect(within(await nav()).getByRole('link', { name: 'マージリクエスト' }).getAttribute('aria-current')).toBeNull();
  });
});
