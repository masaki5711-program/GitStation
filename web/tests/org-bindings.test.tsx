// ADR-0038 §2: the members screen lists the people an org binding brings in
// (「組織から（…）」, no remove button) and lets the Owner add a binding (a
// unit picked from the org master, descendants, membership, minimum rank,
// role up to Developer) and remove one after asking. Synthetic codes only.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { mockServer, PROJECT, RESOLVE_GROUP, RESOLVE_PROJECT, SESSION, signedIn, GROUP } from './mock';
import { json, renderApp } from './render';

const BASE = '/acme/forge-web/-';

const member = (id: number, name: string, role: string) => ({
  user_id: id,
  username: name,
  display_name: name.toUpperCase(),
  role,
  active: true,
  created_at: '2026-10-01T00:00:00Z',
});

const unit = { level: 'section', name_ja: '品質保証課', name_en: 'Quality Assurance', name_id: null };

const binding = {
  id: 11,
  plant_code: 'P001',
  org_code: 'SEC-1',
  include_descendants: true,
  membership: 'any',
  min_rank: null,
  role: 'developer',
  reason: 'synthetic',
  created_by: { id: 5, username: 'taro' },
  created_at: '2026-11-20T00:00:00Z',
  unit,
};

const bound = (canManage: boolean, bindings: unknown[] = [binding]) => ({
  bindings,
  members:
    bindings.length === 0
      ? []
      : [{ user_id: 21, username: 'jiro', display_name: 'JIRO', role: 'developer', binding_id: 11, org_code: 'SEC-1', unit }],
  members_truncated: false,
  viewer: { can_manage: canManage },
});

const projectRoutes = () => ({
  ...signedIn(),
  'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_PROJECT),
  'GET /api/v1/projects/7': () => json(200, PROJECT),
  'GET /api/v1/projects/7/members': () => json(200, { members: [member(5, 'taro', 'owner')], next_after: null }),
});

describe('org bindings on the members screen', () => {
  it('lists the people from a binding apart, without a remove button', async () => {
    const server = mockServer({
      ...projectRoutes(),
      'GET /api/v1/projects/7/org-bindings': () => json(200, bound(false)),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/members`, lang: 'ja' });
    await screen.findByText('組織から（品質保証課）');
    const regions = screen.getAllByRole('region', { name: '組織から入っている人' });
    const region = regions[regions.length - 1] as HTMLElement;
    expect(within(region).getByText('jiro')).toBeTruthy();
    expect(within(region).getByText('組織から（品質保証課）')).toBeTruthy();
    expect(within(region).queryByRole('button')).toBeNull();
    // Not the Owner: the bindings are shown, not changed.
    expect(screen.getByText('結び付けを変えられるのは Owner と管理者だけです。')).toBeTruthy();
    expect(screen.queryByRole('button', { name: '結び付ける' })).toBeNull();
    expect(server.unmatched).toEqual([]);
  });

  it('lets the Owner bind a picked unit with the CSRF header and remove one after asking', async () => {
    let bindings: unknown[] = [];
    const server = mockServer({
      ...projectRoutes(),
      'GET /api/v1/projects/7/org-bindings': () => json(200, bound(true, bindings)),
      'GET /api/v1/org-units': () =>
        json(200, {
          items: [
            {
              plant_code: 'P001',
              unit: { code: 'SEC-1', level: 'section', name_ja: '品質保証課', name_en: 'Quality Assurance', name_id: null },
              parent_code: 'DEP-1',
            },
          ],
        }),
      'POST /api/v1/projects/7/org-bindings': () => {
        bindings = [binding];
        return json(201, binding);
      },
      'DELETE /api/v1/projects/7/org-bindings/11': () => {
        bindings = [];
        return new Response(null, { status: 204 });
      },
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/members`, lang: 'en' });
    expect(await screen.findAllByText('No organisation is bound.', {}, { timeout: 3000 })).toHaveLength(2);
    // Binding without a unit is refused before sending.
    fireEvent.click(screen.getByRole('button', { name: 'Bind' }));
    expect(await screen.findByText('Find and pick a unit first.')).toBeTruthy();
    expect(server.to('POST', '/api/v1/projects/7/org-bindings')).toHaveLength(0);

    fireEvent.change(screen.getByLabelText('Find a unit'), { target: { value: 'quality' } });
    const searches = screen.getAllByRole('button', { name: 'Search' });
    fireEvent.click(searches[searches.length - 1] as HTMLElement);
    fireEvent.click(await screen.findByRole('radio', { name: /Quality Assurance/ }));
    expect(server.to('GET', '/api/v1/org-units')[0]?.query.get('search')).toBe('quality');
    fireEvent.change(screen.getByLabelText('Lowest position rank'), { target: { value: '55' } });
    const roles = screen.getAllByLabelText('Role');
    fireEvent.change(roles[roles.length - 1] as HTMLElement, { target: { value: 'developer' } });
    // A binding never offers more than Developer.
    const options = within(roles[roles.length - 1] as HTMLElement)
      .getAllByRole('option')
      .map((o) => (o as HTMLOptionElement).value);
    expect(options).toEqual(['reporter', 'developer']);
    fireEvent.click(screen.getByRole('button', { name: 'Bind' }));
    await waitFor(() => {
      expect(server.to('POST', '/api/v1/projects/7/org-bindings')).toHaveLength(1);
    });
    const post = server.to('POST', '/api/v1/projects/7/org-bindings')[0];
    expect(post?.body).toEqual({
      plant_code: 'P001',
      org_code: 'SEC-1',
      include_descendants: true,
      membership: 'any',
      min_rank: 55,
      role: 'developer',
      reason: null,
    });
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    expect(await screen.findByText('Bound Quality Assurance.')).toBeTruthy();

    // Removing asks first.
    fireEvent.click(await screen.findByRole('button', { name: 'Remove the binding of Quality Assurance' }));
    const dialog = screen.getByRole('dialog');
    expect(server.to('DELETE', '/api/v1/projects/7/org-bindings/11')).toHaveLength(0);
    fireEvent.click(within(dialog).getByRole('button', { name: 'Remove' }));
    await waitFor(() => {
      expect(server.to('DELETE', '/api/v1/projects/7/org-bindings/11')).toHaveLength(1);
    });
  });

  it('a rank outside 0..999 is refused before sending', async () => {
    const server = mockServer({
      ...projectRoutes(),
      'GET /api/v1/projects/7/org-bindings': () => json(200, bound(true, [])),
    });
    await renderApp({ fetch: server.fetch, path: `${BASE}/members`, lang: 'en' });
    fireEvent.change(await screen.findByLabelText('Lowest position rank'), { target: { value: '1000' } });
    expect(await screen.findByText('Enter a number from 0 to 999.')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Bind' }));
    expect(server.to('POST', '/api/v1/projects/7/org-bindings')).toHaveLength(0);
  });

  it('a group uses the group endpoints', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_GROUP),
      'GET /api/v1/groups/3': () => json(200, GROUP),
      'GET /api/v1/groups/3/members': () => json(200, { members: [member(5, 'taro', 'owner')], next_after: null }),
      'GET /api/v1/groups/3/org-bindings': () => json(200, bound(false)),
    });
    await renderApp({ fetch: server.fetch, path: '/acme/-/members', lang: 'en' });
    expect(await screen.findByText('From the organisation (Quality Assurance)')).toBeTruthy();
    expect(server.unmatched).toEqual([]);
  });
});
