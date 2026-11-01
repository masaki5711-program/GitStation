// @vitest-environment node
// The eslint rules that keep text in the dictionaries and style injection
// out (ADR-0022 §1 / §3a / §6) really fire: each snippet is linted as if it
// were a source file.
import { join } from 'node:path';

import { ESLint } from 'eslint';
import { describe, expect, it } from 'vitest';

const eslint = new ESLint({
  cwd: join(import.meta.dirname, '..'),
  overrideConfigFile: join(import.meta.dirname, '../eslint.config.js'),
});

/** Rule ids reported for `code` linted as `file` (an existing source path,
 * so the TypeScript project service knows it). */
async function rulesFor(code: string, file = 'src/pages/NotFound.tsx'): Promise<string[]> {
  const [result] = await eslint.lintText(code, { filePath: file });
  return (result?.messages ?? []).map((m) => m.ruleId ?? 'parse');
}

const component = (jsx: string) =>
  `export function X() {\n  return (${jsx});\n}\n`;

describe('eslint: user-facing text comes from the dictionaries', () => {
  it.each([
    ['a child string', '<p>Hello</p>', 'react/jsx-no-literals'],
    ['a string in a prop expression', "<p title={'Hello'} />", 'no-restricted-syntax'],
    ['alt text', '<img alt="Logo" src="x" />', 'no-restricted-syntax'],
    ['an aria-label', '<button aria-label="Close" type="button" />', 'no-restricted-syntax'],
    ['a placeholder', '<input placeholder="Name" />', 'no-restricted-syntax'],
    ['a template title', '<p title={`Hi`} />', 'no-restricted-syntax'],
  ])('refuses %s', async (_name, jsx, rule) => {
    expect(await rulesFor(component(jsx))).toContain(rule);
  });

  it('accepts structural attributes and separators', async () => {
    expect(
      await rulesFor(component('<p role="status" id="x" className="y">{\': \'}</p>')),
    ).toEqual([]);
  });
});

describe('eslint: no runtime style injection, Radix only in src/ui/', () => {
  it.each(['@emotion/react', 'styled-components', 'goober', '@mui/material', '@chakra-ui/react', '@stitches/react'])(
    'refuses %s everywhere',
    async (mod) => {
      const code = `import x from '${mod}';\nexport const y = x;\n`;
      expect(await rulesFor(code)).toContain('no-restricted-imports');
    },
  );

  it('refuses Radix outside src/ui/', async () => {
    const code = "import * as Menu from '@radix-ui/react-dropdown-menu';\nexport const y = Menu;\n";
    expect(await rulesFor(code)).toContain('no-restricted-imports');
  });
});
