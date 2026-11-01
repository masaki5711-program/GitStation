// axe on every state of the screens of this MR (WCAG 2.2 AA tags). jsdom has
// no layout or paint: the colour-contrast rule cannot run here (tokens.test.ts
// computes the ratios of the token pairs; the Playwright journey runs axe in a
// real browser with every rule on).
import axe from 'axe-core';
import { fireEvent, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import {
  GROUP,
  LISTED_GROUP,
  PROJECT,
  REFS_EMPTY,
  REFS_FILLED,
  RESOLVE_GROUP,
  RESOLVE_PROJECT,
  SPEC,
  apiError,
  mockServer,
  page,
  signedIn,
} from './mock';
import { json, renderApp } from './render';
import { repoRoutes } from './repo-fixtures';

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map(
    (v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`,
  );
}

const home = (over: Record<string, unknown> = {}) => ({
  ...signedIn(),
  'GET /api/v1/projects': (c: { query: URLSearchParams }) =>
    c.query.get('kind') === 'spec' ? page([SPEC]) : page([PROJECT]),
  'GET /api/v1/groups': () => page([LISTED_GROUP]),
  ...over,
});

describe('axe', () => {
  it('control: the check does find a violation (an image without alt text)', async () => {
    const holder = document.createElement('div');
    holder.innerHTML = '<img src="x.png">';
    document.body.append(holder);
    expect((await violations(holder)).join()).toContain('image-alt');
    holder.remove();
  });

  it('login', async () => {
    const server = mockServer({ 'GET /api/v1/session': () => apiError(401, 'x', 'r') });
    const { container } = await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ログインしてください' });
    expect(await violations(container)).toEqual([]);
  });

  it('login: sign-in not configured', async () => {
    const server = mockServer({
      'GET /api/v1/session': () => apiError(401, 'x', 'r'),
      'GET /-/auth/login': () => apiError(503, 'x', 'req-1', 'FG-AUTH-0018'),
    });
    const { container } = await renderApp({ fetch: server.fetch, lang: 'en' });
    fireEvent.click(await screen.findByRole('button', { name: 'Sign in with the IoT portal' }));
    await screen.findByRole('alert');
    expect(await violations(container)).toEqual([]);
  });

  for (const lang of ['ja', 'en', 'id'] as const) {
    it(`home (${lang}), menu open`, async () => {
      const server = mockServer(home());
      const { container } = await renderApp({ fetch: server.fetch, lang });
      await screen.findByText('Forge Web');
      await screen.findByText('Press Spec');
      fireEvent.click(container.querySelector('header button') as HTMLElement);
      expect(await violations(container)).toEqual([]);
    });
  }

  it('home: loading, empty and error', async () => {
    const server = mockServer(
      home({ 'GET /api/v1/projects': () => apiError(500, 'エラー', 'req-e1') }),
    );
    const { container } = await renderApp({ fetch: server.fetch });
    expect(await violations(container)).toEqual([]);
    await screen.findAllByRole('alert');
    expect(await violations(container)).toEqual([]);
  });

  it('home: the create dialog with its validation errors', async () => {
    const server = mockServer(home());
    const { container } = await renderApp({ fetch: server.fetch });
    fireEvent.click(await screen.findByRole('button', { name: 'プロジェクトを作る' }));
    const dialog = screen.getByRole('dialog');
    fireEvent.click(
      Array.from(dialog.querySelectorAll('button')).find((b) => b.type === 'submit') as HTMLElement,
    );
    await screen.findAllByText('名前を入力してください。');
    expect(await violations(container)).toEqual([]);
  });

  it('project: filled and empty repository', async () => {
    const routes = (refs: unknown) =>
      repoRoutes({
        'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_PROJECT),
        'GET /api/v1/projects/7/repository/refs': () => json(200, refs),
      });
    for (const refs of [REFS_FILLED, REFS_EMPTY]) {
      const server = mockServer(routes(refs));
      const { container, unmount } = await renderApp({ fetch: server.fetch, path: '/acme/forge-web' });
      await screen.findByText(refs === REFS_FILLED ? '既定のブランチ' : 'リポジトリはまだ空です');
      expect(await violations(container)).toEqual([]);
      unmount();
    }
  });

  it('group, not found and the unavailable screen', async () => {
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/namespaces/resolve': (c) =>
        c.query.get('path') === 'acme'
          ? json(200, RESOLVE_GROUP)
          : apiError(404, 'x', 'req-404'),
      'GET /api/v1/groups/3': () => json(200, GROUP),
      'GET /api/v1/groups': () => page([LISTED_GROUP]),
      'GET /api/v1/projects': () => page([PROJECT]),
    });
    const group = await renderApp({ fetch: server.fetch, path: '/acme' });
    await screen.findByText('Forge Web');
    expect(await violations(group.container)).toEqual([]);
    group.unmount();

    const missing = await renderApp({ fetch: server.fetch, path: '/nowhere' });
    await screen.findByText('ページが見つかりません');
    expect(await violations(missing.container)).toEqual([]);
  });
});
