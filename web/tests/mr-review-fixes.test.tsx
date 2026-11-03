// U1 independent review fixes: focus after a comment box closes (a11y HIGH),
// one announcement per event, the thread's description, the Changes tab's
// truncated notice and per-line comment state, "next" from the Changes tab,
// the "wait" / "none" hints and tones, one state badge, resolved threads
// collapsed, and the list's empty state.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer } from './mock';
import {
  MR_API,
  MR_SCREENS,
  discussion,
  discussions,
  mr,
  mrRoutes,
  status,
} from './mr-fixtures';
import { json, renderApp } from './render';

const VIEW = `${MR_SCREENS}/pull/3`;
const CHANGES = `${VIEW}/changes`;
const LINE3 = 'Comment on line 3 of src/lib.rs';
const UNRESOLVED_1 = status([{ reason: 'unresolved_discussions', detail: 'count=1' }]);

async function open(over: Parameters<typeof mrRoutes>[0] = {}, path = VIEW) {
  const server = mockServer(mrRoutes(over));
  await renderApp({ fetch: server.fetch, path, lang: 'en' });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return server;
}

const inline = (id: number, over: Record<string, unknown> = {}) =>
  discussion(id, {
    kind: 'inline',
    position: { path: 'src/lib.rs', line: 3, side: 'new', version: 1 },
    ...over,
  });

describe('focus returns when a comment box closes (a11y HIGH)', () => {
  it('Changes: cancel puts the focus back on that line button', async () => {
    await open({}, CHANGES);
    fireEvent.click(await screen.findByRole('button', { name: LINE3 }));
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(document.activeElement).toBe(screen.getByRole('button', { name: LINE3 }));
  });

  it('Changes: a sent comment puts the focus back on that line button', async () => {
    let list: unknown[] = [];
    await open(
      {
        [`GET ${MR_API}/3/discussions`]: () => json(200, discussions(list)),
        [`POST ${MR_API}/3/discussions`]: () => {
          list = [inline(9)];
          return json(201, list[0]);
        },
      },
      CHANGES,
    );
    fireEvent.click(await screen.findByRole('button', { name: LINE3 }));
    fireEvent.change(screen.getByLabelText('Your comment: line 3 of src/lib.rs'), {
      target: { value: 'x' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Comment' }));
    await screen.findByTestId('thread-9');
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByRole('button', { name: LINE3 }));
    });
  });

  it('a thread: cancel puts the focus back on Reply', async () => {
    await open({ [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([discussion(2)])) });
    const thread = await screen.findByTestId('thread-2');
    fireEvent.click(within(thread).getByRole('button', { name: 'Reply' }));
    fireEvent.click(within(thread).getByRole('button', { name: 'Cancel' }));
    expect(document.activeElement).toBe(within(thread).getByRole('button', { name: 'Reply' }));
  });

  it('a thread: a sent reply puts the focus on the thread', async () => {
    await open({
      [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([discussion(2)])),
      [`POST ${MR_API}/3/discussions/2/notes`]: () => json(201, { id: 21 }),
    });
    const thread = await screen.findByTestId('thread-2');
    fireEvent.click(within(thread).getByRole('button', { name: 'Reply' }));
    fireEvent.change(within(thread).getByLabelText('Your reply'), { target: { value: 'ok' } });
    fireEvent.click(within(thread).getByRole('button', { name: 'Send reply' }));
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByTestId('thread-2'));
    });
  });

  it('Overview: resolving moves the thread under the resolved summary, which takes the focus', async () => {
    let resolved = false;
    await open({
      [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([discussion(2, { resolved })])),
      [`POST ${MR_API}/3/discussions/2/resolve`]: () => {
        resolved = true;
        return json(200, discussion(2, { resolved: true }));
      },
    });
    fireEvent.click(
      within(await screen.findByTestId('thread-2')).getByRole('button', { name: 'Resolve thread' }),
    );
    await waitFor(() => {
      expect(document.activeElement?.textContent).toBe('Resolved threads: 1');
    });
  });

  it('Overview: reopening moves the thread back and focuses it', async () => {
    let resolved = true;
    await open({
      [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([discussion(2, { resolved })])),
      [`POST ${MR_API}/3/discussions/2/unresolve`]: () => {
        resolved = false;
        return json(200, discussion(2));
      },
    });
    fireEvent.click(await screen.findByText('Resolved threads: 1'));
    fireEvent.click(
      within(screen.getByTestId('thread-2')).getByRole('button', { name: 'Reopen thread' }),
    );
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByTestId('thread-2'));
    });
  });
});

describe('announcements and names (a11y MEDIUM)', () => {
  it('an empty comment is announced once: through the focused box, not an alert', async () => {
    await open();
    await screen.findByText('No comments yet.');
    fireEvent.click(screen.getByRole('button', { name: 'Comment' }));
    const error = await screen.findByText('Write a comment first.');
    expect(error.getAttribute('role')).toBeNull();
    const box = screen.getByLabelText('Add a comment');
    expect(document.activeElement).toBe(box);
    expect(box.getAttribute('aria-describedby')?.split(' ')).toContain(error.id);
  });

  it('resolve and reopen are announced politely', async () => {
    let resolved = false;
    await open(
      {
        [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([inline(9, { resolved })])),
        [`POST ${MR_API}/3/discussions/9/resolve`]: () => {
          resolved = true;
          return json(200, inline(9, { resolved: true }));
        },
        [`POST ${MR_API}/3/discussions/9/unresolve`]: () => {
          resolved = false;
          return json(200, inline(9));
        },
      },
      CHANGES,
    );
    const live = screen.getByTestId('mr-announce');
    expect(live.getAttribute('aria-live')).toBe('polite');
    const thread = await screen.findByTestId('thread-9');
    fireEvent.click(within(thread).getByRole('button', { name: 'Resolve thread' }));
    await waitFor(() => {
      expect(live.textContent).toBe('Thread resolved.');
    });
    fireEvent.click(await within(thread).findByRole('button', { name: 'Reopen thread' }));
    await waitFor(() => {
      expect(live.textContent).toBe('Thread reopened.');
    });
  });

  it("a thread's description carries its state and path:line", async () => {
    await open({ [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([inline(9)])) });
    const thread = await screen.findByTestId('thread-9');
    const described = (thread.getAttribute('aria-describedby') ?? '')
      .split(' ')
      .map((id) => document.getElementById(id)?.textContent);
    expect(described).toEqual(['Unresolved', 'src/lib.rs:3']);
  });
});

describe('Changes tab (typescript MEDIUM, design MEDIUM)', () => {
  it('says when the thread list was truncated', async () => {
    await open(
      { [`GET ${MR_API}/3/discussions`]: () => json(200, discussions([inline(9)], true)) },
      CHANGES,
    );
    const notice = await screen.findByTestId('mr-threads-truncated');
    expect(notice.getAttribute('role')).toBe('status');
  });

  it("each line's box has its own sending state", async () => {
    await open(
      { [`POST ${MR_API}/3/discussions`]: () => new Promise<Response>(() => undefined) },
      CHANGES,
    );
    fireEvent.click(await screen.findByRole('button', { name: LINE3 }));
    fireEvent.change(screen.getByLabelText('Your comment: line 3 of src/lib.rs'), {
      target: { value: 'x' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Comment' }));
    await screen.findByRole('button', { name: 'Sending…' });
    fireEvent.click(screen.getByRole('button', { name: 'Comment on line 2 of src/lib.rs' }));
    expect(screen.getByLabelText('Your comment: line 2 of src/lib.rs')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Comment' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Sending…' })).toBeNull();
  });

  it('with unresolved threads first, "next" leads to the first unresolved thread on the Overview', async () => {
    await open(
      {
        [`GET ${MR_API}/3/status`]: () => json(200, UNRESOLVED_1),
        [`GET ${MR_API}/3/discussions`]: () =>
          json(200, discussions([discussion(1, { resolved: true }), discussion(2)])),
      },
      CHANGES,
    );
    fireEvent.click(await screen.findByRole('link', { name: 'Open the first unresolved thread' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe(VIEW);
    });
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByTestId('thread-2'));
    });
  });

  it('when the threads cannot be read, "next" leads to their error', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () => json(200, UNRESOLVED_1),
      [`GET ${MR_API}/3/discussions`]: () => apiError(500, 'スレッドを読めません', 'req-d-2'),
    });
    await screen.findByText('スレッドを読めません');
    fireEvent.click(await screen.findByRole('link', { name: 'Open the first unresolved thread' }));
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByRole('heading', { level: 2, name: 'Threads' }));
    });
  });
});

describe('status hints and tones (design MEDIUM)', () => {
  it.each([
    ['approvals_missing', 'rule=@protected_branch required=1 counted=0', 'Waiting for reviewers to approve.', 'blocked'],
    ['required_check_missing', 'name=ci/gates', 'Waiting for the check result.', 'blocked'],
    ['merge_in_progress', null, 'Waiting for the merge to finish.', 'info'],
    ['draft', null, 'The author takes it out of draft when it is ready.', 'info'],
    ['not_open', null, 'To propose more changes, create a new merge request.', 'info'],
    ['codeowners_invalid', 'line=3 reason=x', 'It cannot be merged until the CODEOWNERS file is fixed.', 'blocked'],
  ])('%s shows its hint "%s" (tone %s)', async (reason, detail, hint, tone) => {
    await open({ [`GET ${MR_API}/3/status`]: () => json(200, status([{ reason, detail }])) });
    expect(await screen.findByText(hint)).toBeTruthy();
    expect((await screen.findByTestId('mr-status')).getAttribute('data-tone')).toBe(tone);
    expect(screen.queryByText('Wait')).toBeNull();
  });

  it('waiting for the check is neutral and has no extra hint', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () =>
        json(200, status([{ reason: 'mergeability_unknown', detail: null }])),
    });
    const line = await screen.findByTestId('mr-status');
    await waitFor(() => {
      expect(line.textContent).toBe('Waiting for the check. A push updates it');
    });
    expect(line.getAttribute('data-tone')).toBe('info');
  });

  it('blocked is "blocked", mergeable "ready"', async () => {
    await open({
      [`GET ${MR_API}/3/status`]: () => json(200, status([{ reason: 'conflict', detail: null }])),
    });
    await waitFor(async () => {
      expect((await screen.findByTestId('mr-status')).getAttribute('data-tone')).toBe('blocked');
    });
  });
});

describe('badges, order and empty state (design MEDIUM / LOW)', () => {
  it('every list item has a state badge, open included; dates carry the exact time', async () => {
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}`]: () =>
          json(200, { merge_requests: [mr(4, { state: 'merged' }), mr(3)], next_before: null }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pulls?state=all`, lang: 'en' });
    const items = within(await screen.findByTestId('mr-list')).getAllByRole('listitem');
    expect(items.map((li) => li.querySelector('[data-badge]')?.textContent)).toEqual(['Merged', 'Open']);
    expect(items[0]?.querySelector('[title]')?.getAttribute('title')).toMatch(/2026/);
  });

  it('unresolved threads come first; resolved ones are folded behind a count', async () => {
    await open({
      [`GET ${MR_API}/3/discussions`]: () =>
        json(
          200,
          discussions([
            discussion(1, { resolved: true }),
            discussion(2),
            discussion(3, { resolved: true }),
            discussion(4),
          ]),
        ),
    });
    await screen.findByTestId('thread-4');
    const order = [...document.querySelectorAll('[data-testid^="thread-"]')].map((e) =>
      e.getAttribute('data-testid'),
    );
    expect(order).toEqual(['thread-2', 'thread-4', 'thread-1', 'thread-3']);
    const folded = screen.getByText('Resolved threads: 2').closest('details');
    expect(folded?.open).toBe(false);
    expect(folded?.contains(screen.getByTestId('thread-1'))).toBe(true);
  });

  it('the empty open list is one sentence and the create button; other filters only say empty', async () => {
    const server = mockServer(
      mrRoutes({ [`GET ${MR_API}`]: () => json(200, { merge_requests: [], next_before: null }) }),
    );
    await renderApp({ fetch: server.fetch, path: `${MR_SCREENS}/pulls`, lang: 'en' });
    const empty = await screen.findByTestId('mr-empty');
    expect(within(empty).getByText('There are no open merge requests.')).toBeTruthy();
    expect(within(empty).getByRole('link', { name: 'New merge request' })).toBeTruthy();
    fireEvent.click(screen.getByRole('link', { name: 'Merged' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toContain('state=merged');
    });
    const other = await screen.findByTestId('mr-empty');
    expect(within(other).getByText('There are no merge requests here.')).toBeTruthy();
    expect(within(other).queryByRole('link')).toBeNull();
  });
});
