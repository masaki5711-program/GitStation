// After `vite build`: the first load's JavaScript stays within its budget.
// What the browser must fetch before the first screen is the entry chunk and
// the chunks the shell preloads (`<script type="module">` and
// `<link rel="modulepreload">` in dist/index.html); lazy chunks (Markdown,
// languages, settings, pipelines, the editor…) are not counted.
//
//   node scripts/bundle-budget.mjs dist            # fails over the budget
//   BUDGET_KB=600 node scripts/bundle-budget.mjs dist
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { gzipSync } from 'node:zlib';

/** Raw (minified, uncompressed) kB of the first load. */
const BUDGET_KB = Number(process.env.BUDGET_KB ?? 680);

const dir = process.argv[2];
if (!dir) {
  console.error('usage: node scripts/bundle-budget.mjs <dist>');
  process.exit(2);
}

const html = readFileSync(join(dir, 'index.html'), 'utf8');
const files = new Set();
for (const m of html.matchAll(/<script[^>]+type="module"[^>]+src="([^"]+)"/g)) files.add(m[1]);
for (const m of html.matchAll(/<link[^>]+rel="modulepreload"[^>]+href="([^"]+)"/g)) files.add(m[1]);
if (files.size === 0) {
  console.error('bundle-budget: no module script in dist/index.html');
  process.exit(1);
}

let raw = 0;
let gz = 0;
for (const url of files) {
  const name = url.split('/').pop();
  const bytes = readFileSync(join(dir, name));
  raw += bytes.length;
  gz += gzipSync(bytes, { level: 9 }).length;
  console.log(`  ${name}: ${(bytes.length / 1024).toFixed(1)} kB`);
}
const kb = raw / 1024;
console.log(
  `bundle-budget: first load ${kb.toFixed(1)} kB (gzip ${(gz / 1024).toFixed(1)} kB), budget ${BUDGET_KB} kB`,
);
if (kb > BUDGET_KB) {
  console.error('bundle-budget: over the budget — split a rarely used screen (src/pages/lazyScreens.tsx)');
  process.exit(1);
}
