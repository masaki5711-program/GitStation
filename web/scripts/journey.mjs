// The browser journey of M2 MR9 (plan 10 §6): sign in -> home -> project, plus
// creating a project and signing out, in Chromium under the shell CSP, against
// a stand-in backend (scripts/standin.mjs; no real forge or Keycloak needed).
// axe runs in the real browser on every screen with every WCAG 2.2 AA rule on
// (colour contrast included, which jsdom cannot do).
//
//   npm run build && npm run journey
//   SHOTS=/some/dir npm run journey      # also save a screenshot per step
//
// Fails on: a CSP violation, a console error, a page error, an axe violation,
// a request the stand-in did not expect, or any step not reaching its screen.
import { existsSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';

import axe from 'axe-core';
import { chromium } from 'playwright';

import { dist, listen, redirect, sendError, sendJson, shellCsp, standIn } from './standin.mjs';

const CSRF = 'a'.repeat(64);
const COOKIE = 'gs_journey=1';

const user = { id: 5, username: 'taro', display_name: 'Taro Yamada', is_admin: false };
const forgeWeb = {
  id: 7,
  namespace_id: 11,
  path: 'acme/forge-web',
  name: 'forge-web',
  display_name: 'Forge Web',
  description: 'The web UI',
  visibility: 'internal',
  kind: 'system',
  created_at: '2026-10-01T00:00:00Z',
  role: 'developer',
};
const group = {
  id: 3,
  namespace_id: 30,
  parent_id: null,
  path: 'acme',
  name: 'acme',
  display_name: 'Acme Plant',
  description: 'The plant group',
  created_at: '2026-09-01T00:00:00Z',
  role: 'owner',
};

function backend() {
  const projects = [forgeWeb];
  const unexpected = [];
  let nextId = 8;
  const handle = (req, res, url, body) => {
    const path = url.pathname;
    const signedIn = (req.headers.cookie ?? '').includes(COOKIE);
    const key = `${req.method} ${path}`;

    if (key === 'GET /-/auth/login') {
      return redirect(res, 302, `/-/auth/callback?next=${encodeURIComponent(url.searchParams.get('next') ?? '/')}`), true;
    }
    if (key === 'GET /-/auth/callback') {
      return redirect(res, 302, url.searchParams.get('next') ?? '/', { 'Set-Cookie': `${COOKIE}; Path=/; HttpOnly; SameSite=Lax` }), true;
    }
    if (key === 'POST /-/auth/logout') {
      const csrf = new URLSearchParams(body).get('csrf');
      if (!signedIn || csrf !== CSRF) return sendError(res, 403, 'Rejected.', 'req-csrf-logout'), true;
      return redirect(res, 303, '/', { 'Set-Cookie': `${COOKIE.split('=')[0]}=; Path=/; Max-Age=0` }), true;
    }
    if (path.startsWith('/-/auth/')) return false;

    if (!signedIn) return sendError(res, 401, 'Sign-in is required.', 'req-401', 'FG-AUTH-0001'), true;

    if (key === 'GET /api/v1/session') {
      return sendJson(res, 200, {
        absolute_expires_at: '2026-11-02T00:00:00Z',
        idle_expires_at: '2026-11-01T04:00:00Z',
        public_id: '0123456789abcdef',
        csrf_token: CSRF,
        user,
      }), true;
    }
    if (key === 'GET /api/v1/projects') {
      const kind = url.searchParams.get('kind');
      return sendJson(res, 200, { items: projects.filter((p) => kind === null || p.kind === kind), has_more: false, next_cursor: null }), true;
    }
    if (key === 'POST /api/v1/projects') {
      if (req.headers['x-gitstation-csrf'] !== CSRF) {
        return sendError(res, 403, 'The request was rejected.', 'req-csrf-post', 'FG-AUTH-0016'), true;
      }
      const input = JSON.parse(body);
      if (projects.some((p) => p.path === `acme/${input.path}`)) {
        return sendError(res, 409, 'That path is already in use.', 'req-dup', 'FG-REPO-0003'), true;
      }
      const created = {
        ...forgeWeb,
        id: nextId++,
        namespace_id: input.namespace_id,
        path: `${input.namespace_id === 30 ? 'acme' : 'taro'}/${input.path}`,
        name: input.path,
        display_name: input.display_name,
        description: input.description ?? '',
        visibility: input.visibility ?? 'private',
        kind: input.kind ?? 'system',
        role: 'owner',
      };
      projects.push(created);
      return sendJson(res, 201, created), true;
    }
    if (key === 'GET /api/v1/groups') {
      const { description, parent_id, ...listed } = group;
      void description; void parent_id;
      return sendJson(res, 200, { items: [{ ...listed, description: group.description }], has_more: false, next_cursor: null }), true;
    }
    if (key === 'GET /api/v1/groups/3') return sendJson(res, 200, group), true;
    if (key === 'GET /api/v1/namespaces/resolve') {
      const wanted = (url.searchParams.get('path') ?? '').toLowerCase();
      if (wanted === 'taro') {
        return sendJson(res, 200, { canonical_path: 'taro', kind: 'user', moved: false, namespace_id: 55, user_id: 5 }), true;
      }
      if (wanted === 'acme') {
        return sendJson(res, 200, { canonical_path: 'acme', kind: 'group', moved: false, namespace_id: 30, group_id: 3 }), true;
      }
      const hit = projects.find((p) => wanted === p.path || wanted.startsWith(`${p.path}/`));
      if (hit === undefined || (url.searchParams.get('prefix') !== 'true' && wanted !== hit.path)) {
        return sendError(res, 404, 'Not found.', 'req-404', 'FG-WEB-0001'), true;
      }
      const remainder = wanted.slice(hit.path.length + 1);
      return sendJson(res, 200, {
        canonical_path: hit.path,
        kind: 'project',
        moved: false,
        namespace_id: hit.namespace_id,
        project_id: hit.id,
        ...(remainder === '' ? {} : { remainder }),
      }), true;
    }
    const one = path.match(/^\/api\/v1\/projects\/(\d+)(\/repository\/refs)?$/);
    if (req.method === 'GET' && one) {
      const p = projects.find((x) => x.id === Number(one[1]));
      if (p === undefined) return sendError(res, 404, 'Not found.', 'req-404', 'FG-WEB-0001'), true;
      if (one[2] === undefined) return sendJson(res, 200, p), true;
      const filled = p.id === 7;
      return sendJson(res, 200, filled
        ? { default_branch: 'main', truncated: false, refs: [
            { name: 'main', full_name: 'refs/heads/main', type: 'branch', target: 'a'.repeat(40), commit: 'a'.repeat(40) },
            { name: 'v1', full_name: 'refs/tags/v1', type: 'tag', target: 'a'.repeat(40), commit: 'a'.repeat(40) },
          ] }
        : { default_branch: null, refs: [], truncated: false }), true;
    }
    unexpected.push(key);
    return sendError(res, 500, 'Unexpected test request.', 'req-unexpected'), true;
  };
  return { handle, unexpected, projects };
}

async function main() {
  if (!existsSync(join(dist, 'index.html'))) {
    console.error('journey: web/dist is missing (npm run build first)');
    process.exit(1);
  }
  const shots = process.env.SHOTS;
  if (shots) mkdirSync(shots, { recursive: true });
  const fake = backend();
  const server = standIn(shellCsp(), fake.handle);
  const base = await listen(server);
  const browser = await chromium.launch();
  const problems = [];
  const log = (m) => console.log(`journey: ${m}`);
  try {
    const context = await browser.newContext({ locale: 'en-US', viewport: { width: 1100, height: 800 } });
    await context.addInitScript({ content: axe.source });
    const page = await context.newPage();
    page.on('console', (msg) => {
      // The browser logs every 4xx as a console error; the journey causes a
      // 401 (signed out), a 404 (unknown path) and a 409 (duplicate) on purpose.
      if (/Failed to load resource: the server responded with a status of (401|404|409)/.test(msg.text())) return;
      if (msg.type() === 'error' || /Content Security Policy/i.test(msg.text())) {
        problems.push(`console ${msg.type()}: ${msg.text()}`);
      }
    });
    page.on('pageerror', (err) => problems.push(`page error: ${err.message}`));
    await page.exposeFunction('__cspViolation', (v) => problems.push(`CSP violation: ${v}`));
    await page.addInitScript(() => {
      document.addEventListener('securitypolicyviolation', (e) => {
        window.__cspViolation(`${e.violatedDirective} ${e.blockedURI}`);
      });
    });

    const check = async (name) => {
      const found = await page.evaluate(async () => {
        const result = await window.axe.run(document, {
          runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
        });
        return result.violations.map(
          (v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(' ')).join(', ')})`,
        );
      });
      for (const v of found) problems.push(`axe [${name}]: ${v}`);
      if (shots) await page.screenshot({ path: join(shots, `${name}.png`), fullPage: true });
      log(`${name}: axe ${found.length === 0 ? 'clean' : `${found.length} violation(s)`}`);
    };
    const expectPath = async (expected, step) => {
      await page.waitForURL((u) => u.pathname === expected, { timeout: 10_000 }).catch(() => {
        problems.push(`${step}: expected ${expected}, at ${new URL(page.url()).pathname}`);
      });
    };

    // 1. A deep link without a session: the sign-in screen, same address.
    await page.goto(`${base}/acme/forge-web`, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { name: 'Please sign in' }).waitFor({ timeout: 10_000 });
    await check('01-login');

    // 2. Sign in: leaves for the server route and comes back to the page asked for.
    await page.getByRole('button', { name: 'Sign in with the IoT portal' }).click();
    await page.getByRole('heading', { level: 1, name: 'Forge Web' }).waitFor({ timeout: 10_000 });
    await expectPath('/acme/forge-web', 'after sign-in');
    await check('02-project');
    const clone = await page.getByTestId('clone-url').innerText();
    if (!clone.endsWith('/acme/forge-web.git')) problems.push(`clone URL: ${clone}`);

    // 3. Home.
    await page.getByRole('link', { name: 'GitStation home' }).click();
    await page.getByRole('heading', { level: 1, name: 'Home' }).waitFor();
    await page.getByRole('link', { name: 'Forge Web' }).first().waitFor();
    await check('03-home');

    // 4. Create a project (CSRF header checked by the backend), land on it.
    await page.getByRole('button', { name: 'New project' }).first().click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Create in').selectOption({ label: 'acme' });
    await dialog.getByLabel('Name', { exact: true }).fill('Press Line');
    await check('04-create-dialog');
    await dialog.getByRole('button', { name: 'Create project' }).click();
    await page.getByRole('heading', { level: 1, name: 'Press Line' }).waitFor({ timeout: 10_000 });
    await expectPath('/acme/press-line', 'after create');
    await page.getByRole('heading', { name: 'The repository is empty' }).waitFor();
    await check('05-empty-repository');

    // 5. A duplicate is refused with its reason and request id, input kept.
    await page.getByRole('link', { name: 'GitStation home' }).click();
    await page.getByRole('button', { name: 'New project' }).first().click();
    await page.getByRole('dialog').getByLabel('Create in').selectOption({ label: 'acme' });
    await page.getByRole('dialog').getByLabel('Name', { exact: true }).fill('Press Line');
    await page.getByRole('dialog').getByRole('button', { name: 'Create project' }).click();
    const alert = page.getByRole('dialog').getByRole('alert');
    await alert.waitFor();
    const text = await alert.innerText();
    if (!text.includes('That path is already in use.') || !text.includes('req-dup') || text.includes('FG-REPO')) {
      problems.push(`duplicate refusal text: ${text}`);
    }
    await check('06-create-refused');
    await page.getByRole('dialog').getByRole('button', { name: 'Close' }).click();

    // 6. Group page, URL rescue, not found.
    await page.goto(`${base}/acme`, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { level: 1, name: 'Acme Plant' }).waitFor();
    await page.getByRole('link', { name: 'Press Line' }).waitFor();
    await check('07-group');
    await page.goto(`${base}/acme/forge-web/tree/main/src`, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { name: 'This screen is not available yet' }).waitFor();
    await expectPath('/acme/forge-web/-/tree/main/src', 'URL rescue');
    await page.goto(`${base}/nowhere/at-all`, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { name: 'Page not found' }).waitFor();
    await check('08-not-found');

    // 7. Sign out: a real form POST with the CSRF token; back to the sign-in screen.
    await page.goto(`${base}/`, { waitUntil: 'networkidle' });
    await page.getByRole('button', { name: /Taro Yamada/ }).click();
    await check('09-user-menu');
    await Promise.all([
      page.waitForURL((u) => u.pathname === '/'),
      page.getByRole('button', { name: 'Sign out' }).click(),
    ]);
    await page.getByRole('heading', { name: 'Please sign in' }).waitFor();
    log('signed out');
  } catch (e) {
    problems.push(`step failed: ${e instanceof Error ? e.message.split('\n')[0] : String(e)}`);
  } finally {
    await browser.close();
    server.close();
  }
  for (const u of fake.unexpected) problems.push(`unexpected request: ${u}`);
  if (problems.length > 0) {
    for (const p of problems) console.error(`journey: ${p}`);
    process.exit(1);
  }
  console.log('journey: ok (no CSP violations, console errors, page errors or axe violations)');
}

await main();
