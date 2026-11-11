// Plan 15 MR 9: /-/activity, Home's activity block and the sidebar item.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import type { ActivityEvent } from '../src/api/activity';
import { activitySentence } from '../src/features/ActivityItem';
import { createI18n } from '../src/i18n';
import { ACTIVITY_PATH, apiError, mockServer, signedIn, type Handler } from './mock';
import { json, renderApp } from './render';

const ACTIVITY = '/-/activity';

const event = (seq: number, over: Partial<ActivityEvent> = {}): ActivityEvent => ({
  seq,
  type: 'merge_request.opened',
  // Noon UTC: the same calendar day in every time zone the tests run in.
  occurred_at: '2026-11-10T12:00:00.000Z',
  project_id: 7,
  project_path: 'acme/forge-web',
  actor: { username: 'taro', display_name: '山田 太郎' },
  merge_request: { iid: 12, title: 'ログの文を直す' },
  ref_count: null,
  ...over,
});

const pageOf = (events: ActivityEvent[], next: number | null = null, truncated = false) =>
  json(200, { events, next_before: next, truncated });

const routes = (over: Record<string, Handler> = {}): Record<string, Handler> => ({
  ...signedIn(),
  ...over,
});

describe('activity: one sentence per type', () => {
  const cases: [Partial<ActivityEvent>, string][] = [
    [{ type: 'project.created', merge_request: null }, '山田 太郎 がプロジェクトを作りました'],
    [{ type: 'repository.pushed', merge_request: null, ref_count: 3 }, '山田 太郎 が 3 個のブランチやタグに push しました'],
    [{ type: 'repository.pushed', merge_request: null, ref_count: null }, '山田 太郎 が push しました'],
    [{ type: 'merge_request.opened' }, '山田 太郎 がマージリクエストを開きました'],
    [{ type: 'merge_request.closed' }, '山田 太郎 がマージリクエストを閉じました'],
    [{ type: 'merge_request.reopened' }, '山田 太郎 がマージリクエストをもう一度開きました'],
    [{ type: 'merge_request.commented' }, '山田 太郎 がマージリクエストにコメントしました'],
    [{ type: 'merge_request.merged' }, '山田 太郎 がマージリクエストをマージしました'],
    [{ type: 'merge_request.approved' }, '山田 太郎 がマージリクエストを承認しました'],
    // A type added later: a general sentence, never the raw name.
    [{ type: 'pipeline.finished', merge_request: null }, '山田 太郎 がプロジェクトで何かを変えました'],
    // No actor (a removed user, a system event).
    [{ type: 'merge_request.merged', actor: null }, 'だれか がマージリクエストをマージしました'],
    // No display name: the username.
    [{ actor: { username: 'hanako', display_name: '' } }, 'hanako がマージリクエストを開きました'],
  ];
  it.each(cases)('%o', async (over, sentence) => {
    const i18n = await createI18n('ja');
    expect(activitySentence(i18n.t, event(1, over))).toBe(sentence);
  });

  it('the push count is plural in English', async () => {
    const i18n = await createI18n('en');
    const pushed = (n: number) => event(1, { type: 'repository.pushed', merge_request: null, ref_count: n });
    expect(activitySentence(i18n.t, pushed(1))).toBe('山田 太郎 pushed to 1 branch or tag');
    expect(activitySentence(i18n.t, pushed(2))).toBe('山田 太郎 pushed to 2 branches or tags');
  });
});

describe('/-/activity', () => {
  it('newest first, grouped by day, with links to the merge request and the project', async () => {
    const server = mockServer(
      routes({
        [`GET ${ACTIVITY_PATH}`]: () =>
          pageOf([
            event(30),
            event(29, { type: 'repository.pushed', merge_request: null, ref_count: 2 }),
            event(28, { type: 'project.created', merge_request: null, occurred_at: '2026-11-08T12:00:00.000Z' }),
          ]),
      }),
    );
    await renderApp({ fetch: server.fetch, path: ACTIVITY });
    expect(await screen.findByRole('heading', { name: 'アクティビティ', level: 1 })).toBeTruthy();
    await screen.findByText('山田 太郎 がマージリクエストを開きました');
    // One section per day, labelled by its date.
    const days = [...document.querySelectorAll<HTMLElement>('section[aria-labelledby^="activity-day-"]')];
    expect(days).toHaveLength(2);
    expect(days.map((d) => within(d).getByRole('heading', { level: 2 }).textContent)).not.toContain('');
    const firstDay = days[0] as HTMLElement;
    const items = within(firstDay).getAllByRole('listitem');
    expect(items.map((li) => li.querySelector('p')?.textContent)).toEqual([
      '山田 太郎 がマージリクエストを開きました',
      '山田 太郎 が 2 個のブランチやタグに push しました',
    ]);
    const mr = within(items[0] as HTMLElement).getByRole('link', { name: '!12 ログの文を直す' });
    expect(mr.getAttribute('href')).toBe('/acme/forge-web/-/pull/12');
    const project = within(items[0] as HTMLElement).getByRole('link', { name: 'acme/forge-web' });
    expect(project.getAttribute('href')).toBe('/acme/forge-web');
    // Relative time, the moment in the title.
    const time = (items[0] as HTMLElement).querySelector('time');
    expect(time?.getAttribute('datetime')).toBe('2026-11-10T12:00:00.000Z');
    expect(time?.getAttribute('title')).toBeTruthy();
    expect(server.to('GET', ACTIVITY_PATH)[0]?.query.get('limit')).toBe('30');
    expect(server.to('GET', ACTIVITY_PATH)[0]?.query.get('before')).toBeNull();
    expect(server.unmatched).toEqual([]);
  });

  it('an unknown type shows the general sentence and not its raw name', async () => {
    const server = mockServer(
      routes({ [`GET ${ACTIVITY_PATH}`]: () => pageOf([event(5, { type: 'wiki.page_moved', merge_request: null })]) }),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: ACTIVITY });
    expect(await screen.findByText('山田 太郎 がプロジェクトで何かを変えました')).toBeTruthy();
    expect(container.textContent).not.toContain('wiki.page_moved');
  });

  it('"load more" asks with next_before, adds the rows, and keeps focus on the page', async () => {
    const server = mockServer(
      routes({
        [`GET ${ACTIVITY_PATH}`]: (c) =>
          c.query.get('before') === '50'
            ? pageOf([event(49, { type: 'merge_request.merged', merge_request: { iid: 3, title: null } })])
            : pageOf([event(51)], 50),
      }),
    );
    await renderApp({ fetch: server.fetch, path: ACTIVITY });
    const more = await screen.findByRole('button', { name: 'さらに読む' });
    more.focus();
    fireEvent.click(more);
    expect(await screen.findByText('山田 太郎 がマージリクエストをマージしました')).toBeTruthy();
    expect(server.to('GET', ACTIVITY_PATH)[1]?.query.get('before')).toBe('50');
    // A merge request without a title is linked by its number.
    expect(screen.getByRole('link', { name: '!3' }).getAttribute('href')).toBe('/acme/forge-web/-/pull/3');
    // The last page: the button goes away and focus moves to the first new row.
    await waitFor(() => {
      expect(document.activeElement?.id).toBe('activity-49');
    });
    expect(screen.queryByRole('button', { name: 'さらに読む' })).toBeNull();
    expect(screen.getByText('さらに 1 件を表示しました。')).toBeTruthy();
  });

  it('a last page with nothing in it says so and takes the focus there', async () => {
    const server = mockServer(
      routes({
        [`GET ${ACTIVITY_PATH}`]: (c) => (c.query.get('before') === null ? pageOf([event(9)], 8) : pageOf([])),
      }),
    );
    await renderApp({ fetch: server.fetch, path: ACTIVITY });
    fireEvent.click(await screen.findByRole('button', { name: 'さらに読む' }));
    const end = await screen.findByText('これより前の動きはありません。');
    await waitFor(() => {
      expect(document.activeElement).toBe(end);
    });
  });

  it('a failed "load more" keeps the rows; the retry asks for the next page again', async () => {
    let fail = true;
    let release: () => void = () => undefined;
    const server = mockServer(
      routes({
        [`GET ${ACTIVITY_PATH}`]: async (c) => {
          if (c.query.get('before') === null) return pageOf([event(20)], 19);
          if (fail) return apiError(503, '動きを読めません。', 'req-more-1');
          // Held until the test has seen the loading announcement.
          await new Promise<void>((resolve) => {
            release = resolve;
          });
          return pageOf([event(19, { type: 'merge_request.approved' })]);
        },
      }),
    );
    await renderApp({ fetch: server.fetch, path: ACTIVITY });
    fireEvent.click(await screen.findByRole('button', { name: 'さらに読む' }));
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('動きを読めません。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-more-1');
    // The rows already read stay.
    expect(screen.getByText('山田 太郎 がマージリクエストを開きました')).toBeTruthy();
    // Focus goes to the retry, not to the page body.
    const retry = screen.getByRole('button', { name: 'もう一度試す' });
    await waitFor(() => {
      expect(document.activeElement).toBe(retry);
    });
    fail = false;
    fireEvent.click(retry);
    // While the page is read, the live region says so.
    await waitFor(() => {
      expect(document.getElementById('activity-end')?.textContent).toBe('読み込んでいます…');
    });
    release();
    expect(await screen.findByText('山田 太郎 がマージリクエストを承認しました')).toBeTruthy();
    const asked = server.to('GET', ACTIVITY_PATH).map((c) => c.query.get('before'));
    expect(asked).toEqual([null, '19', '19']);
    expect(screen.getByText('山田 太郎 がマージリクエストを開きました')).toBeTruthy();
    expect(screen.queryByRole('alert')).toBeNull();
    await waitFor(() => {
      expect(document.activeElement?.id).toBe('activity-19');
    });
  });

  it('an unreadable moment gets a plain day heading, not the raw value', async () => {
    const server = mockServer(
      routes({ [`GET ${ACTIVITY_PATH}`]: () => pageOf([event(3, { occurred_at: 'not-a-date' })]) }),
    );
    await renderApp({ fetch: server.fetch, path: ACTIVITY });
    expect(await screen.findByRole('heading', { name: '日付不明', level: 2 })).toBeTruthy();
  });

  it('truncated is a quiet note, not an alert', async () => {
    const server = mockServer(routes({ [`GET ${ACTIVITY_PATH}`]: () => pageOf([event(1)], null, true) }));
    await renderApp({ fetch: server.fetch, path: ACTIVITY });
    expect(
      await screen.findByText('関わっているプロジェクトが多いため、一部の古いプロジェクトの動きは出ていません。'),
    ).toBeTruthy();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('loading, then the empty state', async () => {
    const server = mockServer(routes({ [`GET ${ACTIVITY_PATH}`]: () => pageOf([]) }));
    await renderApp({ fetch: server.fetch, path: ACTIVITY });
    expect(screen.getAllByText('読み込んでいます…').length).toBeGreaterThan(0);
    expect(
      await screen.findByText('まだ動きはありません。プロジェクトのメンバーになると、そこでの動きがここに出ます。'),
    ).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'さらに読む' })).toBeNull();
  });

  it('a failure shows user_message and request_id, and the retry reads again', async () => {
    let fail = true;
    const server = mockServer(
      routes({
        [`GET ${ACTIVITY_PATH}`]: () =>
          fail ? apiError(503, '動きを読めません。', 'req-activity-1') : pageOf([event(1)]),
      }),
    );
    await renderApp({ fetch: server.fetch, path: ACTIVITY });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('動きを読めません。');
    expect(screen.getByTestId('request-id').textContent).toBe('req-activity-1');
    fail = false;
    fireEvent.click(screen.getByRole('button', { name: 'もう一度試す' }));
    expect(await screen.findByText('山田 太郎 がマージリクエストを開きました')).toBeTruthy();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('the sidebar marks Activity as the current page', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: ACTIVITY });
    await screen.findByRole('heading', { name: 'アクティビティ', level: 1 });
    const nav = screen.getByRole('navigation', { name: 'マイワーク' });
    const current = within(nav)
      .getAllByRole('link')
      .filter((l) => l.getAttribute('aria-current') === 'page');
    expect(current.map((l) => l.textContent)).toEqual(['アクティビティ']);
  });
});

describe('home: activity', () => {
  it('shows the latest 10 and links to all of the activity', async () => {
    const server = mockServer(
      routes({ [`GET ${ACTIVITY_PATH}`]: () => pageOf([event(2), event(1, { type: 'merge_request.approved' })], 1) }),
    );
    await renderApp({ fetch: server.fetch });
    const section = (await screen.findByRole('heading', { name: 'アクティビティ', level: 2 })).closest('section') as HTMLElement;
    expect(await within(section).findByText('山田 太郎 がマージリクエストを承認しました')).toBeTruthy();
    expect(within(section).getAllByRole('listitem')).toHaveLength(2);
    expect(within(section).getByRole('link', { name: 'アクティビティをすべて見る' }).getAttribute('href')).toBe(
      ACTIVITY,
    );
    // The same name as the sidebar and the page, with a short lead.
    expect(within(section).getByText('関わっているプロジェクトの新しい動きを 10 件まで出しています。')).toBeTruthy();
    // Home pages no further.
    expect(within(section).queryByRole('button', { name: 'さらに読む' })).toBeNull();
    expect(server.to('GET', ACTIVITY_PATH)[0]?.query.get('limit')).toBe('10');
  });

  it('empty: says what will appear', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch });
    const section = (await screen.findByRole('heading', { name: 'アクティビティ', level: 2 })).closest('section') as HTMLElement;
    expect(
      await within(section).findByText('まだ動きはありません。プロジェクトのメンバーになると、そこでの動きがここに出ます。'),
    ).toBeTruthy();
  });

  it('a failure shows its request id with a retry', async () => {
    const server = mockServer(
      routes({ [`GET ${ACTIVITY_PATH}`]: () => apiError(500, '動きを読めません。', 'req-home-activity') }),
    );
    await renderApp({ fetch: server.fetch });
    const section = (await screen.findByRole('heading', { name: 'アクティビティ', level: 2 })).closest('section') as HTMLElement;
    expect((await within(section).findByRole('alert')).textContent).toContain('動きを読めません。');
    expect(within(section).getByTestId('request-id').textContent).toBe('req-home-activity');
    expect(within(section).getByRole('button', { name: 'もう一度試す' })).toBeTruthy();
  });
});
