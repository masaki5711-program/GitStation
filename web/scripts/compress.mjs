// After `vite build`: check dist/ has the shape forge-web embeds and write
// the precompressed variants it serves (ADR-0022 §2).
// - Flat: no sub-directories (an asset URL is one segment, /-/assets/{file}).
// - Only the allow-listed extensions (the same list as forge-web's build.rs).
// - `<file>.br` and `<file>.gz` next to each compressible file, written only
//   when smaller than the original. Node's zlib only: no extra dependency.
import { readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { brotliCompressSync, constants, gzipSync } from 'node:zlib';

import { inlineProblems } from './inline-check.mjs';

const ALLOWED = new Set(['js', 'css', 'woff2', 'svg', 'png', 'ico', 'json', 'txt']);
const COMPRESSIBLE = new Set(['js', 'css', 'svg', 'json', 'txt']);

const dir = process.argv[2];
if (!dir) {
  console.error('usage: node scripts/compress.mjs <dist>');
  process.exit(2);
}

const problems = [];
let written = 0;
for (const name of readdirSync(dir)) {
  const path = join(dir, name);
  if (statSync(path).isDirectory()) {
    problems.push(`${name}: a directory (assets must be flat)`);
    continue;
  }
  if (name === 'index.html') continue;
  const ext = name.includes('.') ? name.slice(name.lastIndexOf('.') + 1) : '';
  if (ext === 'br' || ext === 'gz') {
    problems.push(`${name}: a compressed file before compression (stale dist/?)`);
    continue;
  }
  if (!ALLOWED.has(ext)) {
    problems.push(`${name}: extension .${ext} is not allow-listed`);
    continue;
  }
  if (!COMPRESSIBLE.has(ext)) continue;
  const data = readFileSync(path);
  const br = brotliCompressSync(data, {
    params: {
      [constants.BROTLI_PARAM_QUALITY]: 11,
      [constants.BROTLI_PARAM_SIZE_HINT]: data.length,
    },
  });
  const gz = gzipSync(data, { level: 9 });
  if (br.length < data.length) {
    writeFileSync(`${path}.br`, br);
    written += 1;
  }
  if (gz.length < data.length) {
    writeFileSync(`${path}.gz`, gz);
    written += 1;
  }
}

if (!readdirSync(dir).includes('index.html')) {
  problems.push('index.html: missing');
} else {
  for (const p of inlineProblems(readFileSync(join(dir, 'index.html'), 'utf8'))) {
    problems.push(`index.html: ${p} (the shell CSP allows none)`);
  }
}
if (problems.length > 0) {
  for (const p of problems) console.error(`dist: ${p}`);
  process.exit(1);
}
console.log(`dist: ${written} precompressed files written`);
