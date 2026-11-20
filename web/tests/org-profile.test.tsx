// ADR-0021 amendment 2026-11-20: the org claims' snapshot on screen — the
// account page and the user menu show the departments (the primary first,
// marked with the text 「主」, not colour alone), positions and plant; the
// people pickers show the primary department code only. Synthetic codes.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { ORG_PROFILE_PATH, mockServer, page, PROJECT, RESOLVE_PROJECT, signedIn } from './mock';
import { json, renderApp } from './render';
import { mergeSettings } from './mr-fixtures';
import { docLintMigration } from './merge-settings-fixtures';

const ORG = {
  org_units: [
    { code: 'SYN-A-0001', primary: true },
    { code: 'SYN-B-0002', primary: false },
  ],
  positions: ['member', 'line_lead'],
  position_rank: 3,
  plants: ['P001'],
  plant_code: 'P001',
  confirmed_at: '2026-11-20T00:00:00Z',
  changed_at: '2026-11-20T00:00:00Z',
  invalid_since: null,
};

const account = (org: unknown) => ({
  ...signedIn(),
  'GET /api/v1/namespaces/resolve': () =>
    json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
  'GET /api/v1/user/tokens': () => json(200, { tokens: [] }),
  [`GET ${ORG_PROFILE_PATH}`]: () => json(200, { user_id: 5, org }),
});

describe('org claims snapshot', () => {
  it('the account page lists the departments with the primary first and marked in text', async () => {
    const server = mockServer(account(ORG));
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings' });
    const section = (await screen.findByRole('heading', { name: '所属' })).closest('section') as HTMLElement;
    await within(section).findByText('SYN-A-0001');
    const items = within(section).getAllByRole('listitem');
    expect(items[0]?.textContent).toBe('主SYN-A-0001');
    expect(items[1]?.textContent).toBe('SYN-B-0002');
    expect(within(section).getByText('member, line_lead')).toBeTruthy();
    expect(within(section).getByText('P001')).toBeTruthy();
    expect(server.unmatched).toEqual([]);
  });

  it('says when there is no snapshot, and when the last claims were invalid', async () => {
    const none = mockServer(account(null));
    await renderApp({ fetch: none.fetch, path: '/taro/-/settings' });
    expect(await screen.findByText(/所属の情報はまだありません/)).toBeTruthy();
  });

  it('marks an invalid snapshot', async () => {
    const server = mockServer(account({ ...ORG, invalid_since: '2026-11-20T01:00:00Z' }));
    await renderApp({ fetch: server.fetch, path: '/taro/-/settings', lang: 'en' });
    expect(await screen.findByText(/the previous ones are shown/)).toBeTruthy();
  });

  it('the user menu shows the departments once it is opened', async () => {
    const server = mockServer({
      ...account(ORG),
      'GET /api/v1/projects': () => page([]),
      'GET /api/v1/groups': () => page([]),
    });
    await renderApp({ fetch: server.fetch });
    const trigger = await screen.findByRole('button', { name: /山田 太郎/ });
    expect(server.to('GET', ORG_PROFILE_PATH)).toHaveLength(0);
    fireEvent.click(trigger);
    const line = await screen.findByLabelText('所属');
    await waitFor(() => {
      expect(line.textContent).toContain('主SYN-A-0001');
    });
    expect(line.textContent).toContain('P001');
  });

  it('the member picker shows the primary department code next to a name', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': () => json(200, { ...RESOLVE_PROJECT }),
      'GET /api/v1/projects/7': () => json(200, PROJECT),
      'GET /api/v1/projects/7/protected-refs': () => json(200, { rules: [] }),
      'GET /api/v1/projects/7/merge-settings': () => json(200, mergeSettings()),
      'GET /api/v1/projects/7/approval-rules': () => json(200, { rules: [] }),
      'GET /api/v1/projects/7/integration-bindings': () => json(200, { items: [] }),
      'GET /api/v1/projects/7/doc-lint/migration': () => json(200, docLintMigration()),
      'GET /api/v1/groups': () => page([]),
      'GET /api/v1/projects': () => page([]),
      'GET /api/v1/projects/7/members': () => json(200, { members: [], next_after: null }),
      'GET /api/v1/users': () =>
        json(200, {
          items: [
            { id: 9, username: 'hanako', display_name: 'HANAKO', primary_org_code: 'SYN-A-0001' },
            { id: 8, username: 'jiro', display_name: 'JIRO', primary_org_code: null },
          ],
        }),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/members', lang: 'en' });
    fireEvent.change(await screen.findByLabelText('Find a user'), { target: { value: 'ha' } });
    fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    const results = await screen.findByRole('list', { name: 'Search results' });
    await within(results).findByText('SYN-A-0001');
    expect(within(results).getAllByText('Department code', { exact: false })).toHaveLength(1);
  });
});
