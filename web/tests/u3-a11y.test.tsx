// axe (WCAG 2.2 AA tags) on the U3a screens and states, in every language.
// jsdom has no layout: contrast is tokens.test.ts' and the journey's job.
import axe from 'axe-core';
import { fireEvent, screen, waitFor } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import type { Language } from '../src/i18n';
import { apiError, mockServer, signedIn, type Handler } from './mock';
import { json, renderApp } from './render';
import { HOOKS, PREFS, PREFS_URL, SECRET, delivery, hookRoutes, webhook } from './webhook-fixtures';

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`);
}

const settle = async () => {
  await screen.findAllByRole('heading', { level: 1 });
  await waitFor(() => {
    expect(screen.queryByText(/読み込んでいます|Loading|Memuat/)).toBeNull();
  });
};

const LIST = `${HOOKS}/3/deliveries`;
const deliveriesRoutes: Record<string, Handler> = {
  [`GET ${HOOKS}/3`]: () => json(200, webhook(3, { enabled: false, disabled_reason: 'failures' })),
  [`GET ${LIST}`]: () =>
    json(200, {
      items: [
        delivery(2, { state: 'failed', attempts: 10, last_status: 503 }),
        delivery(1),
        delivery(3, { state: 'skipped', last_status: null, last_reason: 'owner_lost' }),
      ],
      next_before: 1,
    }),
  [`POST ${LIST}/2/redeliver`]: () => apiError(429, 'Busy.', 'req-a11y-429', 'FG-WEB-0010'),
};

const userRoutes = (): Record<string, Handler> => ({
  ...signedIn(),
  'GET /api/v1/namespaces/resolve': () =>
    json(200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }),
  [`GET ${PREFS_URL}`]: () => json(200, PREFS),
});

type Act = () => Promise<void>;
const SCREENS: [string, string, () => Record<string, Handler>, Act?][] = [
  [
    'webhook list',
    '/acme/forge-web/-/webhooks',
    () =>
      hookRoutes({
        [`GET ${HOOKS}`]: () =>
          json(200, { items: [webhook(3), webhook(4, { enabled: false, disabled_reason: 'failing_24h' })] }),
      }),
  ],
  [
    'webhook list, empty',
    '/acme/forge-web/-/webhooks',
    () => hookRoutes({ [`GET ${HOOKS}`]: () => json(200, { items: [] }) }),
  ],
  [
    'webhook list, refused',
    '/acme/forge-web/-/webhooks',
    () => hookRoutes({ [`GET ${HOOKS}`]: () => apiError(403, 'No.', 'req-1') }),
  ],
  [
    'the one-time secret',
    '/acme/forge-web/-/webhooks',
    () => hookRoutes({ [`POST ${HOOKS}`]: () => json(201, { webhook: webhook(4), secret: SECRET }) }),
    async () => {
      const url = document.querySelector<HTMLInputElement>('input[type="text"]');
      fireEvent.change(url as HTMLInputElement, { target: { value: 'https://hooks.example.test:8443/' } });
      fireEvent.submit((url as HTMLInputElement).closest('form') as HTMLFormElement);
      await screen.findByTestId('webhook-secret');
    },
  ],
  [
    'the edit dialog',
    '/acme/forge-web/-/webhooks',
    () => hookRoutes(),
    async () => {
      const edit = (await screen.findAllByRole('button')).find((b) => /ci3/.test(b.getAttribute('aria-label') ?? ''));
      fireEvent.click(edit as HTMLElement);
      await screen.findByRole('dialog');
    },
  ],
  [
    'delivery history with a refused redelivery',
    '/acme/forge-web/-/webhooks/3/deliveries',
    () => hookRoutes(deliveriesRoutes),
    async () => {
      const again = (await screen.findAllByRole('button')).find((b) => /#2/.test(b.getAttribute('aria-label') ?? ''));
      fireEvent.click(again as HTMLElement);
      await screen.findByTestId('request-id');
    },
  ],
  ['notification settings', '/taro/-/settings?tab=notifications', userRoutes],
];

describe.each(['ja', 'en', 'id'] as Language[])('U3a axe (%s)', (lang) => {
  it.each(SCREENS)('%s', async (_name, path, routes, act) => {
    const server = mockServer(routes());
    const { container } = await renderApp({ fetch: server.fetch, path, lang });
    await settle();
    if (act) await act();
    expect(await violations(container)).toEqual([]);
    expect(server.unmatched).toEqual([]);
  });
});
