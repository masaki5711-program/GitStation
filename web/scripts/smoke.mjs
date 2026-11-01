// Browser smoke test of the built UI under the shell CSP (ADR-0022 §3 /
// §3a): load pages in Chromium and fail on any CSP violation, console error
// or page error.
//
//   npm run build && npm run smoke                 # a local stand-in server
//   npm run smoke -- --url http://127.0.0.1:8080   # a running `forge serve`
//                                                  # built with embed-ui
//
// The stand-in (scripts/standin.mjs) serves web/dist the way forge-web does
// (the same bytes for every page path, /-/assets/{file}) with the shell CSP
// read from crates/forge-web/src/security_headers.rs, so the policy has one
// source. Signed out, every page shows the sign-in screen.
// Playwright is a pinned devDependency (in the lockfile); the browser comes
// from `npx playwright install chromium` (never an install script).
//
// Before the browser runs, dist/index.html is checked for inline <script>
// and <style> (the shell CSP allows neither).
import { readFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';

import { chromium } from 'playwright';

import { inlineProblems } from './inline-check.mjs';
import { dist, listen, sendJson, shellCsp, standIn } from './standin.mjs';


const C = 'a'.repeat(40);
const REV = { type: 'branch', name: 'main', full_name: 'refs/heads/main', commit: C };

/** A signed-in person and one project, so the pages render without a
 * failed request (a 4xx is logged by the browser as a console error). */
function smokeApi(_req, res, url) {
  const path = url.pathname;
  if (path === '/api/v1/session') {
    sendJson(res, 200, {
      absolute_expires_at: '2026-11-02T00:00:00Z',
      idle_expires_at: '2026-11-01T04:00:00Z',
      public_id: '0123456789abcdef',
      csrf_token: 'c'.repeat(64),
      user: { id: 1, username: 'taro', display_name: 'Taro Yamada', is_admin: false },
    });
  } else if (path === '/api/v1/projects' || path === '/api/v1/groups') {
    sendJson(res, 200, { items: [], has_more: false, next_cursor: null });
  } else if (path === '/api/v1/namespaces/resolve') {
    sendJson(res, 200, {
      canonical_path: 'acme/forge-web',
      kind: 'project',
      moved: false,
      namespace_id: 11,
      project_id: 1,
    });
  } else if (path === '/api/v1/projects/1') {
    sendJson(res, 200, {
      id: 1,
      namespace_id: 11,
      path: 'acme/forge-web',
      name: 'forge-web',
      display_name: 'Forge Web',
      description: '',
      visibility: 'internal',
      kind: 'system',
      created_at: '2026-10-01T00:00:00Z',
      role: 'developer',
    });
  } else if (path === '/api/v1/projects/1/repository/refs') {
    sendJson(res, 200, {
      default_branch: 'main',
      truncated: false,
      refs: [{ name: 'main', full_name: 'refs/heads/main', type: 'branch', target: C, commit: C }],
    });
  } else if (path === '/api/v1/projects/1/repository/resolve') {
    const spec = url.searchParams.get('spec') ?? '';
    const [, ...rest] = spec.split('/');
    sendJson(res, 200, { ref: REV, path: rest.join('/') });
  } else if (path === '/api/v1/projects/1/repository/tree') {
    const entry = (name, type, size) => ({ name, path: name, type, mode: '100644', oid: 'e'.repeat(40), size });
    sendJson(res, 200, {
      ref: REV,
      path: '',
      oid: 'f'.repeat(40),
      truncated: false,
      entries: [entry('README.md', 'blob', 40), entry('lib.rs', 'blob', 30)],
    });
  } else if (path === '/api/v1/projects/1/repository/blob') {
    const name = url.searchParams.get('path') ?? '';
    const content =
      name === 'README.md' ? '# Readme\n\nA [link](lib.rs) and `code`.\n\n```rust\nfn main() {}\n```\n' : 'fn main() {\n    // hi\n}\n';
    sendJson(res, 200, {
      ref: REV, path: name, name, type: 'blob', mode: '100644', oid: 'e'.repeat(40),
      size: content.length, binary: false, content, truncated: false,
    });
  } else {
    return false;
  }
  return true;
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
    server = standIn(shellCsp(), smokeApi);
    base = await listen(server);
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
    for (const path of ['/', '/acme/forge-web/-/tree/main', '/acme/forge-web/-/blob/main/lib.rs', '/-/projects/1']) {
      const res = await page.goto(base + path, { waitUntil: 'networkidle' });
      if (res === null || res.status() !== 200) {
        problems.push(`${path}: status ${res?.status() ?? 'none'}`);
        continue;
      }
      await page.waitForSelector('header img', { timeout: 10_000 });
      await page.waitForSelector('main h1', { timeout: 10_000 });
      // The lazy chunks (syntax colours, Markdown) load under the shell CSP.
      if (path.includes('/-/blob/')) await page.waitForSelector('.hljs-keyword', { timeout: 10_000 });
      if (path.includes('/-/tree/')) await page.waitForSelector('[data-testid=markdown]', { timeout: 10_000 });
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
