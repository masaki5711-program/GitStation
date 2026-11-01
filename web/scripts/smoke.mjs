// Browser smoke test of the built UI under the shell CSP (ADR-0022 §3 /
// §3a): load pages in Chromium and fail on any CSP violation, console error
// or page error.
//
//   npm run build && npm run smoke                 # a local stand-in server
//   npm run smoke -- --url http://127.0.0.1:8080   # a running `forge serve`
//                                                  # built with embed-ui
//
// The stand-in serves web/dist the way forge-web does (the same bytes for
// every page path, /-/assets/{file}) with the shell CSP read from
// crates/forge-web/src/security_headers.rs, so the policy has one source.
// Playwright is a pinned devDependency (in the lockfile); the browser comes
// from `npx playwright install chromium` (never an install script).
//
// Before the browser runs, dist/index.html is checked for inline <script>
// and <style> (the shell CSP allows neither).
import { readFileSync, existsSync } from 'node:fs';
import { createServer } from 'node:http';
import { join } from 'node:path';

import { chromium } from 'playwright';

import { inlineProblems } from './inline-check.mjs';

const root = new URL('..', import.meta.url).pathname;
const dist = join(root, 'dist');

function shellCsp() {
  const src = readFileSync(join(root, '../crates/forge-web/src/security_headers.rs'), 'utf8');
  const m = src.match(/const CSP_SHELL_BASE: &str = "([\s\S]*?)";/);
  if (!m) throw new Error('CSP_SHELL_BASE not found in security_headers.rs');
  return m[1].replace(/\\\n\s*/g, '');
}

const TYPES = {
  js: 'text/javascript; charset=utf-8',
  css: 'text/css; charset=utf-8',
  svg: 'image/svg+xml',
  png: 'image/png',
  json: 'application/json',
};

function standIn(csp) {
  const index = readFileSync(join(dist, 'index.html'));
  return createServer((req, res) => {
    const path = new URL(req.url ?? '/', 'http://x').pathname;
    const common = {
      'X-Content-Type-Options': 'nosniff',
      'Referrer-Policy': 'same-origin',
      'Cross-Origin-Opener-Policy': 'same-origin',
    };
    if (path.startsWith('/api/')) {
      res.writeHead(200, {
        ...common,
        'Content-Type': 'application/json',
        'Content-Security-Policy': "default-src 'none'; frame-ancestors 'none'",
      });
      res.end(JSON.stringify({ items: [], has_more: false, next_cursor: null }));
      return;
    }
    if (path.startsWith('/-/assets/')) {
      const name = path.slice('/-/assets/'.length);
      const file = join(dist, name);
      const ext = name.slice(name.lastIndexOf('.') + 1);
      if (name.includes('/') || name === 'index.html' || !TYPES[ext] || !existsSync(file)) {
        res.writeHead(404, common);
        res.end();
        return;
      }
      res.writeHead(200, { ...common, 'Content-Type': TYPES[ext], 'Content-Security-Policy': csp });
      res.end(readFileSync(file));
      return;
    }
    res.writeHead(200, {
      ...common,
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Security-Policy': csp,
      'Cache-Control': 'no-cache',
    });
    res.end(index);
  });
}

async function main() {
  const urlArg = process.argv.indexOf('--url');
  let base = urlArg > 0 ? process.argv[urlArg + 1] : undefined;
  let server = null;
  if (base === undefined) {
    if (!existsSync(join(dist, 'index.html'))) {
      console.error('smoke: web/dist is missing (npm run build first)');
      process.exit(1);
    }
    const inline = inlineProblems(readFileSync(join(dist, 'index.html'), 'utf8'));
    if (inline.length > 0) {
      for (const p of inline) console.error(`smoke: dist/index.html: ${p}`);
      process.exit(1);
    }
    server = standIn(shellCsp());
    await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
    base = `http://127.0.0.1:${server.address().port}`;
  }

  const browser = await chromium.launch();
  const problems = [];
  try {
    const page = await browser.newPage();
    page.on('console', (msg) => {
      if (msg.type() === 'error' || /Content Security Policy/i.test(msg.text())) {
        problems.push(`console ${msg.type()}: ${msg.text()}`);
      }
    });
    page.on('pageerror', (err) => problems.push(`page error: ${err.message}`));
    await page.exposeFunction('__cspViolation', (v) => problems.push(`CSP violation: ${v}`));
    await page.addInitScript(() => {
      document.addEventListener('securitypolicyviolation', (e) => {
        // @ts-expect-error exposed by the smoke test
        window.__cspViolation(`${e.violatedDirective} ${e.blockedURI}`);
      });
    });
    for (const path of ['/', '/acme/forge-web/-/tree/main', '/-/projects/1']) {
      const res = await page.goto(base + path, { waitUntil: 'networkidle' });
      if (res === null || res.status() !== 200) {
        problems.push(`${path}: status ${res?.status() ?? 'none'}`);
        continue;
      }
      await page.waitForSelector('header img', { timeout: 10_000 });
      await page.waitForSelector('main h1', { timeout: 10_000 });
      const styles = await page.evaluate(() => document.querySelectorAll('style').length);
      if (styles !== 0) problems.push(`${path}: ${styles} <style> elements in the document`);
      console.log(`smoke: ${path} ok (${await page.locator('main h1').innerText()})`);
    }
  } finally {
    await browser.close();
    server?.close();
  }
  if (problems.length > 0) {
    for (const p of problems) console.error(`smoke: ${p}`);
    process.exit(1);
  }
  console.log('smoke: no CSP violations, console errors or page errors');
}

await main();
