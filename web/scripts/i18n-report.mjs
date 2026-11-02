// Report how many keys id lacks against ja (ADR-0022 §6: not a failure; the
// vitest gate enforces ja/en). In GitHub Actions the line also goes to the
// job summary.
import { appendFileSync, readFileSync } from 'node:fs';

const load = (lang) => JSON.parse(readFileSync(new URL(`../src/i18n/${lang}.json`, import.meta.url), 'utf8'));

function keys(dict, prefix = '') {
  return Object.entries(dict).flatMap(([k, v]) => {
    const key = prefix === '' ? k : `${prefix}.${k}`;
    return typeof v === 'string' ? [key] : keys(v, key);
  });
}

const ja = keys(load('ja'));
const id = new Set(keys(load('id')));
const missing = ja.filter((k) => !id.has(k));
const line = `i18n: id is missing ${missing.length} of ${ja.length} keys (shown in en)`;
console.log(line);
for (const k of missing) console.log(`  ${k}`);
if (process.env.GITHUB_STEP_SUMMARY) {
  appendFileSync(
    process.env.GITHUB_STEP_SUMMARY,
    `### i18n\n\n${line}\n${missing.map((k) => `- \`${k}\`\n`).join('')}\n`,
  );
}
