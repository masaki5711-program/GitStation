// ADR-0022 §1 / §3a / §6.
// - `react/jsx-no-literals`: no hard-coded user-facing text in JSX (i18n).
// - `no-restricted-imports`: only src/ui/ may import Radix (the wrappers keep
//   menus `modal={false}` and avoid components that inject <style>).
// - No CSS-in-JS, no dangerouslySetInnerHTML.
import js from '@eslint/js';
import globals from 'globals';
import react from 'eslint-plugin-react';
import reactHooks from 'eslint-plugin-react-hooks';
import tseslint from 'typescript-eslint';

const RADIX_ONLY_IN_UI = {
  patterns: [
    {
      group: ['@radix-ui/*', 'radix-ui', 'radix-ui/*'],
      message: 'Import Radix only through the wrappers in src/ui/ (ADR-0022 §3a).',
    },
  ],
  paths: [
    { name: '@emotion/react', message: 'No runtime CSS-in-JS (CSP style-src self, ADR-0022 §1).' },
    { name: '@emotion/styled', message: 'No runtime CSS-in-JS (CSP style-src self, ADR-0022 §1).' },
    { name: 'styled-components', message: 'No runtime CSS-in-JS (CSP style-src self, ADR-0022 §1).' },
  ],
};

export default tseslint.config(
  { ignores: ['dist/', 'node_modules/', 'coverage/', 'src/api/schema.gen.ts'] },
  js.configs.recommended,
  ...tseslint.configs.strictTypeChecked,
  {
    languageOptions: {
      ecmaVersion: 2022,
      globals: { ...globals.browser },
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
    settings: { react: { version: 'detect' } },
    plugins: { react, 'react-hooks': reactHooks },
    rules: {
      ...react.configs.recommended.rules,
      ...react.configs['jsx-runtime'].rules,
      ...reactHooks.configs.recommended.rules,
      'react/jsx-no-literals': [
        'error',
        {
          noStrings: true,
          ignoreProps: true,
          noAttributeStrings: false,
          // Symbols and separators only; words come from the dictionaries.
          allowedStrings: ['·', '—', '/', ':', '(', ')', '#', '…', '|'],
        },
      ],
      'react/no-danger': 'error',
      'no-restricted-imports': ['error', RADIX_ONLY_IN_UI],
      '@typescript-eslint/restrict-template-expressions': ['error', { allowNumber: true }],
    },
  },
  {
    // The wrappers themselves may import Radix.
    files: ['src/ui/**'],
    rules: {
      'no-restricted-imports': ['error', { paths: RADIX_ONLY_IN_UI.paths }],
    },
  },
  {
    files: ['scripts/**', 'vite.config.ts', 'eslint.config.js'],
    languageOptions: { globals: { ...globals.node } },
  },
  {
    // Plain JavaScript (build scripts, this file): not in the TS project.
    files: ['**/*.mjs', '**/*.js'],
    ...tseslint.configs.disableTypeChecked,
  },
  {
    files: ['tests/**'],
    languageOptions: { globals: { ...globals.node } },
    rules: { 'react/jsx-no-literals': 'off' },
  },
);
