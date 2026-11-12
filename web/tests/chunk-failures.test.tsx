// A chunk that does not load (offline, or an old shell pointing at a deleted
// file after a deploy) must never leave a blank or stuck page:
// - the en dictionary chunk failing: start in ja, keep the stored choice
//   unchanged, and refuse a switch to en (the page stays in ja);
// - a lazy screen chunk failing: the error with a retry that loads again.
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { I18nextProvider } from 'react-i18next';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { createI18n, switchLanguage } from '../src/i18n';
import { lazyScreen } from '../src/pages/lazyScreens';
import { mockServer, signedIn, ACTIVITY_PATH } from './mock';
import { json, renderApp } from './render';

vi.mock('../src/i18n/en.json', () => {
  throw new Error('chunk en-deadbeef.js is gone');
});

afterEach(() => {
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe('a language chunk that does not load', () => {
  it('createI18n falls back to the bundled ja instead of failing the start', async () => {
    const log = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const i18n = await createI18n('en');
    expect(i18n.language).toBe('ja');
    expect(i18n.t('app.name')).not.toBe('app.name');
    expect(log).toHaveBeenCalled();
  });

  it('switchLanguage keeps the current language and does not remember the choice', async () => {
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const i18n = await createI18n('ja');
    expect(await switchLanguage(i18n, 'en')).toBe(false);
    expect(i18n.language).toBe('ja');
    expect(window.localStorage.getItem('gitstation.lang')).toBeNull();
  });

  it('the header language menu stays in ja when en cannot load', async () => {
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const server = mockServer({
      ...signedIn(),
      'GET /api/v1/projects': () => json(200, { items: [], has_more: false, next_cursor: null }),
      'GET /api/v1/groups': () => json(200, { items: [], has_more: false, next_cursor: null }),
      [`GET ${ACTIVITY_PATH}`]: () => json(200, { events: [], next_before: null, truncated: false }),
    });
    await renderApp({ fetch: server.fetch });
    await screen.findByRole('heading', { name: 'ホーム' });
    fireEvent.change(screen.getByLabelText('表示する言語'), { target: { value: 'en' } });
    await new Promise((r) => setTimeout(r, 50));
    expect(screen.getByRole('heading', { name: 'ホーム' })).toBeTruthy();
    expect(window.localStorage.getItem('gitstation.lang')).toBeNull();
  });
});

describe('a lazy screen chunk that does not load', () => {
  it('shows the error with a retry, and the retry loads the chunk again', async () => {
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const i18n = await createI18n('ja');
    let attempts = 0;
    const Screen = lazyScreen(() => {
      attempts += 1;
      if (attempts === 1) return Promise.reject(new Error('Failed to fetch dynamically imported module'));
      return Promise.resolve(function Loaded({ name }: { name: string }) {
        return <p>{name}</p>;
      });
    });
    render(
      <I18nextProvider i18n={i18n}>
        <Screen name="loaded-screen" />
      </I18nextProvider>,
    );
    const retry = await screen.findByRole('button', { name: i18n.t('error.retry') });
    fireEvent.click(retry);
    await waitFor(() => {
      expect(screen.getByText('loaded-screen')).toBeTruthy();
    });
    expect(attempts).toBe(2);
  });
});
