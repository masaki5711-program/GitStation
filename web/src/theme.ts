// The colour theme (user menu → テーマ). tokens.css holds every value; this
// only sets <html data-theme> and remembers the choice in this browser
// (not a secret; storage may be disabled, so every access is guarded).
// main.tsx applies the stored choice before the first render; the CSP
// forbids an inline script in the shell, so the empty shell may paint once
// in the system theme before the bundle runs (no content is on it yet).

export const THEMES = ['system', 'white', 'navy', 'dark'] as const;
export type Theme = (typeof THEMES)[number];

export const THEME_STORAGE_KEY = 'gitstation.theme';

export function isTheme(value: unknown): value is Theme {
  return typeof value === 'string' && (THEMES as readonly string[]).includes(value);
}

/** The stored choice, else "system". */
export function storedTheme(): Theme {
  try {
    const value = window.localStorage.getItem(THEME_STORAGE_KEY);
    return isTheme(value) ? value : 'system';
  } catch {
    // Storage disabled (private mode, policy): follow the system.
    return 'system';
  }
}

/** Show `theme` now. */
export function applyTheme(theme: Theme): void {
  document.documentElement.dataset.theme = theme;
}

/** Show `theme` now and remember it for the next visit. */
export function chooseTheme(theme: Theme): void {
  applyTheme(theme);
  try {
    window.localStorage.setItem(THEME_STORAGE_KEY, theme);
  } catch {
    // Not remembered; the theme still applies to this page view.
  }
}
