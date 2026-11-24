// i18n (ADR-0022 §6): ja is the default; the browser's language picks en or
// id; a missing id string falls back to en. The choice the user makes lives
// in this browser only.
import i18next, { type BackendModule, type i18n } from 'i18next';
import { initReactI18next } from 'react-i18next';

// Only the ja strings the first load uses; the rest is a chunk the lazy
// screens wait for (`loadJaRest`, scripts/i18n-split.mjs).
import ja from 'virtual:i18n/ja-core';

export const LANGUAGES = ['ja', 'en', 'id'] as const;
export type Language = (typeof LANGUAGES)[number];

/** ja (the default, most people) is in the main chunk (its first-load part); en and id are
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

// Keyed by the instance's resource store: react-i18next's useTranslation
// hands each component its own wrapper object around the instance (a new
// one per language), but every wrapper shares the instance's store, which
// is where the strings live.
type Store = i18n['store'];
const jaRest = new WeakMap<Store, Promise<void>>();
const jaRestFailed = new WeakSet<Store>();
const jaRestLoaded = new WeakSet<Store>();
/** The strings, once the chunk has loaded (shared by every instance). */
let jaRestStrings: Record<string, unknown> | undefined;

/** Loads the chunk of the ja strings only the lazy screens use (tests
 * preload it with the screens, pages/lazyScreens.tsx `loadAllScreens`). */
export async function preloadJaRest(): Promise<Record<string, unknown>> {
  const m = await import('virtual:i18n/ja-rest');
  jaRestStrings = m.default;
  return m.default;
}

function addJaRest(instance: i18n, strings: Record<string, unknown>): void {
  instance.addResourceBundle('ja', 'translation', strings, true, false);
  jaRestLoaded.add(instance.store);
}

/** Whether `loadJaRest` has added the strings to `instance`. */
export function hasJaRest(instance: i18n): boolean {
  return jaRestLoaded.has(instance.store);
}

/** Adds the ja strings only the lazy screens use to `instance` (once; the
 * same promise until it fails and `forgetFailedJaRest` drops it). A lazy
 * screen renders only after this resolves (pages/lazyScreens.tsx), so it
 * never shows a raw key. Loaded whatever the language: switching to ja on
 * a lazy screen must find the strings there. */
export function loadJaRest(instance: i18n): Promise<void> {
  let pending = jaRest.get(instance.store);
  if (pending === undefined) {
    // Once the chunk is in, every later instance (a test's, or one made
    // after a reload of the provider) gets the strings at once.
    if (jaRestStrings !== undefined) addJaRest(instance, jaRestStrings);
    pending =
      jaRestStrings !== undefined
        ? Promise.resolve()
        : preloadJaRest().then(
            (strings) => {
              addJaRest(instance, strings);
            },
            (e: unknown) => {
              jaRestFailed.add(instance.store);
              throw e;
            },
          );
    jaRest.set(instance.store, pending);
  }
  return pending;
}

/** After a failed load, the next `loadJaRest` asks for the chunk again. */
export function forgetFailedJaRest(instance: i18n): void {
  if (jaRestFailed.delete(instance.store)) jaRest.delete(instance.store);
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
  // A language chunk that did not load (offline, or a shell older than the
  // deployed files): start in the bundled ja rather than a blank page or
  // raw keys. The stored choice stays; the next start tries it again.
  if (!instance.hasResourceBundle(language, 'translation')) {
    console.error(`gitstation: the ${language} language bundle did not load; showing ja`);
    await instance.changeLanguage('ja');
  }
  return instance;
}

/** Switch to `lng` once its strings are loaded, and remember the choice
 * only then. `false` (logged once): the chunk did not load and nothing
 * changed. */
export async function switchLanguage(instance: i18n, lng: Language): Promise<boolean> {
  // A lazy screen already asked for the ja remainder (it may be on screen
  // now): ja is complete only with it.
  if (lng === 'ja' && jaRest.has(instance.store)) {
    forgetFailedJaRest(instance);
    try {
      await loadJaRest(instance);
    } catch {
      console.error(`gitstation: the ja strings did not load; keeping ${instance.language}`);
      return false;
    }
  }
  await instance.loadLanguages(lng);
  if (!instance.hasResourceBundle(lng, 'translation')) {
    console.error(`gitstation: the ${lng} language bundle did not load; keeping ${instance.language}`);
    return false;
  }
  await instance.changeLanguage(lng);
  rememberLanguage(lng);
  return true;
}
