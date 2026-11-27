// Plan 19 §8.2: the project's AI first-pass review in the settings. Off by
// default; a Maintainer chooses advisory with the CSRF header and sees the
// new state; someone the server would refuse reads it and is told who can,
// never the form; a failed read says so with a retry. axe.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { llmSetting, settingsRoutes } from './merge-settings-fixtures';
import { apiError, mockServer, SESSION, type Handler } from './mock';
import { json, renderApp } from './render';

const API = '/api/v1/projects/7/llm';
const PATH = '/acme/forge-web/-/settings';

async function open(over: Record<string, Handler> = {}, lang: 'en' | 'ja' = 'en') {
  const server = mockServer(settingsRoutes(over));
  const view = await renderApp({ fetch: server.fetch, path: PATH, lang });
  await screen.findByRole('heading', { level: 1 });
  return { server, ...view };
}

describe('AI first-pass review setting', () => {
  it('a Maintainer chooses advisory with the CSRF header', async () => {
    const { server } = await open({
      [`PUT ${API}`]: (c) =>
        json(
          200,
          llmSetting({
            review: (c.body as { review: string }).review,
            changed_by: 5,
            changed_at: '2026-11-28T01:02:03Z',
            viewer: { can_change: true },
          }),
        ),
    });
    const s = await screen.findByRole('region', { name: 'AI first-pass review' });
    expect(await within(s).findByText('Now: Off (default)')).toBeTruthy();
    fireEvent.click(within(s).getByRole('radio', { name: 'As advice' }));
    fireEvent.click(within(s).getByRole('button', { name: 'Save' }));
    await waitFor(() => {
      expect(server.to('PUT', API)).toHaveLength(1);
    });
    const put = server.to('PUT', API)[0];
    expect(put?.body).toEqual({ review: 'advisory' });
    expect(put?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    expect(await within(s).findByText('Saved: As advice')).toBeTruthy();
    expect(within(s).getByText('Now: As advice')).toBeTruthy();
    const results = await axe.run(s);
    expect(results.violations).toEqual([]);
  });

  it('someone who may not change it reads it and is told who can', async () => {
    const { server } = await open({
      [`GET ${API}`]: () => json(200, llmSetting({ review: 'required', changed_by: 5, changed_at: '2026-11-28T01:02:03Z' })),
    });
    const s = await screen.findByRole('region', { name: 'AI first-pass review' });
    expect(await within(s).findByText('Now: Required')).toBeTruthy();
    expect(within(s).queryByRole('radio')).toBeNull();
    expect(within(s).queryByRole('button', { name: 'Save' })).toBeNull();
    expect(
      within(s).getByText(
        'Only a Maintainer or above can change this, signed in on the web (never an agent, a bot or a service account).',
      ),
    ).toBeTruthy();
    expect(server.to('PUT', API)).toHaveLength(0);
  });

  it('a failed read says so and offers a retry (ja)', async () => {
    await open({ [`GET ${API}`]: () => apiError(500, '設定を読めませんでした。', 'req-llm-1') }, 'ja');
    const s = await screen.findByRole('region', { name: 'AI の一次レビュー' });
    expect(await within(s).findByText(/req-llm-1/)).toBeTruthy();
    expect(within(s).getByRole('button', { name: 'もう一度試す' })).toBeTruthy();
  });
});
