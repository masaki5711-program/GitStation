// Licence allow-list for the npm dependencies (ADR-0022 §8, the same idea as
// deny.toml): every package in package-lock.json must carry an allowed SPDX
// expression. Reads the lockfile and node_modules/*/package.json only.
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

const ALLOWED = new Set([
  'MIT',
  'ISC',
  'BSD-2-Clause',
  'BSD-3-Clause',
  'Apache-2.0',
  '0BSD',
  'CC0-1.0',
  // Reviewed for M2 MR2 (2026-11-01); all build / test tools, none shipped
  // in web/dist except as noted:
  'MIT-0', // @csstools/color-helpers (jsdom's CSS parser; dev only)
  'BlueOak-1.0.0', // a glob helper (dev only; permissive, MIT-like)
  'Python-2.0', // argparse (eslint; dev only; permissive)
  'CC-BY-4.0', // caniuse-lite data (browserslist at build time; not shipped)
  'MPL-2.0', // axe-core (a11y checks in vitest and the Playwright journey; dev only, file-level copyleft, unmodified, never shipped; reviewed for M2 MR9)
]);

const root = new URL('..', import.meta.url).pathname;
const lock = JSON.parse(readFileSync(join(root, 'package-lock.json'), 'utf8'));

/** `(MIT OR Apache-2.0)`: allowed when one alternative is; `AND`: all. */
function allowed(expr) {
  const e = expr.replace(/[()]/g, '').trim();
  if (e.includes(' OR ')) return e.split(' OR ').some((p) => allowed(p));
  if (e.includes(' AND ')) return e.split(' AND ').every((p) => allowed(p));
  return ALLOWED.has(e);
}

const problems = [];
let count = 0;
for (const [path, meta] of Object.entries(lock.packages ?? {})) {
  if (path === '' || meta.link) continue;
  count += 1;
  let license = meta.license;
  const pkg = join(root, path, 'package.json');
  if (license === undefined && existsSync(pkg)) {
    const json = JSON.parse(readFileSync(pkg, 'utf8'));
    license = typeof json.license === 'string' ? json.license : json.license?.type;
  }
  if (typeof license !== 'string' || !allowed(license)) {
    problems.push(`${path.replace(/^node_modules\//, '')}: ${license ?? 'no licence'}`);
  }
}
if (problems.length > 0) {
  for (const p of problems) console.error(`licence not allow-listed: ${p}`);
  process.exit(1);
}
console.log(`licences: ${count} packages, all allow-listed`);
