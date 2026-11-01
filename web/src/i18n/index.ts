// i18n (ADR-0022 §6): ja is the default; the browser's language picks en or
// id; a missing id string falls back to en. The choice the user makes lives
// in this browser only.
import i18next, { type i18n } from 'i18next';
import { initReactI18next } from 'react-i18next';

import en from './en.json';
import id from './id.json';
import ja from './ja.json';

export const LANGUAGES = ['ja', 'en', 'id'] as const;
export type Language = (typeof LANGUAGES)[number];

export const resources = {
  ja: { translation: ja },
  en: { translation: en },
  id: { translation: id },
} as const;

const STORAGE_KEY = 'gitstation.lang';

function isLanguage(value: string): value is Language {
  return (LANGUAGES as readonly string[]).includes(value);
}

/** The stored choice, else the first supported browser language, else ja. */
export function detectLanguage(
  stored: string | null,
  browser: readonly string[],
): Language {
  if (stored !== null && isLanguage(stored)) return stored;
  for (const tag of browser) {
    const primary = tag.toLowerCase().split('-')[0] ?? '';
    if (isLanguage(primary)) return primary;
  }
  return 'ja';
}

/** Remember the user's choice in this browser only. */
export function rememberLanguage(lng: Language): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, lng);
  } catch {
    // Storage disabled: the choice lasts for this page view only.
  }
}

function storedLanguage(): string | null {
  try {
    return window.localStorage.getItem(STORAGE_KEY);
  } catch {
    // Storage disabled (private mode, policy): detect from the browser.
    return null;
  }
}

/** Initialise one i18next instance (tests make their own). */
export async function createI18n(lng?: Language): Promise<i18n> {
  const instance = i18next.createInstance();
  await instance.use(initReactI18next).init({
    resources,
    lng: lng ?? detectLanguage(storedLanguage(), navigator.languages),
    supportedLngs: [...LANGUAGES],
    fallbackLng: { id: ['en'], default: ['ja'] },
    interpolation: { escapeValue: false },
    returnNull: false,
  });
  return instance;
}
