// The theme choice (user menu → テーマ): applies at once on <html
// data-theme>, is remembered in this browser, and defaults to the system.
import { fireEvent, screen, within } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { THEME_STORAGE_KEY, applyTheme, chooseTheme, storedTheme } from '../src/theme';
import { mockServer, page, signedIn } from './mock';
import { renderApp } from './render';

const home = () => ({
  ...signedIn(),
  'GET /api/v1/projects': () => page([]),
  'GET /api/v1/groups': () => page([]),
});

afterEach(() => {
  window.localStorage.clear();
  delete document.documentElement.dataset.theme;
  vi.restoreAllMocks();
});

async function openMenu() {
  const server = mockServer(home());
  await renderApp({ fetch: server.fetch });
  fireEvent.click(await screen.findByRole('button', { name: /山田 太郎/ }));
  return screen.getByRole('group', { name: 'テーマ' });
}

describe('theme', () => {
  it('defaults to システムに合わせる', async () => {
    expect(storedTheme()).toBe('system');
    const group = await openMenu();
    const radios = within(group).getAllByRole('radio');
    expect(radios.map((r) => r.parentElement?.textContent)).toEqual([
      'システムに合わせる',
      'ホワイト',
      'ネイビー',
      'ダーク',
    ]);
    expect(within(group).getByRole('radio', { name: 'システムに合わせる' })).toHaveProperty(
      'checked',
      true,
    );
  });

  it('picking a theme applies it at once and remembers it', async () => {
    const group = await openMenu();
    fireEvent.click(within(group).getByRole('radio', { name: 'ネイビー' }));
    expect(document.documentElement.dataset.theme).toBe('navy');
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe('navy');
    expect(within(group).getByRole('radio', { name: 'ネイビー' })).toHaveProperty('checked', true);

    fireEvent.click(within(group).getByRole('radio', { name: 'ダーク' }));
    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(storedTheme()).toBe('dark');
  });

  it('the menu shows the remembered theme as chosen', async () => {
    window.localStorage.setItem(THEME_STORAGE_KEY, 'white');
    const group = await openMenu();
    expect(within(group).getByRole('radio', { name: 'ホワイト' })).toHaveProperty('checked', true);
  });

  it('an unknown stored value falls back to the system', () => {
    window.localStorage.setItem(THEME_STORAGE_KEY, 'pink');
    expect(storedTheme()).toBe('system');
  });

  it('applyTheme(storedTheme()) is what the entry runs before the first render', () => {
    window.localStorage.setItem(THEME_STORAGE_KEY, 'dark');
    applyTheme(storedTheme());
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('still applies when storage is disabled', () => {
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('denied');
    });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('denied');
    });
    expect(storedTheme()).toBe('system');
    chooseTheme('navy');
    expect(document.documentElement.dataset.theme).toBe('navy');
  });
});
