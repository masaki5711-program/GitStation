// @vitest-environment node
// public/theme-init-<hash>.js: the classic script in <head> that applies the
// remembered theme before the first paint (no wrong-theme flash; the CSP
// forbids an inline script, so it is a same-origin file).
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { runInNewContext } from 'node:vm';
import { describe, expect, it } from 'vitest';

import { THEMES, THEME_STORAGE_KEY } from '../src/theme';

const web = join(import.meta.dirname, '..');
const html = readFileSync(join(web, 'index.html'), 'utf8');
const scripts = readdirSync(join(web, 'public')).filter((f) => /^theme-init-.*\.js$/.test(f));
const name = scripts[0] ?? '';
const code = readFileSync(join(web, 'public', name), 'utf8');

/** Run the script against a fake page; the data-theme it set, if any. */
function run(getItem: () => string | null): string | undefined {
  const attrs: Record<string, string> = {};
  runInNewContext(code, {
    window: { localStorage: { getItem } },
    document: {
      documentElement: {
        setAttribute: (k: string, v: string) => {
          attrs[k] = v;
        },
      },
    },
  });
  return attrs['data-theme'];
}

describe('theme-init script', () => {
  it('is one file whose name carries its content hash (it is cached immutable)', () => {
    expect(scripts).toHaveLength(1);
    const hash = createHash('sha256').update(readFileSync(join(web, 'public', name))).digest('hex');
    expect(name).toBe(`theme-init-${hash.slice(0, 8)}.js`);
  });

  it('index.html loads it as a classic script in <head>, before the app', () => {
    const head = html.slice(0, html.indexOf('</head>'));
    expect(head).toContain(`<script src="/${name}"></script>`);
    expect(head).not.toMatch(/<script(?![^>]*\bsrc=)/); // no inline script
    expect(html.indexOf(name)).toBeLessThan(html.indexOf('type="module"'));
  });

  it('index.html declares both colour schemes', () => {
    expect(html).toContain('<meta name="color-scheme" content="light dark" />');
  });

  it('uses the same storage key as src/theme.ts', () => {
    expect(code).toContain(`'${THEME_STORAGE_KEY}'`);
  });

  it.each(THEMES)('applies the stored %s', (theme) => {
    expect(run(() => theme)).toBe(theme);
  });

  it('ignores nothing stored, an unknown value and disabled storage', () => {
    expect(run(() => null)).toBeUndefined();
    expect(run(() => 'pink')).toBeUndefined();
    expect(
      run(() => {
        throw new Error('denied');
      }),
    ).toBeUndefined();
  });
});
