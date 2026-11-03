// The merge request list and the create screen (U1 phase 2): newest first,
// the state filter, `next_before` paging, the empty / error states, and
// creating one from the repository's branches.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer, SESSION } from './mock';
import { MR_API, MR_SCREENS, mr, mrRoutes, status } from './mr-fixtures';
import { json, renderApp } from './render';

const LIST = `${MR_SCREENS}/pulls`;
const NEW = `${MR_SCREENS}/pulls/new`;

const list = (items: unknown[], next_before: number | null = null) => () =>
  json(200, { merge_requests: items, next_before });

describe('merge request list', () => {
  it('lists newest first with the author name, links each one and marks the MR nav item', async () => {
    const server = mockServer(
      mrRoutes({ [`GET ${MR_API}`]: list([mr(5), mr(3, { title: 'Older one' })]) }),
    );
    await renderApp({ fetch: server.fetch, path: LIST, lang: 'en' });
    expect(await screen.findByRole('heading', { level: 1, name: 'Merge requests' })).toBeTruthy();
    const items = within(await screen.findByTestId('mr-list')).getAllByRole('listitem');
    expect(items.map((li) => within(li).getAllByRole('link')[0]?.textContent)).toEqual([
      'Synthetic change 5',
      'Older one',
    ]);
    expect(within(items[0] as HTMLElement).getByText(/佐藤 花子/)).toBeTruthy();
    expect(
      within(items[1] as HTMLElement).getByRole('link', { name: 'Older one' }).getAttribute('href'),
    ).toBe(`${MR_SCREENS}/pull/3`);
    const nav = screen.getByRole('navigation', { name: 'Repository screens' });
    expect(within(nav).getByRole('link', { name: 'MR' }).getAttribute('aria-current')).toBe('page');
    // The default filter is "open", asked for explicitly.
    expect(server.to('GET', MR_API)[0]?.query.get('state')).toBe('open');
    expect(server.unmatched).toEqual([]);
  });

  it('newest first even when a page arrives out of order', async () => {
    const server = mockServer(mrRoutes({ [`GET ${MR_API}`]: list([mr(2), mr(9), mr(4)]) }));
    await renderApp({ fetch: server.fetch, path: LIST, lang: 'en' });
    const items = within(await screen.findByTestId('mr-list')).getAllByRole('listitem');
    expect(items.map((li) => within(li).getAllByRole('link')[0]?.textContent)).toEqual([
      'Synthetic change 9',
      'Synthetic change 4',
      'Synthetic change 2',
    ]);
  });

  it.each([
    ['Merged', 'merged'],
    ['Closed', 'closed'],
    ['All', 'all'],
    ['Open', 'open'],
  ])('the "%s" filter asks for state=%s and keeps it in the address', async (label, state) => {
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}`]: (c) =>
          json(200, {
            merge_requests: [mr(3, { title: `state ${c.query.get('state') ?? ''}`, state: 'open' })],
            next_before: null,
          }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: `${LIST}?state=closed`, lang: 'en' });
    await screen.findByText('state closed');
    const filter = screen.getByRole('navigation', { name: 'Show merge requests' });
    expect(within(filter).getByRole('link', { name: 'Closed' }).getAttribute('aria-current')).toBe(
      'page',
    );
    fireEvent.click(within(filter).getByRole('link', { name: label }));
    expect(await screen.findByText(`state ${state}`)).toBeTruthy();
    expect(screen.getByTestId('location').textContent).toBe(`${LIST}?state=${state}`);
  });

  it('pages with next_before and goes back to the newest', async () => {
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}`]: (c) =>
          c.query.get('before') === '3'
            ? json(200, { merge_requests: [mr(2), mr(1)], next_before: null })
            : json(200, { merge_requests: [mr(4), mr(3)], next_before: 3 }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: LIST, lang: 'en' });
    await screen.findByText('Synthetic change 4');
    expect(screen.queryByRole('link', { name: 'Back to the newest' })).toBeNull();
    fireEvent.click(screen.getByRole('link', { name: 'Older merge requests' }));
    expect(await screen.findByText('Synthetic change 1')).toBeTruthy();
    expect(screen.queryByText('Synthetic change 4')).toBeNull();
    const calls = server.to('GET', MR_API);
    expect(calls.at(-1)?.query.get('before')).toBe('3');
    expect(calls.at(-1)?.query.get('state')).toBe('open');
    // The last page has no "older" link.
    expect(screen.queryByRole('link', { name: 'Older merge requests' })).toBeNull();
    fireEvent.click(screen.getByRole('link', { name: 'Back to the newest' }));
    expect(await screen.findByText('Synthetic change 4')).toBeTruthy();
  });

  it('empty: says so and leads to creating one', async () => {
    const server = mockServer(mrRoutes({ [`GET ${MR_API}`]: list([]) }));
    await renderApp({ fetch: server.fetch, path: LIST, lang: 'en' });
    const empty = await screen.findByTestId('mr-empty');
    expect(within(empty).getByText('There are no merge requests here.')).toBeTruthy();
    expect(
      within(empty).getByRole('link', { name: 'New merge request' }).getAttribute('href'),
    ).toBe(NEW);
  });

  it('a failed load shows the user_message and request_id; retry reads again', async () => {
    let fail = true;
    const server = mockServer(
      mrRoutes({
        [`GET ${MR_API}`]: () =>
          fail ? apiError(500, '一時的に読めません', 'req-mr-1') : list([mr(3)])(),
      }),
    );
    await renderApp({ fetch: server.fetch, path: LIST, lang: 'en' });
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('一時的に読めません')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-mr-1');
    fail = false;
    fireEvent.click(within(alert).getByRole('button', { name: 'Try again' }));
    expect(await screen.findByText('Synthetic change 3')).toBeTruthy();
  });
});

describe('create a merge request', () => {
  async function openForm(over: Parameters<typeof mrRoutes>[0] = {}) {
    const server = mockServer(mrRoutes(over));
    await renderApp({ fetch: server.fetch, path: NEW, lang: 'en' });
    expect(
      await screen.findByRole('heading', { level: 1, name: 'New merge request' }),
    ).toBeTruthy();
    // The branch choices arrive with the refs.
    await screen.findAllByRole('option', { name: 'dev' });
    return server;
  }

  const source = () => screen.getByLabelText<HTMLSelectElement>('Branch with the changes');
  const target = () => screen.getByLabelText<HTMLSelectElement>('Branch to merge into');
  const title = () => screen.getByLabelText<HTMLInputElement>('Title');

  it('offers the branches (not tags), defaults the target to the default branch', async () => {
    await openForm();
    const sourceNames = within(source())
      .getAllByRole('option')
      .map((o) => (o as HTMLOptionElement).value);
    expect(sourceNames).toEqual(['', 'main', 'dev']);
    expect(target().value).toBe('main');
    expect(source().value).toBe('');
  });

  it('posts with the CSRF header and goes to the new merge request', async () => {
    const server = await openForm({
      [`POST ${MR_API}`]: () => json(201, mr(12, { title: 'Add a pump spec' })),
      [`GET ${MR_API}/12`]: () => json(200, mr(12, { title: 'Add a pump spec' })),
      [`GET ${MR_API}/12/status`]: () => json(200, { ...status([]), iid: 12 }),
      [`GET ${MR_API}/12/discussions`]: () => json(200, { iid: 12, discussions: [], truncated: false }),
    });
    fireEvent.change(source(), { target: { value: 'dev' } });
    fireEvent.change(title(), { target: { value: '  Add a pump spec ' } });
    fireEvent.click(screen.getByRole('button', { name: 'Create merge request' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe(`${MR_SCREENS}/pull/12`);
    });
    const post = server.to('POST', MR_API)[0];
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    expect(post?.body).toEqual({
      source_branch: 'dev',
      target_branch: 'main',
      title: 'Add a pump spec',
    });
  });

  it('sends the description when one is written', async () => {
    const server = await openForm({ [`POST ${MR_API}`]: () => json(201, mr(12)) });
    fireEvent.change(source(), { target: { value: 'dev' } });
    fireEvent.change(title(), { target: { value: 'T' } });
    fireEvent.change(screen.getByLabelText('Description (optional, Markdown)'), {
      target: { value: 'Why: the pump changed.' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Create merge request' }));
    await waitFor(() => {
      expect(server.to('POST', MR_API)).toHaveLength(1);
    });
    expect(server.to('POST', MR_API)[0]?.body).toMatchObject({
      description: 'Why: the pump changed.',
    });
  });

  it('a 422 shows the server user_message and keeps the form', async () => {
    const server = await openForm({
      [`POST ${MR_API}`]: () =>
        apiError(422, '同じブランチの変更依頼が既に開いています。', 'req-422', 'FG-REVIEW-0002'),
    });
    fireEvent.change(source(), { target: { value: 'dev' } });
    fireEvent.change(title(), { target: { value: 'Typed title' } });
    fireEvent.click(screen.getByRole('button', { name: 'Create merge request' }));
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByText('同じブランチの変更依頼が既に開いています。')).toBeTruthy();
    expect(within(alert).getByTestId('request-id').textContent).toBe('req-422');
    expect(title().value).toBe('Typed title');
    expect(screen.getByTestId('location').textContent).toBe(NEW);
    expect(server.to('POST', MR_API)).toHaveLength(1);
  });

  it('refuses the same source and target before sending', async () => {
    const server = await openForm();
    fireEvent.change(source(), { target: { value: 'main' } });
    fireEvent.change(title(), { target: { value: 'T' } });
    fireEvent.click(screen.getByRole('button', { name: 'Create merge request' }));
    const summary = await screen.findByRole('alert');
    expect(within(summary).getByText(/The two branches are the same/)).toBeTruthy();
    expect(source().getAttribute('aria-invalid')).toBe('true');
    expect(server.to('POST', MR_API)).toHaveLength(0);
  });

  it('refuses a missing branch or title before sending', async () => {
    const server = await openForm();
    fireEvent.click(screen.getByRole('button', { name: 'Create merge request' }));
    const summary = await screen.findByRole('alert');
    expect(within(summary).getByText('Choose the branch with the changes.')).toBeTruthy();
    expect(within(summary).getByText('Enter a title.')).toBeTruthy();
    expect(server.to('POST', MR_API)).toHaveLength(0);
  });

  it('refuses a title over 255 characters before sending', async () => {
    const server = await openForm();
    fireEvent.change(source(), { target: { value: 'dev' } });
    fireEvent.change(title(), { target: { value: '題'.repeat(256) } });
    fireEvent.click(screen.getByRole('button', { name: 'Create merge request' }));
    const summary = await screen.findByRole('alert');
    expect(within(summary).getByText('The title can be at most 255 characters.')).toBeTruthy();
    expect(server.to('POST', MR_API)).toHaveLength(0);
  });

  it('a repository with no branches says to push first', async () => {
    const server = mockServer(
      mrRoutes({
        'GET /api/v1/projects/7/repository/refs': () =>
          json(200, { default_branch: null, refs: [], truncated: false }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: NEW, lang: 'en' });
    expect(
      await screen.findByText('This repository has no branches yet. Push a branch first.'),
    ).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Create merge request' })).toBeNull();
  });

  it('the list links to the create screen', async () => {
    const server = mockServer(mrRoutes());
    await renderApp({ fetch: server.fetch, path: LIST, lang: 'en' });
    await screen.findByText('Synthetic change 3');
    expect(screen.getByRole('link', { name: 'New merge request' }).getAttribute('href')).toBe(NEW);
  });

  it('an unknown word after pulls is not found', async () => {
    const server = mockServer(mrRoutes());
    await renderApp({ fetch: server.fetch, path: `${LIST}/whatever`, lang: 'en' });
    expect(await screen.findByRole('heading', { level: 1, name: 'Page not found' })).toBeTruthy();
    expect(server.to('GET', MR_API)).toHaveLength(0);
  });
});
