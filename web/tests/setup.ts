import { cleanup } from '@testing-library/react';
import { afterEach } from 'vitest';

// Node 25 ships a built-in global Web Storage (`localStorage` /
// `sessionStorage`). Without `--localstorage-file` it is an object whose
// methods are undefined. vitest's jsdom environment only copies a window key
// onto the global when the global does not already have it, so Node's broken
// Storage shadows jsdom's and `window.localStorage.clear()` throws.
// vitest exposes the JSDOM instance as `globalThis.jsdom`; point both names at
// jsdom's real Storage. On Node 22 (CI) that is the object the global already
// returns, so the suite behaves the same on both. Assigned unconditionally:
// merely reading Node's getter prints a `--localstorage-file` warning.
// Chosen over `execArgv: ['--no-experimental-webstorage']`: that flag is
// unknown to Node < 22.4 (engines allows >= 22) and may be renamed when the
// feature leaves experimental status; this works on every version.
const dom = (globalThis as { jsdom?: { window: Pick<Window, 'localStorage' | 'sessionStorage'> } })
  .jsdom;
if (dom) {
  for (const name of ['localStorage', 'sessionStorage'] as const) {
    Object.defineProperty(globalThis, name, {
      value: dom.window[name],
      configurable: true,
      writable: true,
    });
  }
}

// MarkdownView renders the parser through React.lazy (a separate chunk in the
// build). vitest isolates each test file, so without this the first test of a
// file that shows Markdown (a README, an MR description, a thread) pays the
// cold import of react-markdown + remark-gfm + micromark *inside* its 1 s
// `waitFor` / `findBy` budget: ~0.6 s on an idle machine, 0.6-3.5 s while the
// whole suite runs in parallel — the Loading fallback was still on screen at
// the timeout. Evaluating the chunk here, before any test starts, makes the
// lazy import resolve from the module cache; the Suspense boundary is still
// exercised, it just no longer depends on machine load.
await import('../src/features/Markdown');
// The same for the screens split out of the main chunk (pages/lazyScreens.tsx).
await (await import('../src/pages/lazyScreens')).loadAllScreens();

afterEach(() => {
  cleanup();
});
