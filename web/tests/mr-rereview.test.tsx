// U1 re-review: resolve / reopen must announce and move the focus even when
// the thread re-mounts before the mutation settles (the discussions and the
// status refetch land in either order); the arrival focus works under
// StrictMode and never steals the focus from a field.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { mockServer, type Handler } from './mock';
import { MR_API, MR_SCREENS, discussion, discussions, mrRoutes, status } from './mr-fixtures';
import { json, renderApp } from './render';

const VIEW = `${MR_SCREENS}/pull/3`;
const later = (ms: number, answer: () => Response): Promise<Response> =>
  new Promise((resolve) => {
    setTimeout(() => {
      resolve(answer());
    }, ms);
  });

/** Routes where, once the thread changed, one of the two refetches is slow. */
function racing(slow: 'status' | 'discussions', startResolved: boolean): Record<string, Handler> {
  let resolved = startResolved;
  let changed = false;
  const threads = () => json(200, discussions([discussion(2, { resolved })]));
  const st = () =>
    json(200, resolved ? status([]) : status([{ reason: 'unresolved_discussions', detail: 'count=1' }]));
  return {
    [`GET ${MR_API}/3/discussions`]: () =>
      changed && slow === 'discussions' ? later(150, threads) : threads(),
    [`GET ${MR_API}/3/status`]: () => (changed && slow === 'status' ? later(150, st) : st()),
    [`POST ${MR_API}/3/discussions/2/resolve`]: () => {
      resolved = true;
      changed = true;
      return json(200, discussion(2, { resolved: true }));
    },
    [`POST ${MR_API}/3/discussions/2/unresolve`]: () => {
      resolved = false;
      changed = true;
      return json(200, discussion(2));
    },
  };
}

async function open(over: Record<string, Handler>, options: { strict?: boolean; state?: unknown } = {}) {
  const server = mockServer(mrRoutes(over));
  await renderApp({ fetch: server.fetch, path: VIEW, lang: 'en', ...options });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return server;
}

describe('resolve / reopen when the refetches race (HIGH)', () => {
  it.each(['status', 'discussions'] as const)(
    'resolve with the %s refetch late: announced, focus on the resolved summary',
    async (slow) => {
      await open(racing(slow, false));
      const thread = await screen.findByTestId('thread-2');
      fireEvent.click(within(thread).getByRole('button', { name: 'Resolve thread' }));
      await waitFor(() => {
        expect(screen.getByTestId('mr-announce').textContent).toBe('Thread resolved.');
      });
      await waitFor(() => {
        expect(document.activeElement?.textContent).toBe('Resolved threads: 1');
      });
    },
  );

  it.each(['status', 'discussions'] as const)(
    'reopen with the %s refetch late: announced, focus on the thread',
    async (slow) => {
      await open(racing(slow, true));
      fireEvent.click(await screen.findByText('Resolved threads: 1'));
      fireEvent.click(
        within(screen.getByTestId('thread-2')).getByRole('button', { name: 'Reopen thread' }),
      );
      await waitFor(() => {
        expect(screen.getByTestId('mr-announce').textContent).toBe('Thread reopened.');
      });
      await waitFor(() => {
        expect(document.activeElement).toBe(screen.getByTestId('thread-2'));
      });
    },
  );
});

describe('arrival focus (MEDIUM / LOW)', () => {
  const unresolved = {
    [`GET ${MR_API}/3/status`]: () =>
      json(200, status([{ reason: 'unresolved_discussions', detail: 'count=1' }])),
    [`GET ${MR_API}/3/discussions`]: () =>
      json(200, discussions([discussion(1, { resolved: true }), discussion(2)])),
  };

  it('under StrictMode the link from Changes still focuses the first unresolved thread', async () => {
    const server = mockServer(mrRoutes(unresolved));
    await renderApp({ fetch: server.fetch, path: `${VIEW}/changes`, lang: 'en', strict: true });
    fireEvent.click(await screen.findByRole('link', { name: 'Open the first unresolved thread' }));
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByTestId('thread-2'));
    });
  });

  it('a late thread load does not take the focus from a field', async () => {
    await open(
      {
        ...unresolved,
        [`GET ${MR_API}/3/discussions`]: () =>
          later(200, () => json(200, discussions([discussion(2)]))),
      },
      { state: { focus: 'first-unresolved' } },
    );
    const box = screen.getByLabelText('Add a comment');
    box.focus();
    await screen.findByTestId('thread-2');
    await new Promise((r) => setTimeout(r, 50));
    expect(document.activeElement).toBe(box);
  });

  it('without a field in use, the arrival still focuses the thread', async () => {
    await open(unresolved, { state: { focus: 'first-unresolved' }, strict: true });
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByTestId('thread-2'));
    });
  });
});
