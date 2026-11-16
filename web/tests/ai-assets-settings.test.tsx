// Plan 17 P17-3: the project's AI assets switch in the settings. Off by
// default; a Maintainer turns it on with the CSRF header and sees the new
// state; someone the server would refuse sees why, never the button. axe.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { aiAssets, settingsRoutes } from './merge-settings-fixtures';
import { mockServer, SESSION, type Handler } from './mock';
import { json, renderApp } from './render';

const API = '/api/v1/projects/7/ai-assets';
const PATH = '/acme/forge-web/-/settings';

async function open(over: Record<string, Handler> = {}) {
  const server = mockServer(settingsRoutes(over));
  const view = await renderApp({ fetch: server.fetch, path: PATH, lang: 'en' });
  await screen.findByRole('heading', { level: 1 });
  return { server, ...view };
}

const section = () => screen.findByRole('region', { name: 'AI assets' });

describe('AI assets setting', () => {
  it('a Maintainer turns the AI asset checks on with the CSRF header', async () => {
    const { server } = await open({
      [`PUT ${API}`]: (c) =>
        json(
          200,
          aiAssets({
            enabled: (c.body as { enabled: boolean }).enabled,
            changed_by: 5,
            changed_at: '2026-11-15T01:02:03Z',
            viewer: { can_change: true },
          }),
        ),
    });
    const s = await section();
    expect(await within(s).findByText('AI asset checks and rules: off (default)')).toBeTruthy();
    fireEvent.click(within(s).getByRole('button', { name: 'Turn AI asset checks on' }));
    await waitFor(() => {
      expect(server.to('PUT', API)).toHaveLength(1);
    });
    const put = server.to('PUT', API)[0];
    expect(put?.body).toEqual({ enabled: true });
    expect(put?.headers.get('X-GitStation-CSRF')).toBe(SESSION.csrf_token);
    expect(await within(s).findByText('AI asset checks and rules are on.')).toBeTruthy();
    expect(within(s).getByText('AI asset checks and rules: on')).toBeTruthy();
    expect(within(s).getByRole('button', { name: 'Turn AI asset checks off' })).toBeTruthy();
    const results = await axe.run(s);
    expect(results.violations).toEqual([]);
  });

  it('someone who may not change it reads it and is told who can', async () => {
    const { server } = await open({
      [`GET ${API}`]: () => json(200, aiAssets({ enabled: true, changed_by: 5, changed_at: '2026-11-15T01:02:03Z' })),
    });
    const s = await section();
    expect(await within(s).findByText('AI asset checks and rules: on')).toBeTruthy();
    expect(within(s).queryByRole('button', { name: /AI asset checks/ })).toBeNull();
    expect(
      within(s).getByText(
        'Only a Maintainer or above can change this, signed in on the web (never an agent, a bot or a service account).',
      ),
    ).toBeTruthy();
    expect(server.to('PUT', API)).toHaveLength(0);
  });
});
