// Plan 16 A8: /-/marketplace (tabs by kind, beta / stable, the capability
// marks), one part's page, and the publish dialog (the CI result and the
// capability diff). The named tests: the marks are read aloud as words
// (never a colour alone), and nobody is shown a button the server would
// refuse them — publish, promote and revoke follow the caller's role on the
// publishing group, promoting a part that uses the network an
// administrator's. axe checks the screens.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { capabilityDiff, groupPathOf, lockedVersions, orderedCapabilities, updatesOf } from '../src/api/marketplace';
import { CATALOG, marketRoutes, version } from './marketplace-fixtures';
import { apiError, mockServer, SESSION, signedInAdmin, type Handler } from './mock';
import { json, renderApp } from './render';

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`);
}

async function openCatalog(groupRole: string | null, over: Record<string, Handler> = {}, path = '/-/marketplace') {
  const server = mockServer(marketRoutes(groupRole, over));
  const view = await renderApp({ fetch: server.fetch, path, lang: 'ja' });
  await screen.findByRole('heading', { level: 1, name: 'マーケットプレイス' });
  return { server, ...view };
}

const PART_PATH = '/api/v1/agent-parts/fmt-hook';
const FMT_VERSIONS = {
  slug: 'fmt-hook',
  versions: [
    version({ slug: 'fmt-hook', kind: 'hook', version: '0.3.0', channel: 'beta', capabilities: ['exec', 'permissions'] }),
    version({ slug: 'fmt-hook', kind: 'hook', version: '0.2.0', channel: 'stable', capabilities: ['exec'] }),
    version({
      slug: 'fmt-hook',
      kind: 'hook',
      version: '0.1.0',
      state: 'revoked',
      revoked_at: '2026-11-10T00:00:00Z',
      revoked_by: 9,
      revoked_reason: 'bad hook',
    }),
  ],
};
const FMT_DETAIL = {
  version: FMT_VERSIONS.versions[0],
  files: [{ path: '.claude/hooks/fmt.js', mode: '100755', sha256: 'f'.repeat(64), size: 2048 }],
};

async function openPart(groupRole: string | null, over: Record<string, Handler> = {}) {
  const server = mockServer(
    marketRoutes(groupRole, {
      [`GET ${PART_PATH}`]: () => json(200, FMT_VERSIONS),
      [`GET ${PART_PATH}/versions/0.3.0`]: () => json(200, FMT_DETAIL),
      ...over,
    }),
  );
  const view = await renderApp({ fetch: server.fetch, path: '/-/marketplace/fmt-hook', lang: 'ja' });
  await screen.findByRole('heading', { level: 1, name: 'fmt-hook' });
  return { server, ...view };
}

describe('marketplace helpers', () => {
  it('orders capabilities, diffs them and reads a lock', () => {
    expect(orderedCapabilities(['fs-write', 'zzz', 'exec'])).toEqual(['exec', 'fs-write', 'zzz']);
    expect(capabilityDiff(['exec'], ['exec', 'network'])).toEqual({ added: ['network'], removed: [], kept: ['exec'] });
    expect(capabilityDiff(['exec', 'permissions'], [])).toEqual({ added: [], removed: ['exec', 'permissions'], kept: [] });
    expect(groupPathOf('acme/sub/parts')).toBe('acme/sub');
    expect(groupPathOf('parts')).toBeNull();
    const locked = lockedVersions(JSON.stringify({ schema_version: 1, parts: [{ slug: 'a', version: '1.0.0' }] }));
    expect(locked?.get('a')).toBe('1.0.0');
    expect(lockedVersions('{')).toBeNull();
    expect(lockedVersions(JSON.stringify({ parts: [{ slug: 1 }] }))).toBeNull();
    expect(updatesOf(locked ?? new Map<string, string>(), [{ slug: 'a', version: '1.1.0' }, { slug: 'b', version: '2.0.0' }])).toEqual([
      { slug: 'a', from: '1.0.0', to: '1.1.0' },
    ]);
  });
});

describe('marketplace: the catalog', () => {
  it('capability marks are read aloud as words, never a colour alone', async () => {
    await openCatalog('developer');
    const card = (await screen.findByRole('link', { name: 'fmt-hook' })).closest('li');
    expect(card).not.toBeNull();
    const marks = within(card as HTMLElement).getByRole('list', { name: '能力' });
    const items = within(marks).getAllByRole('listitem').map((li) => li.textContent);
    expect(items).toEqual(['コマンドを実行する', '権限の設定を広げる']);
    // The channel is a word too.
    expect(within(card as HTMLElement).getByText('beta')).toBeTruthy();
    const mcp = (await screen.findByRole('link', { name: 'kanflow-mcp' })).closest('li') as HTMLElement;
    expect(within(mcp).getByRole('list', { name: '能力' }).textContent).toContain('ネットワークを使う');
    const plain = (await screen.findByRole('link', { name: 'review-skill' })).closest('li') as HTMLElement;
    expect(within(plain).getByText('特別な能力はありません')).toBeTruthy();
    expect(await violations(document.body)).toEqual([]);
  });

  it('tabs by kind and the channel filter ask the server', async () => {
    const { server } = await openCatalog('developer');
    const tabs = screen.getByRole('navigation', { name: '部品の種類' });
    fireEvent.click(within(tabs).getByRole('link', { name: 'フック' }));
    await waitFor(() => {
      expect(server.to('GET', '/api/v1/agent-parts').some((c) => c.query.get('kind') === 'hook')).toBe(true);
    });
    expect(within(tabs).getByRole('link', { name: 'フック' }).getAttribute('aria-current')).toBe('page');
    fireEvent.change(screen.getByLabelText('チャンネル'), { target: { value: 'stable' } });
    await waitFor(() => {
      expect(
        server.to('GET', '/api/v1/agent-parts').some((c) => c.query.get('kind') === 'hook' && c.query.get('channel') === 'stable'),
      ).toBe(true);
    });
  });

  it('a Developer of the publishing group sees no publish button', async () => {
    const { server } = await openCatalog('developer');
    await waitFor(() => {
      expect(server.to('GET', '/api/v1/groups/3')).toHaveLength(1);
    });
    await screen.findByRole('link', { name: 'review-skill' });
    expect(screen.queryByRole('button', { name: '部品を公開' })).toBeNull();
  });

  it('a Maintainer of the publishing group publishes: the CI result and the capability diff', async () => {
    const published = version({
      slug: 'fmt-hook',
      kind: 'hook',
      version: '0.4.0',
      channel: 'beta',
      ci_pipeline_id: 912,
      capabilities: ['exec', 'network'],
    });
    const { server } = await openCatalog('maintainer', {
      [`POST ${PART_PATH}/versions`]: () => json(201, { version: published, files: [] }),
    });
    fireEvent.click(await screen.findByRole('button', { name: '部品を公開' }));
    const dialog = await screen.findByRole('dialog', { name: '部品を公開' });
    fireEvent.change(within(dialog).getByLabelText('部品'), { target: { value: 'fmt-hook' } });
    fireEvent.change(within(dialog).getByLabelText('版'), { target: { value: '0.4.0' } });
    // Before publishing: what the newest version declares now.
    expect(within(dialog).getByText('いまの最新版 0.3.0 の能力')).toBeTruthy();
    fireEvent.click(within(dialog).getByRole('button', { name: '公開する' }));
    await waitFor(() => {
      expect(server.to('POST', `${PART_PATH}/versions`)).toHaveLength(1);
    });
    const post = server.to('POST', `${PART_PATH}/versions`)[0];
    expect(post?.body).toEqual({ tag: 'fmt-hook/v0.4.0' });
    expect(post?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    const result = await within(dialog).findByRole('status');
    expect(result.textContent).toContain('0.4.0 を beta として公開しました');
    expect(within(dialog).getByRole('link', { name: 'CI パイプライン #912 は成功しています' }).getAttribute('href')).toBe(
      '/acme/agent-parts/-/pipelines/912',
    );
    const added = within(dialog).getByRole('list', { name: '増えた能力' });
    expect(added.textContent).toContain('ネットワークを使う');
    const removed = within(dialog).getByRole('list', { name: 'なくなった能力' });
    expect(removed.textContent).toContain('権限の設定を広げる');
    expect(await violations(dialog)).toEqual([]);
  });

  it('a refused publish shows the server message and the request id', async () => {
    await openCatalog('maintainer', {
      [`POST ${PART_PATH}/versions`]: () =>
        apiError(409, 'このコミットの最新のパイプラインが成功していません。', 'req-ci-red', 'FG-AGENT-0004'),
    });
    fireEvent.click(await screen.findByRole('button', { name: '部品を公開' }));
    const dialog = await screen.findByRole('dialog', { name: '部品を公開' });
    fireEvent.change(within(dialog).getByLabelText('部品'), { target: { value: 'fmt-hook' } });
    fireEvent.change(within(dialog).getByLabelText('版'), { target: { value: '0.4.0' } });
    fireEvent.click(within(dialog).getByRole('button', { name: '公開する' }));
    expect(await within(dialog).findByText('このコミットの最新のパイプラインが成功していません。')).toBeTruthy();
    expect(within(dialog).getByText('req-ci-red')).toBeTruthy();
  });

  it('the publish form says what is missing before asking', async () => {
    const { server } = await openCatalog('maintainer');
    fireEvent.click(await screen.findByRole('button', { name: '部品を公開' }));
    const dialog = await screen.findByRole('dialog', { name: '部品を公開' });
    fireEvent.click(within(dialog).getByRole('button', { name: '公開する' }));
    expect(await within(dialog).findAllByText('部品の名前（slug）を入れてください。')).not.toHaveLength(0);
    expect(server.to('POST', `${PART_PATH}/versions`)).toHaveLength(0);
  });

  it('empty and error states', async () => {
    await openCatalog('developer', { 'GET /api/v1/agent-parts': () => json(200, { parts: [] }) });
    expect(await screen.findByText('この条件の部品はまだありません。')).toBeTruthy();
    document.body.innerHTML = '';
    await openCatalog('developer', {
      'GET /api/v1/agent-parts': () => apiError(500, '一覧を読めませんでした。', 'req-list-1'),
    });
    expect(await screen.findByText('一覧を読めませんでした。')).toBeTruthy();
    expect(screen.getByText('req-list-1')).toBeTruthy();
  });
});

describe('marketplace: one part', () => {
  it('shows the versions with their marks and files; no buttons for a Developer', async () => {
    await openPart('developer');
    const versions = screen.getByRole('table', { name: '版' });
    const rows = within(versions).getAllByRole('row');
    expect(rows).toHaveLength(4);
    expect(within(rows[1] as HTMLElement).getByRole('list', { name: '能力' }).textContent).toContain('コマンドを実行する');
    expect(within(rows[3] as HTMLElement).getByText('取り消し済み')).toBeTruthy();
    expect(within(rows[3] as HTMLElement).getByText('bad hook')).toBeTruthy();
    expect(await screen.findByText('.claude/hooks/fmt.js')).toBeTruthy();
    expect(screen.queryByRole('button', { name: /stable にする/ })).toBeNull();
    expect(screen.queryByRole('button', { name: /取り消す/ })).toBeNull();
    expect(await violations(document.body)).toEqual([]);
  });

  it('a Maintainer promotes the beta version and may revoke the live ones', async () => {
    const { server } = await openPart('maintainer', {
      [`POST ${PART_PATH}/versions/0.3.0/promote`]: () =>
        json(200, { changed: true, version: { ...FMT_DETAIL, version: { ...FMT_DETAIL.version, channel: 'stable' } } }),
    });
    await waitFor(() => {
      expect(screen.getByRole('button', { name: '0.3.0 を stable にする' })).toBeTruthy();
    });
    // The stable one cannot be promoted; the revoked one cannot be revoked.
    expect(screen.queryByRole('button', { name: '0.2.0 を stable にする' })).toBeNull();
    expect(screen.getByRole('button', { name: '0.2.0 を取り消す' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: '0.1.0 を取り消す' })).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: '0.3.0 を stable にする' }));
    await waitFor(() => {
      expect(server.to('POST', `${PART_PATH}/versions/0.3.0/promote`)).toHaveLength(1);
    });
    expect(await screen.findByText('0.3.0 を stable にしました。')).toBeTruthy();
  });

  it('promoting a part that uses the network is an administrator’s', async () => {
    const net = {
      slug: 'fmt-hook',
      versions: [version({ slug: 'fmt-hook', version: '0.3.0', channel: 'beta', capabilities: ['network'] })],
    };
    await openPart('maintainer', { [`GET ${PART_PATH}`]: () => json(200, net) });
    await screen.findByRole('button', { name: '0.3.0 を取り消す' });
    expect(screen.queryByRole('button', { name: '0.3.0 を stable にする' })).toBeNull();
    expect(screen.getByText('ネットワークを使う部品を stable にできるのは管理者だけです。')).toBeTruthy();
    document.body.innerHTML = '';
    const server = mockServer(
      marketRoutes(null, {
        ...signedInAdmin(),
        [`GET ${PART_PATH}`]: () => json(200, net),
        [`GET ${PART_PATH}/versions/0.3.0`]: () => json(200, FMT_DETAIL),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/marketplace/fmt-hook', lang: 'ja' });
    expect(await screen.findByRole('button', { name: '0.3.0 を stable にする' })).toBeTruthy();
  });

  it('revoking asks for a reason, then posts it', async () => {
    const { server } = await openPart('maintainer', {
      [`POST ${PART_PATH}/versions/0.2.0/revoke`]: () => json(200, FMT_DETAIL),
    });
    fireEvent.click(await screen.findByRole('button', { name: '0.2.0 を取り消す' }));
    const dialog = await screen.findByRole('dialog', { name: '0.2.0 を取り消す' });
    fireEvent.change(within(dialog).getByLabelText('理由'), { target: { value: 'leaks a token' } });
    fireEvent.click(within(dialog).getByRole('button', { name: '取り消す' }));
    await waitFor(() => {
      expect(server.to('POST', `${PART_PATH}/versions/0.2.0/revoke`)).toHaveLength(1);
    });
    expect(server.to('POST', `${PART_PATH}/versions/0.2.0/revoke`)[0]?.body).toEqual({ reason: 'leaks a token' });
  });

  it('a part that does not exist: the server message and request id', async () => {
    const server = mockServer(
      marketRoutes('developer', {
        [`GET ${PART_PATH}`]: () => apiError(404, '見つかりません。', 'req-404-part', 'FG-REPO-0005'),
      }),
    );
    await renderApp({ fetch: server.fetch, path: '/-/marketplace/fmt-hook', lang: 'ja' });
    expect(await screen.findByText('見つかりません。')).toBeTruthy();
    expect(screen.getByText('req-404-part')).toBeTruthy();
  });

  it('the catalog fixture has one of each mark', () => {
    expect(CATALOG.flatMap((p) => p.latest.capabilities).sort()).toEqual(['exec', 'network', 'permissions']);
  });
});
