// M4 C10: "Revert this merge" on a merged merge request. Offered only when
// the status says the person may (viewer.can_revert) and the merge request
// is merged; one press POSTs …/revert and then one line names the revert
// merge request with a link to it. A refusal shows user_message and the
// request id, never the error code.
import { fireEvent, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer } from './mock';
import { MR_API, MR_SCREENS, mr, mrRoutes, status } from './mr-fixtures';
import { json, renderApp } from './render';

const VIEW = `${MR_SCREENS}/pull/3`;

const withRevert = (can_revert: boolean) => {
  const s = status([]);
  return { ...s, viewer: { ...s.viewer, can_revert } };
};

async function open(state: string, can_revert: boolean, routes: Parameters<typeof mrRoutes>[0] = {}) {
  const server = mockServer(
    mrRoutes({
      [`GET ${MR_API}/3`]: () => json(200, mr(3, { state })),
      [`GET ${MR_API}/3/status`]: () => json(200, withRevert(can_revert)),
      ...routes,
    }),
  );
  const view = await renderApp({ fetch: server.fetch, path: VIEW, lang: 'en' });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return { server, ...view };
}

describe('revert this merge', () => {
  it('is offered on a merged merge request the person may revert', async () => {
    await open('merged', true);
    expect(await screen.findByRole('heading', { name: 'Revert' })).toBeTruthy();
    expect(screen.getByText(/does not change until that merge request is merged/)).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Revert this merge' })).toBeTruthy();
  });

  it('is absent without can_revert, and on an open merge request', async () => {
    await open('merged', false);
    await screen.findByRole('region', { name: 'Merge conditions' });
    expect(screen.queryByTestId('mr-revert')).toBeNull();
  });

  it('is absent on an open merge request even with can_revert', async () => {
    await open('open', true);
    await screen.findByRole('region', { name: 'Merge conditions' });
    expect(screen.queryByTestId('mr-revert')).toBeNull();
  });

  it('one press opens the revert and names it with a link', async () => {
    const { server } = await open('merged', true, {
      [`POST ${MR_API}/3/revert`]: () =>
        json(201, {
          merge_request: mr(4, { source_branch: 'revert/mr-3', title: 'Revert "Synthetic change 3"' }),
          reverted_iid: 3,
          commit_sha: 'c'.repeat(40),
          parent_sha: 'd'.repeat(40),
        }),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Revert this merge' }));
    const done = await screen.findByTestId('mr-revert-opened');
    expect(done.textContent).toContain('Revert merge request !4 opened.');
    expect(screen.getByRole('link', { name: 'Open it' }).getAttribute('href')).toBe(`${MR_SCREENS}/pull/4`);
    expect(server.to('POST', `${MR_API}/3/revert`)).toHaveLength(1);
  });

  it('a refusal shows the message and the request id, never the code', async () => {
    await open('merged', true, {
      [`POST ${MR_API}/3/revert`]: () =>
        apiError(409, 'Reverting this merge conflicts with changes made after it.', 'req-revert-1', 'FG-REVIEW-0030'),
    });
    fireEvent.click(await screen.findByRole('button', { name: 'Revert this merge' }));
    expect(await screen.findByText(/conflicts with changes made after it/)).toBeTruthy();
    expect(screen.getByText(/req-revert-1/)).toBeTruthy();
    expect(screen.queryByText(/FG-REVIEW-0030/)).toBeNull();
  });
});
