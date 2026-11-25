// The ja dictionary split (ADR-0022 §6): ja is the default language and is
// bundled, but only the strings the first load can show sit in the main
// chunk ("core"); the strings only the lazy screens use ("rest") are a
// separate chunk that a lazy screen waits for before it renders
// (`loadJaRest` in src/i18n/index.ts, src/pages/lazyScreens.tsx).
//
// The split is mechanical, so a new key never needs hand placement:
// - eager files = every src file reachable from src/main.tsx through static
//   imports and through every dynamic import EXCEPT those in
//   src/pages/screenLoaders.ts (the lazy screens);
// - core = every dictionary key an eager file uses as `t('a.b')` (with its
//   plural forms `a.b_one`, `a.b_other`, …); rest = every other key.
// Keys are always string literals (tests/i18n.test.ts fails on an unused
// key, so a key built at run time cannot exist), which makes the scan exact.
// A form the scan cannot read (a double-quoted or template specifier or key)
// is an error, never a file or key silently left out of core.
//
// Used by the Vite plugin in vite.config.ts (`virtual:i18n/ja-core`,
// `virtual:i18n/ja-rest`) and by tests/i18n-split.test.ts.
import { existsSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';

/** `t('a.b')` / `i18n.t('a.b')`: the same pattern as the dictionary gate. */
const T_CALL = /\bt\(\s*'([a-zA-Z0-9_.]+)'/g;
/** Static `import … from '…'`, `import '…'`, `export … from '…'`. */
const STATIC_IMPORT = /(?:^|[\s;])(?:import|export)\s+(?:[^'";]*?\sfrom\s+)?'([^']+)'/g;
/** `import('…')`. */
const DYNAMIC_IMPORT = /\bimport\(\s*'([^']+)'\s*\)/g;
const PLURAL = /_(zero|one|two|few|many|other)$/;
const EXTENSIONS = ['', '.ts', '.tsx', '/index.ts', '/index.tsx'];
/** A double-quoted or template `import(…)` or `t(…)`; a double-quoted
 * `import "…"` or `… from "…"` on an import / export line (`}` ends a
 * multi-line one), so prose in a comment is not taken for one. */
const UNREADABLE = /\bimport\(\s*["`]|\bt\(\s*["`]|^\s*(?:import|export|\})[^\n]*\bfrom\s+"|^\s*import\s+"/m;

/** Throws when `source` uses an import or key form the scan cannot read. */
export function assertReadable(file, source) {
  const m = UNREADABLE.exec(source);
  if (m !== null) {
    const line = source.slice(0, m.index).split('\n').length;
    throw new Error(
      `i18n-split: ${file}:${line}: write a single-quoted string here (${m[0]}…); the ja split cannot read it`,
    );
  }
}

/** The file a relative specifier names, or null (packages, CSS, JSON). */
function resolveImport(from, specifier) {
  if (!specifier.startsWith('.')) return null;
  const base = resolve(dirname(from), specifier);
  for (const ext of EXTENSIONS) {
    const path = base + ext;
    if (/\.(ts|tsx)$/.test(path) && existsSync(path) && statSync(path).isFile()) return path;
  }
  return null;
}

function matches(source, re) {
  return [...source.matchAll(re)].map((m) => m[1]).filter((s) => s !== undefined);
}

/** The keys a source file uses through `t('…')`. */
export function keysInSource(source) {
  return new Set(matches(source, T_CALL));
}

/**
 * The files the first load may execute: the closure of `entry` over static
 * imports and over dynamic imports outside `lazyLoaders`. `lazyRoots` are
 * the files `lazyLoaders` imports dynamically (the lazy screens).
 */
export function eagerFiles(entry, lazyLoaders) {
  const eager = new Set();
  const lazyRoots = new Set();
  const stack = [resolve(entry)];
  const loaders = resolve(lazyLoaders);
  while (stack.length > 0) {
    const file = stack.pop();
    if (eager.has(file)) continue;
    eager.add(file);
    const source = readFileSync(file, 'utf8');
    assertReadable(file, source);
    for (const spec of matches(source, STATIC_IMPORT)) {
      const target = resolveImport(file, spec);
      if (target !== null) stack.push(target);
    }
    for (const spec of matches(source, DYNAMIC_IMPORT)) {
      const target = resolveImport(file, spec);
      if (target === null) continue;
      if (file === loaders) lazyRoots.add(target);
      else stack.push(target);
    }
  }
  return { eager, lazyRoots };
}

/** Every key the given files use. */
export function keysInFiles(files) {
  const keys = new Set();
  for (const file of files) for (const k of keysInSource(readFileSync(file, 'utf8'))) keys.add(k);
  return keys;
}

function flatten(dict, prefix, out) {
  for (const [k, v] of Object.entries(dict)) {
    const key = prefix === '' ? k : `${prefix}.${k}`;
    if (v !== null && typeof v === 'object') flatten(v, key, out);
    else out.set(key, v);
  }
  return out;
}

function unflatten(entries) {
  const out = {};
  for (const [key, value] of entries) {
    const parts = key.split('.');
    const leaf = parts.pop();
    let node = out;
    for (const p of parts) node = node[p] ??= {};
    node[leaf] = value;
  }
  return out;
}

/** Split `dict` into the keys in `used` (with their plural forms) and the rest. */
export function splitDictionary(dict, used) {
  const core = [];
  const rest = [];
  for (const entry of flatten(dict, '', new Map())) {
    const [key] = entry;
    (used.has(key) || used.has(key.replace(PLURAL, '')) ? core : rest).push(entry);
  }
  return { core: unflatten(core), rest: unflatten(rest) };
}

/** The split of `src/i18n/ja.json` for the web app rooted at `srcDir`. */
export function splitJa(srcDir) {
  const ja = JSON.parse(readFileSync(join(srcDir, 'i18n/ja.json'), 'utf8'));
  const { eager } = eagerFiles(join(srcDir, 'main.tsx'), join(srcDir, 'pages/screenLoaders.ts'));
  return splitDictionary(ja, keysInFiles(eager));
}
