// axe (WCAG 2.2 AA tags) on the merge request screens and their states, in
// every language. jsdom has no layout: contrast is tokens.test.ts' job.
import axe from 'axe-core';
import { fireEvent, screen, waitFor } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import type { Language } from '../src/i18n';
import { apiError, mockServer, type Handler } from './mock';
import { MR_API, MR_SCREENS, mr, mrRoutes } from './mr-fixtures';
import { json, renderApp } from './render';

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map(
    (v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`,
  );
}

const settle = async () => {
  await screen.findAllByRole('heading', { level: 1 });
  await waitFor(() => {
    expect(screen.queryByText(/読み込んでいます|Loading|Memuat/)).toBeNull();
  });
};

const SCREENS: [string, string, Record<string, Handler>][] = [
  ['list', `${MR_SCREENS}/pulls`, { [`GET ${MR_API}`]: () => json(200, { merge_requests: [mr(4, { state: 'merged', draft: true }), mr(3)], next_before: 3 }) }],
  ['empty list', `${MR_SCREENS}/pulls?state=all`, { [`GET ${MR_API}`]: () => json(200, { merge_requests: [], next_before: null }) }],
  ['list error', `${MR_SCREENS}/pulls`, { [`GET ${MR_API}`]: () => apiError(500, 'エラー', 'req-500') }],
  ['create', `${MR_SCREENS}/pulls/new`, {}],
];

describe('axe: merge request screens', () => {
  for (const lang of ['ja', 'en', 'id'] as Language[]) {
    it.each(SCREENS)(`${lang}: %s`, async (_name, path, over) => {
      const server = mockServer(mrRoutes(over));
      const { container } = await renderApp({ fetch: server.fetch, path, lang });
      await settle();
      expect(await violations(container)).toEqual([]);
    });
  }

  it('create with every field error shown', async () => {
    const server = mockServer(mrRoutes());
    const { container } = await renderApp({
      fetch: server.fetch,
      path: `${MR_SCREENS}/pulls/new`,
      lang: 'en',
    });
    await settle();
    fireEvent.click(screen.getByRole('button', { name: 'Create merge request' }));
    await screen.findByRole('alert');
    expect(await violations(container)).toEqual([]);
  });
});
