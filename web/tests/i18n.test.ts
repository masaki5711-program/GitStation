// The dictionary gate (ADR-0022 §6):
// - ja and en carry every key, no empty value, the same keys on both sides;
// - every key the source uses exists, every key in the dictionaries is used;
// - id may lag: its missing keys fall back to en and are counted (CI shows
//   the number); an id key that ja/en do not have is an error.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

import en from '../src/i18n/en.json';
import id from '../src/i18n/id.json';
import ja from '../src/i18n/ja.json';
import { createI18n, detectLanguage } from '../src/i18n';

type Dict = { [key: string]: string | Dict };

function flatten(dict: Dict, prefix = ''): Map<string, string> {
  const out = new Map<string, string>();
  for (const [k, v] of Object.entries(dict)) {
    const key = prefix === '' ? k : `${prefix}.${k}`;
    if (typeof v === 'string') out.set(key, v);
    else for (const [kk, vv] of flatten(v, key)) out.set(kk, vv);
  }
  return out;
}

function sourceFiles(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return sourceFiles(path);
    return /\.(ts|tsx)$/.test(name) && !name.endsWith('.gen.ts') ? [path] : [];
  });
}

/** `t('a.b')` keys in the source (string literals only: a dynamic key
 * would escape this gate, so the source never builds one). */
function usedKeys(): Set<string> {
  const keys = new Set<string>();
  const re = /\bt\(\s*'([a-zA-Z0-9_.]+)'/g;
  for (const file of sourceFiles(join(import.meta.dirname, '../src'))) {
    for (const m of readFileSync(file, 'utf8').matchAll(re)) {
      if (m[1] !== undefined) keys.add(m[1]);
    }
  }
  return keys;
}

const jaKeys = flatten(ja);
const enKeys = flatten(en);
const idKeys = flatten(id);

describe('i18n dictionaries', () => {
  it('ja and en have the same keys', () => {
    const onlyJa = [...jaKeys.keys()].filter((k) => !enKeys.has(k));
    const onlyEn = [...enKeys.keys()].filter((k) => !jaKeys.has(k));
    expect({ onlyJa, onlyEn }).toEqual({ onlyJa: [], onlyEn: [] });
  });

  it('no empty values in any language', () => {
    const empty = [
      ...[...jaKeys].filter(([, v]) => v.trim() === '').map(([k]) => `ja:${k}`),
      ...[...enKeys].filter(([, v]) => v.trim() === '').map(([k]) => `en:${k}`),
      ...[...idKeys].filter(([, v]) => v.trim() === '').map(([k]) => `id:${k}`),
    ];
    expect(empty).toEqual([]);
  });

  it('every key the source uses exists in ja and en', () => {
    const missing = [...usedKeys()].filter((k) => !jaKeys.has(k) || !enKeys.has(k));
    expect(missing).toEqual([]);
  });

  it('every dictionary key is used by the source', () => {
    const used = usedKeys();
    const unused = [...jaKeys.keys()].filter((k) => !used.has(k));
    expect(unused).toEqual([]);
  });

  it('id has no key unknown to ja; its missing keys are reported', () => {
    const unknown = [...idKeys.keys()].filter((k) => !jaKeys.has(k));
    expect(unknown).toEqual([]);
    const missing = [...jaKeys.keys()].filter((k) => !idKeys.has(k));
    // Not a failure (ADR-0022 §6): CI shows this line.
    console.log(`i18n: id is missing ${missing.length} of ${jaKeys.size} keys`);
    for (const k of missing) console.log(`i18n: id missing ${k}`);
  });

  it('screens that must be complete in id are (login, errors; ADR-0022 §6)', () => {
    const required = [...jaKeys.keys()].filter(
      (k) => k.startsWith('error.') || k.startsWith('login.') || k.startsWith('portal.'),
    );
    expect(required.filter((k) => !idKeys.has(k))).toEqual([]);
  });
});

describe('language selection', () => {
  it('prefers the stored choice, then the browser, then ja', () => {
    expect(detectLanguage('en', ['id-ID'])).toBe('en');
    expect(detectLanguage(null, ['id-ID', 'en-US'])).toBe('id');
    expect(detectLanguage(null, ['fr-FR', 'en-GB'])).toBe('en');
    expect(detectLanguage(null, ['fr-FR'])).toBe('ja');
    expect(detectLanguage('xx', [])).toBe('ja');
  });

  it('a key missing in id falls back to en', async () => {
    const i18n = await createI18n('id');
    i18n.addResourceBundle('en', 'translation', { probe: { onlyEn: 'english' } }, true);
    expect(i18n.t('probe.onlyEn')).toBe('english');
  });
});
