// ADR-0022 §1 / §3a / §6.
// - `react/jsx-no-literals` + `no-restricted-syntax`: no hard-coded
//   user-facing text in JSX children, prop expressions or the attributes a
//   person reads or hears (alt, title, placeholder, aria-label, …) (i18n).
// - `no-restricted-imports`: only src/ui/ may import Radix (the wrappers keep
//   menus `modal={false}` and avoid components that inject <style>); no
//   library that injects <style> at run time anywhere.
// - No dangerouslySetInnerHTML.
import js from '@eslint/js';
import globals from 'globals';
import react from 'eslint-plugin-react';
import reactHooks from 'eslint-plugin-react-hooks';
import tseslint from 'typescript-eslint';

const STYLE_INJECTION =
  'No runtime style injection (CSS-in-JS or UI kits that insert <style>; CSP style-src self, ADR-0022 §1).';

/** Never imported (src/ui/ included). */
const STYLE_INJECTING = {
  paths: ['styled-components', 'goober', 'styled-jsx', 'aphrodite', 'jss', 'react-jss'].map(
    (name) => ({ name, message: STYLE_INJECTION }),
  ),
  patterns: [
    {
      group: [
        '@emotion/*',
        '@stitches/*',
        '@mui/*',
        '@chakra-ui/*',
        '@mantine/*',
        '@vanilla-extract/dynamic',
        'antd',
        'antd/*',
        'styled-components/*',
        'goober/*',
      ],
      message: STYLE_INJECTION,
    },
  ],
};

const RADIX = {
  group: ['@radix-ui/*', 'radix-ui', 'radix-ui/*'],
  message: 'Import Radix only through the wrappers in src/ui/ (ADR-0022 §3a).',
};

/** Attributes whose value a person reads or hears. */
const TEXT_ATTRIBUTES =
  '/^(alt|title|placeholder|label|aria-label|aria-description|aria-placeholder|aria-roledescription|aria-valuetext)$/';
const TEXT_ATTRIBUTE_MESSAGE = 'User-facing attribute text must come from t() (ADR-0022 §6).';

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
          // `ignoreProps: false` would flag every structural attribute
          // (id, role, href) in this plugin version: the attributes that
          // carry text are checked by no-restricted-syntax below instead.
          ignoreProps: true,
          noAttributeStrings: false,
          // Symbols and separators only; words come from the dictionaries.
          allowedStrings: ['·', '—', '/', ':', '(', ')', '#', '…', '|'],
        },
      ],
      // jsx-no-literals leaves plain attribute strings (type="button",
      // role="alert"): the text-bearing attributes are checked here.
      'no-restricted-syntax': [
        'error',
        {
          selector: `JSXAttribute[name.name=${TEXT_ATTRIBUTES}] > Literal`,
          message: TEXT_ATTRIBUTE_MESSAGE,
        },
        {
          selector: `JSXAttribute[name.name=${TEXT_ATTRIBUTES}] > JSXExpressionContainer > :matches(Literal, TemplateLiteral)`,
          message: TEXT_ATTRIBUTE_MESSAGE,
        },
      ],
      'react/no-danger': 'error',
      'no-restricted-imports': [
        'error',
        { paths: STYLE_INJECTING.paths, patterns: [...STYLE_INJECTING.patterns, RADIX] },
      ],
      '@typescript-eslint/restrict-template-expressions': ['error', { allowNumber: true }],
    },
  },
  {
    // The wrappers themselves may import Radix (never a style injector).
    files: ['src/ui/**'],
    rules: {
      'no-restricted-imports': ['error', STYLE_INJECTING],
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
    rules: { 'react/jsx-no-literals': 'off', 'no-restricted-syntax': 'off' },
  },
);
