// The colour tokens (ADR-0022 §4): tokens.css is the one definition,
// tokens.json mirrors it, the brand values equal docs/brand/README.md, and
// every text/background pair meets WCAG 2.2 AA.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

import tokens from '../src/styles/tokens.json';

const css = readFileSync(join(import.meta.dirname, '../src/styles/tokens.css'), 'utf8');
const brandReadme = readFileSync(join(import.meta.dirname, '../../docs/brand/README.md'), 'utf8');

function cssColours(): Map<string, string> {
  const out = new Map<string, string>();
  for (const m of css.matchAll(/(--gs-[a-z0-9-]+):\s*(#[0-9a-fA-F]{6})\s*;/g)) {
    if (m[1] !== undefined && m[2] !== undefined) out.set(m[1], m[2].toLowerCase());
  }
  return out;
}

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

const colours = cssColours();
const value = (name: string): string => {
  const v = colours.get(name);
  if (v === undefined) throw new Error(`no token ${name}`);
  return v;
};

describe('tokens', () => {
  it('tokens.json mirrors every colour in tokens.css', () => {
    expect(Object.fromEntries(colours)).toEqual(tokens.color);
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

  it.each(tokens.textPairs)('text %s on %s is at least 4.5:1', (fg, bg) => {
    expect(contrast(value(fg), value(bg))).toBeGreaterThanOrEqual(4.5);
  });

  it.each(tokens.largeTextPairs)('large text %s on %s is at least 3:1', (fg, bg) => {
    expect(contrast(value(fg), value(bg))).toBeGreaterThanOrEqual(3);
  });

  it('the calculation matches known ratios', () => {
    expect(contrast('#000000', '#ffffff')).toBeCloseTo(21, 5);
    expect(contrast('#ffffff', '#ffffff')).toBeCloseTo(1, 5);
  });

  it('only system fonts: no @font-face or url() in the tokens', () => {
    expect(css).not.toMatch(/@font-face|url\(/);
  });
});
