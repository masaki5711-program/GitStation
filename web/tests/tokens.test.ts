// The colour tokens (ADR-0022 §4): tokens.css is the one definition,
// tokens.json mirrors it, the brand values equal docs/brand/README.md, and
// every text/background pair meets WCAG 2.2 AA in every theme (ホワイト,
// ネイビー, ダーク, and システム's dark copy).
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

import tokens from '../src/styles/tokens.json';

const css = readFileSync(join(import.meta.dirname, '../src/styles/tokens.css'), 'utf8');
const brandReadme = readFileSync(join(import.meta.dirname, '../../docs/brand/README.md'), 'utf8');

/** The declarations of the rule `selector { ... }` (no nested braces). */
function ruleBody(selector: string, from = css): string {
  const at = from.indexOf(`${selector} {`);
  if (at < 0) throw new Error(`no rule ${selector}`);
  const start = at + selector.length + 2;
  return from.slice(start, from.indexOf('}', start));
}

function hexColours(body: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const m of body.matchAll(/(--gs-[a-z0-9-]+):\s*(#[0-9a-fA-F]{6})\s*;/g)) {
    if (m[1] !== undefined && m[2] !== undefined) out.set(m[1], m[2].toLowerCase());
  }
  return out;
}

/** Every declaration (name → value, whitespace collapsed). */
function declarations(body: string): Map<string, string> {
  const out = new Map<string, string>();
  const plain = body.replace(/\/\*[\s\S]*?\*\//g, '');
  for (const m of plain.matchAll(/([a-z0-9-]+):\s*([^;]+);/g)) {
    if (m[1] !== undefined && m[2] !== undefined) out.set(m[1], m[2].replace(/\s+/g, ' ').trim());
  }
  return out;
}

const whiteBody = ruleBody(':root');
const navyBody = ruleBody(":root[data-theme='navy']");
const darkBody = ruleBody(":root[data-theme='dark']");
const mediaAt = css.indexOf('@media (prefers-color-scheme: dark) {');
const systemDarkBody = ruleBody(':root:not([data-theme])', css.slice(mediaAt));

const white = hexColours(whiteBody);
/** Colours that are the same in every theme: the logo's, pure white and
 * the theme choice's previews. */
const fixed = (name: string) =>
  name.startsWith('--gs-brand-') || name.startsWith('--gs-swatch-') || name === '--gs-color-white';

const themes: Record<string, Map<string, string>> = {
  white,
  navy: new Map([...white, ...hexColours(navyBody)]),
  dark: new Map([...white, ...hexColours(darkBody)]),
};

function luminance(hex: string): number {
  const channel = (i: number) => {
    const c = parseInt(hex.slice(1 + 2 * i, 3 + 2 * i), 16) / 255;
    return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * channel(0) + 0.7152 * channel(1) + 0.0722 * channel(2);
}

export function contrast(a: string, b: string): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x) as [number, number];
  return (hi + 0.05) / (lo + 0.05);
}

const valueIn =
  (theme: string) =>
  (name: string): string => {
    const v = themes[theme]?.get(name);
    if (v === undefined) throw new Error(`no token ${name} in ${theme}`);
    return v;
  };
const value = valueIn('white');
const themeNames = Object.keys(themes);
const withThemes = (pairs: string[][]): [string, string, string][] =>
  themeNames.flatMap((theme) => pairs.map(([fg, bg]): [string, string, string] => [theme, fg ?? '', bg ?? '']));

describe('tokens', () => {
  it('tokens.json mirrors every colour in tokens.css', () => {
    expect(Object.fromEntries(white)).toEqual(tokens.color);
    expect(Object.fromEntries(hexColours(navyBody))).toEqual(tokens.themes.navy);
    expect(Object.fromEntries(hexColours(darkBody))).toEqual(tokens.themes.dark);
  });

  it.each(['navy', 'dark'])('the %s theme overrides every themed colour', (theme) => {
    const own = hexColours(theme === 'navy' ? navyBody : darkBody);
    const missing = [...white.keys()].filter((name) => !fixed(name) && !own.has(name));
    const stray = [...own.keys()].filter((name) => fixed(name) || !white.has(name));
    expect({ missing, stray }).toEqual({ missing: [], stray: [] });
  });

  it.each(['navy', 'dark'])('the %s theme also sets its shadows, scrim and color-scheme', (theme) => {
    const own = declarations(theme === 'navy' ? navyBody : darkBody);
    for (const name of ['--gs-color-scrim', '--gs-shadow-1', '--gs-shadow-2', '--gs-shadow-3']) {
      expect(own.get(name), name).toBeDefined();
    }
    expect(own.get('color-scheme')).toBe('dark');
    expect(declarations(whiteBody).get('color-scheme')).toBe('light');
  });

  it('システムに合わせる on a dark OS is exactly the dark theme', () => {
    expect(mediaAt).toBeGreaterThan(0);
    expect(css.slice(mediaAt)).toContain(":root[data-theme='system'],");
    expect(declarations(systemDarkBody)).toEqual(declarations(darkBody));
  });

  it('the header keeps the logo navy on the left in every theme', () => {
    for (const theme of themeNames) {
      expect(valueIn(theme)('--gs-header-background')).toBe(value('--gs-brand-navy'));
    }
  });

  it('the brand colours are the docs/brand/README.md values', () => {
    const brand: Record<string, string> = {
      '--gs-brand-navy': '#1B2A44',
      '--gs-brand-teal': '#22B8A0',
      '--gs-brand-teal-strong': '#128F7C',
      '--gs-brand-orange': '#FF7A45',
      '--gs-brand-paper': '#F4FBFC',
    };
    for (const [name, hex] of Object.entries(brand)) {
      expect(brandReadme).toContain(`\`${hex}\``);
      expect(value(name)).toBe(hex.toLowerCase());
    }
  });

  it.each(withThemes(tokens.textPairs))('%s: text %s on %s is at least 4.5:1', (theme, fg, bg) => {
    const v = valueIn(theme);
    expect(contrast(v(fg), v(bg))).toBeGreaterThanOrEqual(4.5);
  });

  it.each(withThemes(tokens.nonTextPairs))(
    '%s: state indicator %s on %s is at least 3:1',
    (theme, fg, bg) => {
      const v = valueIn(theme);
      expect(contrast(v(fg), v(bg))).toBeGreaterThanOrEqual(3);
    },
  );

  it.each(withThemes(tokens.largeTextPairs))(
    '%s: large text %s on %s is at least 3:1',
    (theme, fg, bg) => {
      const v = valueIn(theme);
      expect(contrast(v(fg), v(bg))).toBeGreaterThanOrEqual(3);
    },
  );

  it('the calculation matches known ratios', () => {
    expect(contrast('#000000', '#ffffff')).toBeCloseTo(21, 5);
    expect(contrast('#ffffff', '#ffffff')).toBeCloseTo(1, 5);
  });

  it('only system fonts: no @font-face or url() in the tokens', () => {
    expect(css).not.toMatch(/@font-face|url\(/);
  });
});

describe('component styles use the tokens', () => {
  const src = join(import.meta.dirname, '../src');
  const cssFiles = (dir: string): string[] =>
    readdirSync(dir).flatMap((name) => {
      const path = join(dir, name);
      if (statSync(path).isDirectory()) return cssFiles(path);
      return name.endsWith('.css') && !path.endsWith(join('styles', 'tokens.css')) ? [path] : [];
    });
  const files = cssFiles(src).map((f) => f.slice(src.length + 1));

  it('finds the component styles', () => {
    expect(files.length).toBeGreaterThan(10);
  });

  it.each(files)('%s has no raw colour', (file) => {
    const text = readFileSync(join(src, file), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
    expect(text).not.toMatch(/#[0-9a-fA-F]{3,8}\b|\b(rgba?|hsla?|hwb|lab|lch|oklab|oklch)\(/);
  });

  it.each(files)('%s uses no light-theme-only colour (brand navy, white)', (file) => {
    expect(readFileSync(join(src, file), 'utf8')).not.toMatch(/var\(--gs-(brand-navy|color-white)\)/);
  });
});
