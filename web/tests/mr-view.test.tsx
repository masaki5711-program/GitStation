// One merge request (U1 phase 3): the header, the one-line "why it is
// stopped" from reasons[0], the description as Markdown, and the general
// threads (comment, reply, resolve, unresolve).
import { focusManager } from '@tanstack/react-query';
import { act, fireEvent, screen, waitFor, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { STATUS_POLL_MS } from '../src/api/mergeRequests';
import { apiError, mockServer, SESSION } from './mock';
import {
  MR_API,
  MR_SCREENS,
  discussion,
  discussions,
  mr,
  mrRoutes,
  note,
  status,
} from './mr-fixtures';
import { json, renderApp } from './render';

const VIEW = `${MR_SCREENS}/pull/3`;
const UNRESOLVED_1 = status([{ reason: 'unresolved_discussions', detail: 'count=1' }]);

async function open(over: Parameters<typeof mrRoutes>[0] = {}, path = VIEW) {
  const server = mockServer(mrRoutes(over));
  const view = await renderApp({ fetch: server.fetch, path, lang: 'en' });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return { server, ...view };
}

const statusLine = () => screen.findByTestId('mr-status');

describe('merge request header and status line', () => {
  it('shows the title, number, author name, branches and the status line from reasons[0]', async () => {
    const { server } = await open({
      [`GET ${MR_API}/3/status`]: () =>
        json(
          200,
          status([
            { reason: 'conflict', detail: null },
            { reason: 'approvals_missing', detail: 'rule=@protected_branch required=1 counted=0' },
          ]),
        ),
    });
    expect(screen.getByRole('heading', { level: 1 }).textContent).toContain('!3');
    expect(screen.getByTestId('mr-opened').textContent).toMatch(/^Opened .+ by 佐藤 花子$/);
    expect(screen.getByText(/dev into main/)).toBeTruthy();
    const line = await statusLine();
    expect(line.getAttribute('role')).toBe('status');
    await waitFor(() => {
      expect(line.textContent).toBe('It conflicts with other changes');
    });
    // Only the first reason is the line (U2: the conditions panel lists the rest).
    expect(line.textContent).not.toContain('Not enough approvals');
    expect(screen.getByText('Push a fix')).toBeTruthy();
    // No viewer flag: no approve or merge button (U2 shows them from the flags only).
    expect(screen.queryByRole('button', { name: /approve|merge/i })).toBeNull();
    const nav = await screen.findByRole('navigation', { name: 'Project menu' });
    expect(within(nav).getByRole('link', { name: 'MR' }).getAttribute('aria-current')).toBe('page');
    expect(server.unmatched).toEqual([]);
  });

  it('a mergeable status says so with nothing to press', async () => {
    await open();
    await waitFor(async () => {
      expect((await statusLine()).textContent).toBe('Ready to merge');
    });
  });

  it('"see the changes" leads to the Changes tab', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () => json(200, status([{ reason: 'mystery', detail: null }])),
    });
    await waitFor(async () => {
      expect((await statusLine()).textContent).toBe('Cannot be merged right now. Check the changes');
    });
    const tabs = screen.getByRole('navigation', { name: 'Merge request views' });
    expect(within(tabs).getByRole('link', { name: 'Overview' }).getAttribute('aria-current')).toBe(
      'page',
    );
    fireEvent.click(screen.getByRole('link', { name: 'See the changes' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe(`${VIEW}/changes`);
    });
    const after = screen.getByRole('navigation', { name: 'Merge request views' });
    expect(within(after).getByRole('link', { name: 'Changes' }).getAttribute('aria-current')).toBe(
      'page',
    );
    // The latest version's diff is there (tests/mr-changes.test.tsx has the rest).
    expect(await screen.findByText('Version 1 (the latest)')).toBeTruthy();
  });

  it('"next" moves the focus to the first unresolved thread', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () => json(200, UNRESOLVED_1),
      [`GET ${MR_API}/3/discussions`]: () =>
        json(200, discussions([discussion(1, { resolved: true }), discussion(2), discussion(4)])),
    });
    await waitFor(async () => {
      expect((await statusLine()).textContent).toBe('Unresolved threads: 1');
    });
    await screen.findByTestId('thread-2');
    fireEvent.click(screen.getByRole('button', { name: 'Open the first unresolved thread' }));
    expect(document.activeElement).toBe(screen.getByTestId('thread-2'));
  });

  it('a failed status read shows only that line as an error; the body and threads still show', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () => apiError(503, '状態を読めませんでした', 'req-st-1'),
      [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([discussion(2)])),
    });
    const area = await screen.findByTestId('mr-status-area');
    const alert = await within(area).findByRole('alert');
    expect(within(alert).getByText('状態を読めませんでした')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-st-1');
    expect(await screen.findByText('Synthetic comment 2')).toBeTruthy();
    expect(screen.getByRole('heading', { level: 2, name: 'Description' })).toBeTruthy();
    expect(screen.getAllByRole('alert')).toHaveLength(1);
  });

  describe('polling', () => {
    afterEach(() => {
      vi.useRealTimers();
      focusManager.setFocused(undefined);
    });

    it.each(['mergeability_unknown', 'merge_in_progress'])(
      'reads the status again while reasons[0] is %s',
      async (reason) => {
        vi.useFakeTimers({
          shouldAdvanceTime: true,
          toFake: ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval'],
        });
        focusManager.setFocused(true);
        let answer = status([{ reason, detail: null }]);
        await open({ [`GET ${MR_API}/3/status`]: () => json(200, answer) });
        await waitFor(async () => {
          expect((await statusLine()).textContent).not.toBe('Ready to merge');
        });
        answer = status([]);
        await act(async () => {
          await vi.advanceTimersByTimeAsync(STATUS_POLL_MS * 2);
        });
        await waitFor(async () => {
          expect((await statusLine()).textContent).toBe('Ready to merge');
        });
      },
    );
  });
});

describe('description', () => {
  it('is Markdown with no raw HTML', async () => {
    await open({
      [`GET ${MR_API}/3`]: () =>
        json(
          200,
          mr(3, {
            description:
              '# Why\n\nThe **pump** changed. <script>alert(1)</script><img src="x" onerror="alert(1)">\n\n<b>raw</b>',
          }),
        ),
    });
    expect(await screen.findByRole('heading', { level: 3, name: 'Why' })).toBeTruthy();
    expect(screen.getByText('pump').tagName).toBe('STRONG');
    const section = screen.getByRole('region', { name: 'Description' });
    expect(section.querySelector('script')).toBeNull();
    expect(section.querySelector('img')).toBeNull();
    expect(section.querySelector('b')).toBeNull();
  });

  it('an empty description says so', async () => {
    await open();
    expect(await screen.findByText('No description.')).toBeTruthy();
  });
});

describe('threads', () => {
  it('lists the notes with the author name; empty and truncated states', async () => {
    await open({
      [`GET ${MR_API}/3/discussions`]: () =>
        json(
          200,
          discussions(
            [
              discussion(2, {
                notes: [note(20, 'First **note**'), note(21, 'A reply', { author: { username: 'jiro', display_name: '鈴木 次郎' } })],
              }),
            ],
            true,
          ),
        ),
    });
    const thread = await screen.findByTestId('thread-2');
    expect(await within(thread).findByText('note')).toBeTruthy();
    expect(within(thread).getByText(/鈴木 次郎/)).toBeTruthy();
    expect(within(thread).getByText('Unresolved')).toBeTruthy();
    const notice = screen.getByTestId('mr-threads-truncated');
    expect(notice.getAttribute('role')).toBe('status');
  });

  it('no threads: says so', async () => {
    await open();
    expect(await screen.findByText('No comments yet.')).toBeTruthy();
  });

  it.each([
    [{ outdated: true, outdated_unknown: false }, 'This line was changed by a later version.'],
    [
      { outdated: true, outdated_unknown: true },
      'Could not tell whether a later version changed this line. Look at it again.',
    ],
  ])('an inline thread shows path:line and the outdated marker %#', async (flags, text) => {
    await open({
      [`GET ${MR_API}/3/discussions`]: () =>
        json(
          200,
          discussions([
            discussion(5, {
              kind: 'inline',
              position: { path: 'src/lib.rs', line: 3, side: 'new', version: 1 },
              ...flags,
            }),
          ]),
        ),
    });
    const thread = await screen.findByTestId('thread-5');
    expect(within(thread).getByText('src/lib.rs:3')).toBeTruthy();
    expect(within(thread).getByText(text)).toBeTruthy();
  });

  it('a current inline thread has no outdated marker', async () => {
    await open({
      [`GET ${MR_API}/3/discussions`]: () =>
        json(
          200,
          discussions([
            discussion(5, {
              kind: 'inline',
              position: { path: 'src/lib.rs', line: 3, side: 'new', version: 1 },
            }),
          ]),
        ),
    });
    const thread = await screen.findByTestId('thread-5');
    expect(within(thread).queryByText(/later version/)).toBeNull();
  });

  it('posts a general comment with CSRF, shows it and clears the box', async () => {
    let list = [discussion(2)];
    const { server } = await open({
      [`GET ${MR_API}/3/discussions`]: () => json(200, discussions(list)),
      [`POST ${MR_API}/3/discussions`]: () => {
        list = [...list, discussion(7, { notes: [note(70, 'Looks right to me')] })];
        return json(201, list[1]);
      },
    });
    await screen.findByTestId('thread-2');
    const box = screen.getByLabelText<HTMLTextAreaElement>('Add a comment');
    fireEvent.change(box, { target: { value: '  Looks right to me ' } });
    fireEvent.click(screen.getByRole('button', { name: 'Comment' }));
    expect(await screen.findByText('Looks right to me')).toBeTruthy();
    const post = server.to('POST', `${MR_API}/3/discussions`)[0];
    expect(post?.body).toEqual({ body: 'Looks right to me' });
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    await waitFor(() => {
      expect(box.value).toBe('');
    });
  });

  it('an empty comment is refused before sending', async () => {
    const { server } = await open();
    await screen.findByText('No comments yet.');
    fireEvent.click(screen.getByRole('button', { name: 'Comment' }));
    expect(await screen.findByText('Write a comment first.')).toBeTruthy();
    expect(screen.getByLabelText('Add a comment').getAttribute('aria-invalid')).toBe('true');
    expect(server.to('POST', `${MR_API}/3/discussions`)).toHaveLength(0);
  });

  it('a refused comment shows the user_message and keeps the text', async () => {
    await open({
      [`POST ${MR_API}/3/discussions`]: () => apiError(422, 'コメントが長すぎます', 'req-c-1'),
    });
    await screen.findByText('No comments yet.');
    const box = screen.getByLabelText<HTMLTextAreaElement>('Add a comment');
    fireEvent.change(box, { target: { value: 'kept' } });
    fireEvent.click(screen.getByRole('button', { name: 'Comment' }));
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('コメントが長すぎます')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-c-1');
    expect(box.value).toBe('kept');
  });

  it('replies in a thread', async () => {
    let notes = [note(20, 'Question')];
    const { server } = await open({
      [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([discussion(2, { notes })])),
      [`POST ${MR_API}/3/discussions/2/notes`]: () => {
        notes = [...notes, note(21, 'Answer')];
        return json(201, notes[1]);
      },
    });
    const thread = await screen.findByTestId('thread-2');
    fireEvent.click(within(thread).getByRole('button', { name: 'Reply' }));
    const box = within(thread).getByLabelText('Your reply');
    expect(document.activeElement).toBe(box);
    fireEvent.change(box, { target: { value: 'Answer' } });
    fireEvent.click(within(thread).getByRole('button', { name: 'Send reply' }));
    expect(await within(thread).findByText('Answer')).toBeTruthy();
    expect(server.to('POST', `${MR_API}/3/discussions/2/notes`)[0]?.body).toEqual({ body: 'Answer' });
  });

  it('resolving a thread updates the status line', async () => {
    let resolved = false;
    const { server } = await open({
      [`GET ${MR_API}/3/status`]: () => json(200, resolved ? status([]) : UNRESOLVED_1),
      [`GET ${MR_API}/3/discussions`]: () =>
        json(200, discussions([discussion(2, { resolved })])),
      [`POST ${MR_API}/3/discussions/2/resolve`]: () => {
        resolved = true;
        return json(200, discussion(2, { resolved: true }));
      },
    });
    await waitFor(async () => {
      expect((await statusLine()).textContent).toBe('Unresolved threads: 1');
    });
    const thread = await screen.findByTestId('thread-2');
    fireEvent.click(within(thread).getByRole('button', { name: 'Resolve thread' }));
    await waitFor(async () => {
      expect((await statusLine()).textContent).toBe('Ready to merge');
    });
    // It moved under the resolved summary (a new element there).
    expect(within(screen.getByTestId('thread-2')).getByText('Resolved')).toBeTruthy();
    expect(
      server.to('POST', `${MR_API}/3/discussions/2/resolve`)[0]?.headers.get('X-GitStation-CSRF'),
    ).toBe(SESSION.csrf_token);
  });

  it('unresolves a resolved thread', async () => {
    let resolved = true;
    const { server } = await open({
      [`GET ${MR_API}/3/discussions`]: () =>
        json(200, discussions([discussion(2, { resolved })])),
      [`POST ${MR_API}/3/discussions/2/unresolve`]: () => {
        resolved = false;
        return json(200, discussion(2));
      },
    });
    // Resolved threads are folded behind their count.
    fireEvent.click(await screen.findByText('Resolved threads: 1'));
    const thread = screen.getByTestId('thread-2');
    fireEvent.click(within(thread).getByRole('button', { name: 'Reopen thread' }));
    await waitFor(() => {
      expect(
        within(screen.getByTestId('thread-2')).getByRole('button', { name: 'Resolve thread' }),
      ).toBeTruthy();
    });
    expect(server.to('POST', `${MR_API}/3/discussions/2/unresolve`)).toHaveLength(1);
  });

  it('a failed resolve shows the user_message in the thread', async () => {
    await open({
      [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([discussion(2)])),
      [`POST ${MR_API}/3/discussions/2/resolve`]: () =>
        apiError(403, '解決する権限がありません', 'req-r-1'),
    });
    const thread = await screen.findByTestId('thread-2');
    fireEvent.click(within(thread).getByRole('button', { name: 'Resolve thread' }));
    const alert = await within(thread).findByRole('alert');
    expect(within(alert).getByText('解決する権限がありません')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-r-1');
  });

  it('a failed thread read shows its error and retry; the header still shows', async () => {
    let fail = true;
    await open({
      [`GET ${MR_API}/3/discussions`]: () =>
        fail ? apiError(500, 'スレッドを読めません', 'req-d-1') : json(200, discussions([discussion(2)])),
    });
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('スレッドを読めません')).toBeTruthy();
    fail = false;
    fireEvent.click(within(alert).getByRole('button', { name: 'Try again' }));
    expect(await screen.findByTestId('thread-2')).toBeTruthy();
  });
});

describe('addresses', () => {
  it.each(['abc', '0', '-1', '03', '1.5', '3/other'])(
    '/-/pull/%s is not found without asking the MR API',
    async (tail) => {
      const server = mockServer(mrRoutes());
      await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pull/${tail}`, lang: 'en' });
      expect(await screen.findByRole('heading', { level: 1, name: 'Page not found' })).toBeTruthy();
      expect(server.calls.filter((c) => c.path.startsWith(MR_API))).toEqual([]);
    },
  );

  it('a merge request the API does not know shows its message', async () => {
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}/3`]: () => apiError(404, 'マージリクエストが見つかりません', 'req-404'),
      }),
    );
    await renderApp({ fetch: server.fetch, path: VIEW, lang: 'en' });
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('マージリクエストが見つかりません')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-404');
  });
});
