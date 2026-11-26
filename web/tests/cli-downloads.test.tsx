// The gstn downloads on the access-token page: only the shipped targets,
// each with its size and SHA-256 and a same-origin download link; a server
// that ships none says so; the setup steps are linked from the section.
import { screen, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { mockServer, signedIn } from './mock';
import { json, renderApp } from './render';

const SHA = 'ab'.repeat(32);
const PATH = '/taro/-/settings';

const user = (over: Record<string, unknown> = {}) => ({
  ...signedIn(),
  'GET /api/v1/namespaces/resolve': () =>
    json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
  'GET /api/v1/user/tokens': () => json(200, { items: [] }),
  ...over,
});

describe('gstn downloads', () => {
  it('lists the shipped targets with size, SHA-256 and a download link', async () => {
    const server = mockServer(
      user({
        'GET /api/v1/clients': () =>
          json(200, {
            items: [
              {
                name: 'gstn',
                target: 'linux-x86_64',
                version: '0.1.0+abc1234',
                sha256: SHA,
                size: 2048,
                download_path: '/api/v1/clients/gstn/linux-x86_64/binary',
              },
            ],
          }),
      }),
    );
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    const section = (await screen.findByRole('heading', { name: 'Download the CLI (gstn)' })).closest('section');
    expect(section).not.toBeNull();
    const scope = within(section as HTMLElement);
    expect(await scope.findByText(SHA)).toBeTruthy();
    expect(scope.getByText('0.1.0+abc1234')).toBeTruthy();
    const link = scope.getByRole('link', { name: 'Download for Linux (x86_64)' });
    expect(link.getAttribute('href')).toBe('/api/v1/clients/gstn/linux-x86_64/binary');
    expect(link.hasAttribute('download')).toBe(true);
    // Windows is not shipped: not offered.
    expect(scope.queryByRole('link', { name: /Windows/ })).toBeNull();
    expect(scope.getByRole('link', { name: 'Setup steps' }).getAttribute('href')).toBe('#cli-setup');
    expect(scope.getByRole('heading', { name: 'Setup steps' }).id).toBe('cli-setup');
    expect(server.unmatched).toEqual([]);
  });

  it('says so when the server ships none', async () => {
    const server = mockServer(user());
    await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
    expect(await screen.findByText('This server does not distribute gstn. Ask the administrator.')).toBeTruthy();
  });
});
