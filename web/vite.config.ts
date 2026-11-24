/// <reference types="vitest/config" />
// ADR-0022 §2 / §3: the build forge-web embeds (feature `embed-ui`).
// - Every emitted file sits flat in dist/ (one URL segment under /-/assets/),
//   with a content hash in its name, so it can be cached `immutable`.
// - No inline script or style: no module-preload polyfill, no inlined assets
//   (the shell CSP is `'self'` only).
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { defineConfig, type Plugin } from 'vite';
import react from '@vitejs/plugin-react';

import { splitJa } from './scripts/i18n-split.mjs';

/** The forge server `npm run dev` forwards the API and login to. */
const FORGE = process.env.FORGE_DEV_SERVER ?? 'http://127.0.0.1:8080';

const JA_CORE = 'virtual:i18n/ja-core';
const JA_REST = 'virtual:i18n/ja-rest';

/** The ja dictionary in two modules (scripts/i18n-split.mjs): the strings
 * the first load uses (main chunk) and the rest (a chunk the lazy screens
 * wait for, src/i18n/index.ts `loadJaRest`). The build and the tests use
 * the real split; the dev server serves all of ja as core (it does not
 * re-split on every edit, and has no budget). */
function jaSplit(): Plugin {
  const src = fileURLToPath(new URL('./src', import.meta.url));
  const ids = new Map([
    ['\0' + JA_CORE, 'core'],
    ['\0' + JA_REST, 'rest'],
  ] as const);
  let split = true;
  let parts: { core: unknown; rest: unknown } | undefined;
  return {
    name: 'gitstation-ja-split',
    configResolved(config) {
      split = config.command === 'build' || process.env.VITEST !== undefined;
    },
    buildStart() {
      parts = undefined;
    },
    resolveId(id) {
      return id === JA_CORE || id === JA_REST ? '\0' + id : null;
    },
    load(id) {
      const part = ids.get(id);
      if (part === undefined) return null;
      this.addWatchFile(src + '/i18n/ja.json');
      parts ??= split
        ? splitJa(src)
        : { core: JSON.parse(readFileSync(src + '/i18n/ja.json', 'utf8')) as unknown, rest: {} };
      return 'export default ' + JSON.stringify(parts[part]) + ';';
    },
  };
}

export default defineConfig({
  plugins: [react(), jaSplit()],
  base: '/-/assets/',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    assetsDir: '',
    assetsInlineLimit: 0,
    cssCodeSplit: true,
    modulePreload: { polyfill: false },
    sourcemap: false,
    target: 'es2022',
    rollupOptions: {
      output: {
        entryFileNames: '[name]-[hash].js',
        chunkFileNames: '[name]-[hash].js',
        assetFileNames: '[name]-[hash][extname]',
      },
    },
  },
  server: {
    proxy: {
      '/api': FORGE,
      '/-/auth': FORGE,
    },
  },
  test: {
    environment: 'jsdom',
    include: ['tests/**/*.test.{ts,tsx}'],
    setupFiles: ['tests/setup.ts'],
    css: { modules: { classNameStrategy: 'non-scoped' } },
  },
});
