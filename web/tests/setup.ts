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

afterEach(() => {
  cleanup();
});
