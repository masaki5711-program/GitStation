// i18n (ADR-0022 §6): ja is the default; the browser's language picks en or
// id; a missing id string falls back to en. The choice the user makes lives
// in this browser only.
import i18next, { type BackendModule, type i18n } from 'i18next';
import { initReactI18next } from 'react-i18next';

import ja from './ja.json';

export const LANGUAGES = ['ja', 'en', 'id'] as const;
export type Language = (typeof LANGUAGES)[number];

/** ja (the default, most people) is in the main chunk; en and id are
 * separate chunks loaded when chosen (i18next loads a language, and its
 * fallback, before it switches to it). */
const LAZY: Record<Exclude<Language, 'ja'>, () => Promise<{ default: unknown }>> = {
  en: () => import('./en.json'),
  id: () => import('./id.json'),
};

const lazyLanguages: BackendModule = {
  type: 'backend',
  init: () => undefined,
  read(language, _namespace, callback) {
    if (language !== 'en' && language !== 'id') {
      callback(null, {});
      return;
    }
    LAZY[language]().then(
      (m) => {
        callback(null, m.default as Record<string, unknown>);
      },
      (e: unknown) => {
        callback(e instanceof Error ? e : new Error(String(e)), null);
      },
    );
  },
};

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
  const language = lng ?? detectLanguage(storedLanguage(), navigator.languages);
  await instance.use(lazyLanguages).use(initReactI18next).init({
    resources: { ja: { translation: ja } },
    partialBundledLanguages: true,
    lng: language,
    supportedLngs: [...LANGUAGES],
    fallbackLng: { id: ['en'], default: ['ja'] },
    interpolation: { escapeValue: false },
    returnNull: false,
  });
  // A language chunk that did not load would show raw keys: fail loudly.
  if (!instance.hasResourceBundle(language, 'translation')) {
    throw new Error(`the ${language} language bundle did not load`);
  }
  return instance;
}
