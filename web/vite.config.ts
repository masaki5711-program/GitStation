/// <reference types="vitest/config" />
// ADR-0022 §2 / §3: the build forge-web embeds (feature `embed-ui`).
// - Every emitted file sits flat in dist/ (one URL segment under /-/assets/),
//   with a content hash in its name, so it can be cached `immutable`.
// - No inline script or style: no module-preload polyfill, no inlined assets
//   (the shell CSP is `'self'` only).
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

/** The forge server `npm run dev` forwards the API and login to. */
const FORGE = process.env.FORGE_DEV_SERVER ?? 'http://127.0.0.1:8080';

export default defineConfig({
  plugins: [react()],
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
