// Performance: prefetch on intent, the immutable cache of commit-addressed
// queries, the namespace resolver cache shared by a project's tabs, and the
// drop of that cache after a transfer or a deletion.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { IMMUTABLE } from '../src/api/cachePolicy';
import { createApiClient } from '../src/api/client';
import { mergeRequestsQuery } from '../src/api/mergeRequests';
import { forgetResolutions, resolveKeys, resolveQuery, watchSessionUser } from '../src/api/namespaces';
import { blobQuery, commitQuery, compareQuery, treeQuery } from '../src/api/repo';
import { createQueryClient } from '../src/App';
import { connectionIsSlow, MAX_FLIGHTS, prefetchRoute } from '../src/features/prefetch';
import { mockServer, PROJECT, RESOLVE_PROJECT, SESSION, signedIn, page } from './mock';
import { json, renderApp } from './render';
import { C1, repoRoutes } from './repo-fixtures';

const RESOLVE = '/api/v1/namespaces/resolve';
const COMMITS = '/api/v1/projects/7/repository/commits';
const TREE = '/api/v1/projects/7/repository/tree';
const MRS = '/api/v1/projects/7/merge-requests';

const routes = () =>
  repoRoutes({
    [`GET ${MRS}`]: () => json(200, { merge_requests: [], next_before: null }),
  });

function link(href: string): HTMLElement {
  const a = document.querySelector<HTMLElement>(`a[href="${href}"]`);
  if (a === null) throw new Error(`no link to ${href}`);
  return a;
}

/** A query client and API client over the mock server, as the app makes them. */
function clients(fetch: typeof globalThis.fetch) {
  const queryClient = createQueryClient();
  queryClient.setDefaultOptions({ queries: { ...queryClient.getDefaultOptions().queries, retry: false } });
  const api = createApiClient({ language: () => 'en', fetch });
  return { queryClient, api };
}

afterEach(() => {
  vi.useRealTimers();
  Reflect.deleteProperty(navigator, 'connection');
});

describe('prefetch on intent', () => {
  it('hovering or focusing a sidebar item reads the target screen once', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/tree', lang: 'en', appStaleTime: true });
    await screen.findByRole('link', { name: 'src/' });
    expect(server.to('GET', COMMITS)).toHaveLength(0);
    const commits = link('/acme/forge-web/-/commits');
    fireEvent.mouseEnter(commits);
    fireEvent.focus(commits);
    fireEvent.mouseLeave(commits);
    fireEvent.mouseEnter(commits);
    await waitFor(() => {
      expect(server.to('GET', COMMITS)).toHaveLength(1);
    });
    expect(server.to('GET', COMMITS)[0]?.query.get('ref')).toBe('refs/heads/main');
    // The click then renders from the cache: no second request.
    fireEvent.click(commits);
    expect(await screen.findByText('Add the add function')).toBeTruthy();
    expect(server.to('GET', COMMITS)).toHaveLength(1);
    // One resolution of the project's path for both tabs and the prefetch.
    expect(server.to('GET', RESOLVE)).toHaveLength(1);
  });

  it('a pointer that only passes over a link (shorter than the delay) asks nothing', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/tree', lang: 'en', appStaleTime: true });
    await screen.findByRole('link', { name: 'src/' });
    const commits = link('/acme/forge-web/-/commits');
    fireEvent.mouseEnter(commits);
    fireEvent.mouseLeave(commits);
    await new Promise((r) => setTimeout(r, 120));
    expect(server.to('GET', COMMITS)).toHaveLength(0);
  });

  it('hovering a folder of the file list reads that folder', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/tree', lang: 'en', appStaleTime: true });
    const src = await screen.findByRole('link', { name: 'src/' });
    fireEvent.mouseEnter(src);
    await waitFor(() => {
      expect(server.to('GET', TREE).some((c) => c.query.get('path') === 'src')).toBe(true);
    });
  });

  it('two prefetches of one target share one flight', async () => {
    const server = mockServer(routes());
    const { queryClient, api } = clients(server.fetch);
    const a = prefetchRoute(queryClient, api, '/acme/forge-web/-/tree/main/src');
    const b = prefetchRoute(queryClient, api, '/acme/forge-web/-/tree/main/src');
    expect(b).toBe(a);
    await Promise.all([a, b]);
    expect(server.to('GET', RESOLVE)).toHaveLength(1);
    expect(server.to('GET', '/api/v1/projects/7/repository/resolve')).toHaveLength(1);
    expect(server.to('GET', TREE)).toHaveLength(1);
    expect(queryClient.getQueryData(['tree', 7, 'refs/heads/main', 'src'])).toBeDefined();
  });

  it('a failed prefetch rejects nothing and keeps the failure for the screen', async () => {
    const server = mockServer({
      ...signedIn(),
      [`GET ${RESOLVE}`]: () =>
        json(500, { error_code: 'FG-X-0001', user_message: 'broken', request_id: 'req-pf-1' }),
    });
    const { queryClient, api } = clients(server.fetch);
    await expect(prefetchRoute(queryClient, api, '/acme/forge-web/-/tree')).resolves.toBeUndefined();
    expect(queryClient.getQueryState(resolveKeys.one('acme/forge-web', false))?.status).toBe('error');
  });

  it('Save-Data or a 2G connection prefetches nothing', async () => {
    expect(connectionIsSlow({ connection: { saveData: true } } as unknown as Navigator)).toBe(true);
    expect(connectionIsSlow({ connection: { effectiveType: '2g' } } as unknown as Navigator)).toBe(true);
    expect(connectionIsSlow({ connection: { effectiveType: '4g' } } as unknown as Navigator)).toBe(false);
    Object.defineProperty(navigator, 'connection', { value: { saveData: true }, configurable: true });
    const server = mockServer(routes());
    const { queryClient, api } = clients(server.fetch);
    await prefetchRoute(queryClient, api, '/acme/forge-web/-/tree');
    expect(server.calls).toHaveLength(0);
  });

  it('the project page reads the history and the open MRs when idle', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web', lang: 'en', appStaleTime: true });
    await waitFor(() => {
      expect(server.to('GET', COMMITS).some((c) => !c.query.has('limit'))).toBe(true);
    });
    await waitFor(() => {
      expect(server.to('GET', MRS)[0]?.query.get('state')).toBe('open');
    });
  });
});

describe('cache policy', () => {
  it('queries at a full commit id are never read again; branch-addressed ones are after the stale time', async () => {
    const server = mockServer(routes());
    const { queryClient, api } = clients(server.fetch);
    expect(commitQuery(api, 7, C1)).toMatchObject(IMMUTABLE);
    expect(treeQuery(api, 7, C1, '')).toMatchObject(IMMUTABLE);
    expect(blobQuery(api, 7, 'refs/heads/main', 'README.md').staleTime).toBeUndefined();
    await queryClient.query(commitQuery(api, 7, C1));
    await queryClient.query(treeQuery(api, 7, C1, ''));
    await queryClient.query(treeQuery(api, 7, 'refs/heads/main', ''));
    vi.useFakeTimers({ toFake: ['Date'] });
    vi.setSystemTime(Date.now() + 60 * 60_000);
    await queryClient.query(commitQuery(api, 7, C1));
    await queryClient.query(treeQuery(api, 7, C1, ''));
    await queryClient.query(treeQuery(api, 7, 'refs/heads/main', ''));
    expect(server.to('GET', `${COMMITS}/${C1}`)).toHaveLength(1);
    expect(server.to('GET', TREE).filter((c) => c.query.get('ref') === C1)).toHaveLength(1);
    expect(server.to('GET', TREE).filter((c) => c.query.get('ref') === 'refs/heads/main')).toHaveLength(2);
  });

  it('a commit screen opened again is drawn from the cache without a request', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: `/acme/forge-web/-/commit/${C1}`, lang: 'en', appStaleTime: true });
    expect(await screen.findByText('Synthetic body line.', { exact: false })).toBeTruthy();
    fireEvent.click(link('/acme/forge-web/-/commits'));
    const again = await screen.findByRole('link', { name: 'Add the add function' });
    fireEvent.click(again);
    expect(await screen.findByText('Synthetic body line.', { exact: false })).toBeTruthy();
    expect(server.to('GET', `${COMMITS}/${C1}`)).toHaveLength(1);
  });

  it('the MR list key of a prefetch is the screen key', () => {
    const { api } = clients(mockServer({}).fetch);
    expect(mergeRequestsQuery(api, 7, { state: 'open' }).queryKey).toEqual(['mr', 7, 'list', { state: 'open' }]);
  });
});

describe('namespace resolution cache', () => {
  it('the project page and its tabs ask the resolver once', async () => {
    const server = mockServer(routes());
    await renderApp({ fetch: server.fetch, path: '/acme/forge-web', lang: 'en', appStaleTime: true });
    await screen.findByTestId('clone-url');
    fireEvent.click(link('/acme/forge-web/-/commits'));
    await screen.findByText('Add the add function');
    fireEvent.click(link('/acme/forge-web/-/branches'));
    await screen.findByRole('heading', { name: 'Branches', level: 1 });
    fireEvent.click(link('/acme/forge-web/-/tree'));
    await screen.findByRole('link', { name: 'src/' });
    expect(server.to('GET', RESOLVE)).toHaveLength(1);
  });

  it('forgetResolutions drops every cached path', async () => {
    const server = mockServer(routes());
    const { queryClient, api } = clients(server.fetch);
    await queryClient.query(resolveQuery(api, queryClient, 'acme/forge-web', false));
    await queryClient.query(resolveQuery(api, queryClient, 'acme/forge-web', false));
    expect(server.to('GET', RESOLVE)).toHaveLength(1);
    forgetResolutions(queryClient);
    expect(queryClient.getQueryData(resolveKeys.one('acme/forge-web', false))).toBeUndefined();
    await queryClient.query(resolveQuery(api, queryClient, 'acme/forge-web', false));
    expect(server.to('GET', RESOLVE)).toHaveLength(2);
  });

  const settingsRoutes = () => ({
    ...signedIn(),
    [`GET ${RESOLVE}`]: (c: { query: URLSearchParams }) => {
      const path = c.query.get('path') ?? '';
      if (path === 'taro') {
        return json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 });
      }
      return json(200, { ...RESOLVE_PROJECT, canonical_path: path });
    },
    'GET /api/v1/projects/7': () => json(200, { ...PROJECT, role: 'owner' }),
    'GET /api/v1/projects/7/protected-refs': () => json(200, { rules: [] }),
    'GET /api/v1/projects/7/merge-settings': () => json(404, { error_code: 'FG-X-0001', user_message: 'x', request_id: 'r' }),
    'GET /api/v1/projects/7/approval-rules': () => json(200, { rules: [] }),
    'GET /api/v1/projects/7/integration-bindings': () => json(200, { items: [] }),
    'GET /api/v1/projects/7/doc-lint/migration': () => json(404, { error_code: 'FG-X-0001', user_message: 'x', request_id: 'r' }),
    'GET /api/v1/groups': () => page([]),
    'GET /api/v1/projects': () => page([]),
  });

  it('a rename drops the cached meaning of the old path', async () => {
    const server = mockServer({
      ...settingsRoutes(),
      'POST /api/v1/projects/7/transfer': () =>
        json(200, {
          id: 7, kind: 'project', namespace_id: 11, old_path: 'acme/forge-web', path: 'acme/forge-web-2',
          projects: 1, reclaimed_tombstones: 0,
        }),
    });
    const { queryClient } = await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en', appStaleTime: true });
    fireEvent.change(await screen.findByLabelText('Last part of the path'), { target: { value: 'forge-web-2' } });
    expect(queryClient.getQueryData(resolveKeys.one('acme/forge-web', false))).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: 'Move or rename' }));
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe('/acme/forge-web-2/-/settings');
    });
    expect(queryClient.getQueryState(resolveKeys.one('acme/forge-web', false))).toBeUndefined();
  });

  it('a deletion drops the cached meaning of the path', async () => {
    const server = mockServer({
      ...settingsRoutes(),
      'DELETE /api/v1/projects/7': () =>
        json(200, { id: 7, deleted_at: '2026-11-01T00:00:00Z', purge_after: '2026-11-08T00:00:00Z' }),
    });
    const { queryClient } = await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en', appStaleTime: true });
    await screen.findByLabelText('Name');
    expect(queryClient.getQueryData(resolveKeys.one('acme/forge-web', false))).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: 'Delete this project' }));
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Delete this project' }));
    await waitFor(() => {
      expect(server.to('DELETE', '/api/v1/projects/7')).toHaveLength(1);
    });
    await waitFor(() => {
      expect(screen.getByTestId('location').textContent).toBe(`/${SESSION.user.username}/-/settings?tab=trash`);
    });
    expect(queryClient.getQueryState(resolveKeys.one('acme/forge-web', false))).toBeUndefined();
  });

  it('a visibility change marks the cached path meanings stale', async () => {
    const server = mockServer({
      ...settingsRoutes(),
      'PUT /api/v1/projects/7/visibility': () => json(200, { ...PROJECT, role: 'owner', visibility: 'private' }),
    });
    const { queryClient } = await renderApp({ fetch: server.fetch, path: '/acme/forge-web/-/settings', lang: 'en', appStaleTime: true });
    await screen.findByLabelText('Name');
    fireEvent.click(screen.getByLabelText(/^Private/));
    fireEvent.click(screen.getAllByRole('button', { name: 'Save' })[1] as HTMLElement);
    await waitFor(() => {
      expect(server.to('PUT', '/api/v1/projects/7/visibility')).toHaveLength(1);
    });
    // The shown path is read again (and stays shown meanwhile).
    const ofProject = () => server.to('GET', RESOLVE).filter((c) => c.query.get('path') === 'acme/forge-web');
    await waitFor(() => {
      expect(ofProject()).toHaveLength(2);
    });
    expect(queryClient.getQueryData(resolveKeys.one('acme/forge-web', false))).toBeDefined();
  });

  it('another signed-in user drops the cached path meanings', async () => {
    const server = mockServer(routes());
    const { queryClient, api } = clients(server.fetch);
    watchSessionUser(queryClient);
    queryClient.setQueryData(['session'], { ...SESSION });
    await queryClient.query(resolveQuery(api, queryClient, 'acme/forge-web', false));
    queryClient.setQueryData(['session'], { ...SESSION, csrf_token: 'd'.repeat(64) });
    expect(queryClient.getQueryData(resolveKeys.one('acme/forge-web', false))).toBeDefined();
    queryClient.setQueryData(['session'], { ...SESSION, user: { ...SESSION.user, id: 99 } });
    expect(queryClient.getQueryData(resolveKeys.one('acme/forge-web', false))).toBeUndefined();
  });
});

describe('prefetch limits', () => {
  it('nothing is prefetched while the page is hidden', async () => {
    Object.defineProperty(document, 'hidden', { value: true, configurable: true });
    try {
      const server = mockServer(routes());
      const { queryClient, api } = clients(server.fetch);
      await prefetchRoute(queryClient, api, '/acme/forge-web/-/tree');
      expect(server.calls).toHaveLength(0);
    } finally {
      Reflect.deleteProperty(document, 'hidden');
    }
  });

  it(`at most ${MAX_FLIGHTS} prefetches run at once`, async () => {
    let release: () => void = () => undefined;
    const gate = new Promise<void>((r) => {
      release = r;
    });
    const server = mockServer({
      ...routes(),
      [`GET ${RESOLVE}`]: async (c) => {
        await gate;
        return json(200, { ...RESOLVE_PROJECT, canonical_path: c.query.get('path') ?? '' });
      },
    });
    const { queryClient, api } = clients(server.fetch);
    const flights = ['a/p1', 'a/p2', 'a/p3', 'a/p4', 'a/p5'].map((p) => prefetchRoute(queryClient, api, `/${p}/-/members`));
    await new Promise((r) => setTimeout(r, 20));
    expect(server.to('GET', RESOLVE)).toHaveLength(MAX_FLIGHTS);
    release();
    await Promise.all(flights);
  });

  it('a compare is never cached as immutable (its names may be branch names)', () => {
    const { api } = clients(mockServer({}).fetch);
    expect(compareQuery(api, 7, C1, C1).staleTime).toBeUndefined();
  });
});
