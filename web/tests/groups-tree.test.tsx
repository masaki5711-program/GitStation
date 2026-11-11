// Plan 15 MR 4: /-/groups as a nested list with the create dialogs.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { buildGroupTree, mayCreateProject, mayCreateSubgroup } from '../src/features/groupTreeModel';
import { LISTED_GROUP, apiError, mockServer, page, signedIn, signedInAdmin, type Handler } from './mock';
import { json, renderApp } from './render';

const g = (id: number, path: string, display_name: string, role: string | null) => ({
  ...LISTED_GROUP,
  id,
  namespace_id: id * 10,
  path,
  name: path.split('/').pop() ?? path,
  display_name,
  role,
});

const ACME = g(3, 'acme', 'Acme Plant', 'owner');
const PLANT = g(4, 'acme/plant', 'Plant', 'owner');
const LINE = g(5, 'acme/plant/line1', 'Line 1', 'owner');
const DEV = g(6, 'dev-team', 'Dev Team', 'developer');
const OPS = g(7, 'ops', 'Ops', 'maintainer');
const OPEN = g(8, 'public', 'Public', null);
const ALL = [LINE, OPS, ACME, DEV, PLANT, OPEN];

const routes = (over: Record<string, Handler> = {}, base = signedIn()) => ({
  ...base,
  'GET /api/v1/groups': () => page(ALL),
  ...over,
});

const toggle = (name: string) => screen.getByRole('button', { name: `${name} のサブグループ` });

describe('the tree model', () => {
  it('nests by path, sorts by path, keeps a row whose parent is missing at the top', () => {
    const roots = buildGroupTree([PLANT, g(9, 'gone/child', 'Child', null), ACME, LINE]);
    expect(roots.map((n) => [n.group.path, n.orphan])).toEqual([
      ['acme', false],
      ['gone/child', true],
    ]);
    expect(roots[0]?.children[0]?.group.path).toBe('acme/plant');
    expect(roots[0]?.children[0]?.children[0]?.group.path).toBe('acme/plant/line1');
  });

  it('decides the buttons from the role (admin may always)', () => {
    expect(mayCreateSubgroup('owner', false)).toBe(true);
    expect(mayCreateSubgroup('maintainer', false)).toBe(false);
    expect(mayCreateSubgroup(null, true)).toBe(true);
    expect(mayCreateProject('maintainer', false)).toBe(true);
    expect(mayCreateProject('developer', false)).toBe(false);
    expect(mayCreateProject(null, false)).toBe(false);
    expect(mayCreateProject('guest', true)).toBe(true);
  });
});

describe('/-/groups tree', () => {
  it('shows the top level, opens and closes a level with the disclosure button', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/-/groups' });
    expect(await screen.findByRole('link', { name: 'Acme Plant' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Ops' })).toBeTruthy();
    // Children are hidden until opened.
    expect(screen.queryByRole('link', { name: 'Plant' })).toBeNull();
    // Acme and (hidden) Plant each have one subgroup.
    expect(screen.getAllByText('サブグループ 1 件')).toHaveLength(2);

    const acme = toggle('Acme Plant');
    expect(acme.getAttribute('aria-expanded')).toBe('false');
    const controlled = acme.getAttribute('aria-controls');
    expect(controlled !== null && document.getElementById(controlled) !== null).toBe(true);
    fireEvent.click(acme);
    expect(acme.getAttribute('aria-expanded')).toBe('true');
    expect(screen.getByRole('link', { name: 'Plant' }).getAttribute('href')).toBe('/acme/plant');
    expect(screen.queryByRole('link', { name: 'Line 1' })).toBeNull();

    fireEvent.click(toggle('Plant'));
    expect(screen.getByRole('link', { name: 'Line 1' })).toBeTruthy();
    // A leaf has no disclosure button.
    expect(screen.queryByRole('button', { name: 'Line 1 のサブグループ' })).toBeNull();

    fireEvent.click(acme);
    expect(screen.queryByRole('link', { name: 'Plant' })).toBeNull();

    fireEvent.click(screen.getByRole('button', { name: 'すべて開く' }));
    expect(screen.getByRole('link', { name: 'Line 1' })).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'すべて閉じる' }));
    expect(screen.queryByRole('link', { name: 'Plant' })).toBeNull();
    expect(server.unmatched).toEqual([]);
  });

  it('a non-administrator: no top-level create; subgroup only as Owner, project as Maintainer+', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/-/groups' });
    await screen.findByRole('link', { name: 'Acme Plant' });
    expect(screen.queryByRole('button', { name: 'グループを作る' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Acme Plant にサブグループを作る' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Acme Plant にプロジェクトを作る' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Ops にサブグループを作る' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Ops にプロジェクトを作る' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: /^Dev Team に/ })).toBeNull();
    expect(screen.queryByRole('button', { name: /^Public に/ })).toBeNull();
  });

  it('an administrator: the top-level create and both actions on every group', async () => {
    const server = mockServer(routes({}, signedInAdmin()));
    await renderApp({ fetch: server.fetch, path: '/-/groups' });
    await screen.findByRole('link', { name: 'Public' });
    expect(screen.getByRole('button', { name: 'Public にサブグループを作る' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Dev Team にプロジェクトを作る' })).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'グループを作る' }));
    expect(await screen.findByRole('dialog', { name: 'グループを作る' })).toBeTruthy();
  });

  it('creates a subgroup and goes to it', async () => {
    const created = { ...ACME, id: 12, namespace_id: 120, parent_id: 3, path: 'acme/new-sub', name: 'new-sub', display_name: 'New Sub' };
    const server = mockServer(routes({ 'POST /api/v1/groups/3/subgroups': () => json(201, created) }));
    await renderApp({ fetch: server.fetch, path: '/-/groups' });
    fireEvent.click(await screen.findByRole('button', { name: 'Acme Plant にサブグループを作る' }));
    const d = within(await screen.findByRole('dialog', { name: 'サブグループを作る' }));
    expect(d.getByText('acme')).toBeTruthy();
    fireEvent.change(d.getByLabelText('名前'), { target: { value: 'New Sub' } });
    fireEvent.click(d.getByRole('button', { name: 'グループを作る' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe('/acme/new-sub');
    });
    expect(server.to('POST', '/api/v1/groups/3/subgroups')[0]?.body).toEqual({
      display_name: 'New Sub',
      path: 'new-sub',
      description: null,
    });
  });

  it('creates a project in the group and goes to it', async () => {
    const created = {
      id: 21,
      namespace_id: 70,
      path: 'ops/new-proj',
      name: 'new-proj',
      display_name: 'New Proj',
      description: '',
      visibility: 'private',
      created_at: '2026-11-11T00:00:00Z',
      role: 'maintainer',
    };
    const server = mockServer(routes({ 'POST /api/v1/projects': () => json(201, created) }));
    await renderApp({ fetch: server.fetch, path: '/-/groups' });
    fireEvent.click(await screen.findByRole('button', { name: 'Ops にプロジェクトを作る' }));
    const d = within(await screen.findByRole('dialog', { name: 'プロジェクトを作る' }));
    fireEvent.change(d.getByLabelText('名前'), { target: { value: 'New Proj' } });
    fireEvent.click(d.getByRole('button', { name: 'プロジェクトを作る' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe('/ops/new-proj');
    });
    expect(server.to('POST', '/api/v1/projects')[0]?.body).toEqual({
      namespace_id: 70,
      display_name: 'New Proj',
      path: 'new-proj',
      description: null,
      visibility: 'private',
    });
  });

  it('a refused create shows user_message and the request id, never the code; the form stays', async () => {
    const server = mockServer(
      routes({
        'POST /api/v1/groups/3/subgroups': () =>
          apiError(403, 'サブグループを作る権限がありません。', 'req-sub-1', 'FG-AUTH-0003'),
      }),
    );
    const { container } = await renderApp({ fetch: server.fetch, path: '/-/groups' });
    fireEvent.click(await screen.findByRole('button', { name: 'Acme Plant にサブグループを作る' }));
    const dialog = await screen.findByRole('dialog', { name: 'サブグループを作る' });
    const d = within(dialog);
    fireEvent.change(d.getByLabelText('名前'), { target: { value: 'New Sub' } });
    fireEvent.click(d.getByRole('button', { name: 'グループを作る' }));
    const alert = await d.findByRole('alert');
    expect(alert.textContent).toContain('サブグループを作る権限がありません。');
    expect(d.getByTestId('request-id').textContent).toBe('req-sub-1');
    expect(container.textContent).not.toContain('FG-AUTH-0003');
    expect(d.getByLabelText<HTMLInputElement>('名前').value).toBe('New Sub');
    expect(screen.getByTestId('location').textContent).toBe('/-/groups');
  });

  it('a failed list offers a retry that recovers', async () => {
    let calls = 0;
    const server = mockServer(
      routes({
        'GET /api/v1/groups': () => {
          calls += 1;
          return calls === 1 ? apiError(503, 'グループを読めませんでした。', 'req-g-2') : page(ALL);
        },
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/groups' });
    expect((await screen.findByRole('alert')).textContent).toContain('グループを読めませんでした。');
    fireEvent.click(screen.getByRole('button', { name: 'もう一度試す' }));
    expect(await screen.findByRole('link', { name: 'Acme Plant' })).toBeTruthy();
  });

  it('a list cut at the cap says so and keeps a group whose parent was cut', async () => {
    let n = 0;
    const server = mockServer(
      routes({
        'GET /api/v1/groups': () => {
          n += 1;
          // Every page says "more": the reader stops at its cap (10 pages).
          const row = n === 1 ? g(100, 'far/away', 'Far Away', null) : g(100 + n, `top${String(n)}`, `Top ${String(n)}`, null);
          return page([row], { has_more: true, next_cursor: `c${String(n)}` });
        },
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/groups' });
    expect(await screen.findByText(/一部のグループ（サブグループを含む）が表示されていない場合があります/)).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Far Away' })).toBeTruthy();
    expect(screen.getByText('far/away')).toBeTruthy();
  });

  it('axe: no violations with a level open', async () => {
    const server = mockServer(routes());
    const { container } = await renderApp({ fetch: server.fetch, path: '/-/groups' });
    await screen.findByRole('link', { name: 'Acme Plant' });
    fireEvent.click(toggle('Acme Plant'));
    const result = await axe.run(container, {
      runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
      rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
    });
    expect(result.violations.map((v) => v.id)).toEqual([]);
  });
});
