// axe (WCAG 2.2 AA tags) on every repository screen and state. jsdom has no
// layout, so colour contrast is checked by tokens.test.ts and, in a real
// browser, by the Playwright journey.
import axe from 'axe-core';
import { screen, waitFor } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { apiError, mockServer } from './mock';
import { C1, COMMIT_DETAIL, DIFF, blob, repoRoutes } from './repo-fixtures';
import { json, renderApp } from './render';
import type { Language } from '../src/i18n';

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map(
    (v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`,
  );
}

const BASE = '/acme/forge-web/-';

const SCREENS: [string, string, string, Record<string, ReturnType<typeof repoRoutes>[string]>?][] = [
  ['tree', `${BASE}/tree/main`, 'Press Line Spec'],
  ['sub folder', `${BASE}/tree/main/src`, 'lib.rs'],
  ['code', `${BASE}/blob/main/src/lib.rs`, 'lib.rs'],
  ['markdown', `${BASE}/blob/main/README.md`, 'Press Line Spec'],
  ['image', `${BASE}/blob/main/logo.png`, 'logo.png'],
  ['binary', `${BASE}/blob/main/data.bin`, 'data.bin'],
  ['blame', `${BASE}/blame/main/src/lib.rs`, 'lib.rs'],
  ['commits', `${BASE}/commits`, 'Commits'],
  ['commit with diff', `${BASE}/commit/${C1}`, 'Add the add function'],
  ['compare', `${BASE}/compare/main...dev`, 'Compare'],
  ['compare form', `${BASE}/compare`, 'Compare'],
  ['branches', `${BASE}/branches`, 'Branches'],
  ['tags', `${BASE}/tags`, 'Tags'],
];

describe('axe: repository screens', () => {
  for (const lang of ['ja', 'en', 'id'] as Language[]) {
    it.each(SCREENS)(`${lang}: %s`, async (_name, path) => {
      const server = mockServer(repoRoutes());
      const { container } = await renderApp({ fetch: server.fetch, path, lang });
      await screen.findAllByRole('heading', { level: 1 });
      // Colouring arrives after the text.
      await waitFor(() => {
        expect(screen.queryByText(/読み込んでいます|Loading|Memuat/)).toBeNull();
      });
      expect(await violations(container)).toEqual([]);
    });
  }

  it('states: truncated, omitted patches, binary diff, not found, server error', async () => {
    const detail = {
      ...COMMIT_DETAIL,
      diff: { ...DIFF, truncated: true },
    };
    const cases: [string, Record<string, ReturnType<typeof repoRoutes>[string]>][] = [
      [`${BASE}/commit/${C1}`, { [`GET /api/v1/projects/7/repository/commits/${C1}`]: () => json(200, detail) }],
      [
        `${BASE}/blob/main/src/lib.rs`,
        {
          'GET /api/v1/projects/7/repository/blob': () =>
            json(200, blob('src/lib.rs', 'a\n', { truncated: true, size: 3_000_000 })),
        },
      ],
      [
        `${BASE}/blob/main/a.svg`,
        { 'GET /api/v1/projects/7/repository/blob': () => json(200, blob('a.svg', '<svg/>')) },
      ],
      [`${BASE}/blob/main/missing`, {}],
      [
        `${BASE}/tree/main`,
        { 'GET /api/v1/projects/7/repository/tree': () => apiError(500, 'エラー', 'req-500') },
      ],
    ];
    for (const [path, over] of cases) {
      const server = mockServer(repoRoutes(over));
      const view = await renderApp({ fetch: server.fetch, path, lang: 'en' });
      await waitFor(() => {
        expect(view.container.querySelector('main h1, main [role=alert], main [role=note]')).not.toBeNull();
      });
      await screen.findAllByRole('heading');
      expect(await violations(view.container)).toEqual([]);
      view.unmount();
    }
  });
});
