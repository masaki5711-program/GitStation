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
// a request the stand-in did not expect, a GET route the UI's source calls that
// the stand-in does not answer (scripts/journey-routes.mjs), or any step not
// reaching its screen.
import { existsSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';

import axe from 'axe-core';
import { chromium } from 'playwright';

import { ACT_AS_HEAD, ACT_AS_STATE, actAsBackend } from './journey-act-as.mjs';
import { inboxBackend } from './journey-inbox.mjs';
import { mergeBackend } from './journey-merge.mjs';
import { mrBackend } from './journey-mr.mjs';
import { readsBackend } from './journey-reads.mjs';
import { PNG, REFS, browse } from './journey-repo.mjs';
import { probeGets } from './journey-routes.mjs';
import { SA_SECRET, serviceAccountsBackend } from './journey-service-accounts.mjs';
import { SECRET, settingsBackend } from './journey-settings.mjs';
import { HOOK_SECRET, ROTATED_SECRET, webhooksBackend } from './journey-webhooks.mjs';
import { dist, listen, redirect, sendError, sendJson, shellCsp, standIn } from './standin.mjs';

const CSRF = 'a'.repeat(64);
const COOKIE = 'gs_journey=1';

const user = { id: 5, username: 'taro', display_name: 'Taro Yamada', is_admin: true };
const forgeWeb = {
  id: 7,
  namespace_id: 11,
  path: 'acme/forge-web',
  name: 'forge-web',
  display_name: 'Forge Web',
  description: 'The web UI',
  visibility: 'internal',
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
  const settings = settingsBackend(CSRF);
  const webhooks = webhooksBackend(CSRF);
  const actAs = actAsBackend(CSRF, user);
  const serviceAccounts = serviceAccountsBackend(CSRF, user);
  const mergeRequests = mrBackend(CSRF, user);
  const merging = mergeBackend(CSRF, user);
  const inbox = inboxBackend(CSRF);
  const reads = readsBackend();
  let nextId = 9;
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

    // The raw route (server-served; headers as ADR-0015): the README's image.
    if (req.method === 'GET' && path === `/acme/forge-web/-/raw/${'a'.repeat(40)}/logo.png`) {
      res.writeHead(200, {
        'Content-Type': 'image/png',
        'Content-Security-Policy': "sandbox; default-src 'none'",
        'X-Content-Type-Options': 'nosniff',
        'Cross-Origin-Resource-Policy': 'same-origin',
      });
      res.end(PNG);
      return true;
    }

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
    // Merge requests first: the settings fake claims every /projects/7/ path.
    // !5 (approve and merge, U2) before the U1 fake, which answers 404 for it. The check runs,
    // conflicts, doc-lint migration and integration bindings reads (M3) before both.
    const mr =
      reads.answer(req, url) ??
      actAs.answer(req, url, body) ?? merging.answer(req, url, body) ?? mergeRequests.answer(req, url, body);
    if (mr !== null) {
      if (mr.error) return sendError(res, mr.status, ...mr.error), true;
      return sendJson(res, mr.status, mr.body), true;
    }
    // In-app notifications (U4), webhooks, notification preferences (U3a) and service accounts come before
    // the settings fake, which claims every /projects/7/, /user/ and /admin/ path.
    const more =
      inbox.answer(req, url, body) ??
      webhooks.answer(req, url, body) ??
      serviceAccounts.answer(req, url, body) ??
      settings.answer(req, url, body, forgeWeb);
    if (more !== null) {
      if (more.error) return sendError(res, more.status, ...more.error), true;
      if (more.status === 204) {
        res.writeHead(204, { 'Cache-Control': 'no-store' });
        res.end();
        return true;
      }
      return sendJson(res, more.status, more.body), true;
    }
    if (key === 'GET /api/v1/projects') {
      return sendJson(res, 200, { items: projects, has_more: false, next_cursor: null }), true;
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
    const repo = path.match(/^\/api\/v1\/projects\/7\/repository\/(.+)$/);
    if (req.method === 'GET' && repo) {
      const answer = browse(repo[1], url);
      if (answer !== null) {
        if (answer.error) return sendError(res, answer.status, ...answer.error), true;
        return sendJson(res, 200, answer.body), true;
      }
    }
    const one = path.match(/^\/api\/v1\/projects\/(\d+)(\/repository\/refs)?$/);
    if (req.method === 'GET' && one) {
      const p = projects.find((x) => x.id === Number(one[1]));
      if (p === undefined) return sendError(res, 404, 'Not found.', 'req-404', 'FG-WEB-0001'), true;
      if (one[2] === undefined) return sendJson(res, 200, p), true;
      const filled = p.id === 7;
      const refs = filled ? REFS : { default_branch: null, refs: [], truncated: false };
      return sendJson(res, 200, refs), true;
    }
    unexpected.push(key);
    return sendError(res, 500, 'Unexpected test request.', 'req-unexpected'), true;
  };
  return { handle, unexpected, projects, settings };
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
      // 401 (signed out), a 403 (a merge refused, U2), a 404 (unknown path), a 409 (duplicate, the
      // last-administrator guard, a stale view or stale merge settings) and a 429 (rename limit) on purpose.
      if (/Failed to load resource: the server responded with a status of (400|401|403|404|409|429)/.test(msg.text())) return;
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
    await page.getByRole('heading', { level: 1, name: 'src' }).waitFor();
    await expectPath('/acme/forge-web/-/tree/main/src', 'URL rescue');
    await page.goto(`${base}/nowhere/at-all`, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { name: 'Page not found' }).waitFor();
    await check('08-not-found');

    // 6a. The repository screens: project -> files -> folder -> file -> blame ->
    // commits -> commit diff -> compare -> branches, and the README file.
    const C1 = 'a'.repeat(40);
    const repoNav = (name) =>
      page.getByRole('navigation', { name: 'Repository screens' }).getByRole('link', { name, exact: true });
    await page.goto(`${base}/acme/forge-web`, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { level: 1, name: 'Forge Web' }).waitFor();
    await page.getByRole('link', { name: 'src/' }).waitFor();
    await page.getByTestId('markdown').waitFor();
    await check('12-project-files');
    await repoNav('Files').click();
    await page.getByRole('heading', { level: 1, name: 'Files' }).waitFor();
    await expectPath('/acme/forge-web/-/tree', 'files');
    await page.getByTestId('markdown').waitFor();
    // The README's raw HTML and javascript: link never become active; its own image loads.
    const pwned = await page.evaluate(() => window.__pwned);
    if (pwned !== undefined) problems.push('README script ran');
    const bad = await page.evaluate(
      () => document.querySelectorAll('main script, main iframe, main [onclick], main a[href^="javascript:" i]').length,
    );
    if (bad > 0) problems.push(`README produced ${bad} active element(s)`);
    const width = await page.getByAltText('logo').evaluate((img) => img.naturalWidth);
    if (width !== 1) problems.push(`README image did not load from the raw route (width ${width})`);
    await check('13-tree');
    await page.getByRole('link', { name: 'src/' }).click();
    await expectPath('/acme/forge-web/-/tree/main/src', 'folder');
    await page.getByRole('link', { name: 'lib.rs' }).click();
    await expectPath('/acme/forge-web/-/blob/main/src/lib.rs', 'file');
    await page.locator('[data-testid=code] .hljs-keyword').first().waitFor({ timeout: 10_000 });
    await check('14-file');
    await page.getByRole('link', { name: 'Blame', exact: true }).click();
    await expectPath('/acme/forge-web/-/blame/main/src/lib.rs', 'blame');
    await page.getByRole('table').waitFor();
    await check('15-blame');
    await repoNav('Commits').click();
    await expectPath('/acme/forge-web/-/commits', 'commits');
    await page.getByRole('link', { name: 'Add the add function' }).waitFor();
    await check('16-commits');
    await page.getByRole('link', { name: 'Add the add function' }).click();
    await expectPath(`/acme/forge-web/-/commit/${C1}`, 'commit');
    await page.getByTestId('diff-truncated').waitFor();
    await check('17-commit-diff');
    await page.goto(`${base}/acme/forge-web/-/compare/main...dev`, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { level: 2, name: /Commits only in the compared revision/ }).waitFor();
    await check('18-compare');
    await repoNav('Branches').click();
    await expectPath('/acme/forge-web/-/branches', 'branches');
    await page.getByRole('table').waitFor();
    await check('19-branches');
    await page.goto(`${base}/acme/forge-web/-/blob/main/README.md`, { waitUntil: 'networkidle' });
    await page.getByTestId('markdown').waitFor();
    await check('20-readme-file');
    await page.setViewportSize({ width: 375, height: 800 });
    for (const [path, name] of [
      ['/acme/forge-web/-/blob/main/src/lib.rs', '21-file-375'],
      [`/acme/forge-web/-/commit/${C1}`, '22-commit-375'],
      ['/acme/forge-web/-/tree', '23-tree-375'],
    ]) {
      await page.goto(`${base}${path}`, { waitUntil: 'networkidle' });
      await page.getByRole('heading', { level: 1 }).first().waitFor();
      const overflow = await page.evaluate(
        () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
      );
      if (overflow > 0) problems.push(`${name}: the page scrolls sideways by ${overflow}px at 375px`);
      await check(name);
    }
    await page.setViewportSize({ width: 1100, height: 800 });

    // 6b. A narrow phone in Indonesian (the longest strings): nothing overflows.
    await page.setViewportSize({ width: 375, height: 800 });
    await page.evaluate(() => window.localStorage.setItem('gitstation.lang', 'id'));
    for (const [path, name] of [['/', '10-home-375-id'], ['/acme/forge-web', '11-project-375-id']]) {
      await page.goto(`${base}${path}`, { waitUntil: 'networkidle' });
      await page.getByRole('heading', { level: 1 }).waitFor();
      const overflow = await page.evaluate(
        () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
      );
      if (overflow > 0) problems.push(`${name}: the page scrolls sideways by ${overflow}px at 375px`);
      await check(name);
    }
    await page.evaluate(() => window.localStorage.removeItem('gitstation.lang'));
    await page.setViewportSize({ width: 1100, height: 800 });


    // 6c. Settings, members, tokens, sessions, deleted projects, administration.
    const h1 = (name) => page.getByRole('heading', { level: 1, name }).waitFor({ timeout: 10_000 });
    await page.goto(`${base}/acme/forge-web/-/settings`, { waitUntil: 'networkidle' });
    await h1('Settings');
    await page.getByLabel('Description (optional)').fill('The web UI, edited');
    await page.getByRole('button', { name: 'Save' }).first().click();
    await page.getByText('Saved.').first().waitFor();
    await page.getByLabel('Pattern').fill('refs/heads/release/*');
    await page.getByRole('button', { name: 'Protect a ref' }).click();
    await page.getByRole('cell', { name: /refs\/heads\/release/ }).or(page.getByRole('rowheader', { name: /release/ })).first().waitFor();
    await check('24-settings');

    // 6c'. The merge sections (U2): methods saved with expected_updated_at; a
    // save after someone else's is refused 409 and read again; CODEOWNERS
    // suspended; an approval rule from the pickers; merge conditions whose
    // required check blocks every merge (the warning stays on screen).
    const merging = page.getByRole('region', { name: 'Merging' });
    const savedLine = merging.getByText('The merge settings were saved.');
    await merging.getByRole('checkbox', { name: 'Fast-forward' }).check();
    await merging.getByRole('button', { name: 'Save merge settings' }).click();
    await savedLine.waitFor();
    fake.settings.changeMergeSettingsElsewhere();
    await merging.getByRole('checkbox', { name: 'Delete the source branch after a merge' }).check();
    await merging.getByRole('button', { name: 'Save merge settings' }).click();
    const staleSettings = merging.getByRole('alert');
    await staleSettings.waitFor();
    const staleText = await staleSettings.innerText();
    if (!staleText.includes('changed by someone else') || !staleText.includes('req-ms-409') || staleText.includes('FG-')) {
      problems.push(`merge settings 409 text: ${staleText}`);
    }
    await check('24a-merge-settings-409');
    await merging.getByRole('button', { name: 'Reload the settings' }).click();
    await merging.getByRole('alert').waitFor({ state: 'detached' });
    await merging.getByRole('checkbox', { name: 'Delete the source branch after a merge' }).check();
    await merging.getByRole('button', { name: 'Save merge settings' }).click();
    await savedLine.waitFor();
    await merging.getByLabel('Suspend for (minutes)').fill('30');
    await merging.getByRole('button', { name: 'Suspend CODEOWNERS' }).click();
    await merging.getByText(/CODEOWNERS approval is suspended until/).waitFor();
    await page.getByRole('button', { name: 'Add an approval rule' }).click();
    const ruleDialog = page.getByRole('dialog');
    await ruleDialog.getByLabel('Name', { exact: true }).fill('leads');
    await ruleDialog.getByLabel('Find a user').fill('jir');
    await ruleDialog.getByRole('button', { name: 'Search' }).click();
    await ruleDialog.getByRole('button', { name: 'Add Jiro Suzuki' }).click();
    await ruleDialog.getByLabel('Group', { exact: true }).selectOption('acme');
    await ruleDialog.getByRole('button', { name: 'Add the group' }).click();
    await ruleDialog.getByRole('button', { name: 'Remove acme' }).waitFor();
    await check('24b-approval-rule-dialog');
    await ruleDialog.getByRole('button', { name: 'Save the rule' }).click();
    await page.getByRole('rowheader', { name: 'leads' }).waitFor();
    await page.getByRole('row', { name: /leads.*jiro, acme/ }).waitFor();
    await page.getByRole('button', { name: 'Change the conditions of refs/heads/main' }).click();
    const conditionsDialog = page.getByRole('dialog');
    await conditionsDialog.getByLabel('Required checks (one per line)').fill('ci/gates');
    await conditionsDialog.getByLabel('Forbid direct pushes (merges only)').check();
    await conditionsDialog.getByText(/cannot change at all/).waitFor();
    await check('24c-merge-conditions-dialog');
    await conditionsDialog.getByRole('button', { name: 'Save the conditions' }).click();
    const blockedNote = page.getByRole('note').filter({ hasText: 'every merge into this branch is blocked' });
    await blockedNote.waitFor();
    if (!(await blockedNote.innerText()).includes('cannot change at all')) {
      problems.push('merge conditions: the "branch cannot change" warning is missing');
    }
    // M3 I1d: the link condition is offered, but this save did not set it.
    const linkShown =
      (await page.getByTestId('merge-condition-work-item').count()) +
      (await page.getByTestId('merge-warning-work-item-unbound').count());
    if (linkShown > 0) {
      problems.push('merge conditions show a work item link that was not asked for');
    }
    await check('24d-merge-sections');
    await page.getByLabel('Last part of the path').fill('forge-web-2');
    await page.getByRole('button', { name: 'Move or rename' }).click();
    const refusal = page.getByRole('alert').filter({ hasText: 'renamed or moved too often' });
    await refusal.waitFor();
    const refusalText = await refusal.innerText();
    if (!refusalText.includes('req-move-429') || refusalText.includes('FG-REPO')) {
      problems.push(`move refusal text: ${refusalText}`);
    }
    await check('25-move-refused');
    await page.getByRole('button', { name: 'Delete this project' }).click();
    await page.getByRole('dialog').getByRole('heading', { name: 'Delete this project?' }).waitFor();
    await check('26-delete-confirm');
    await page.getByRole('dialog').getByRole('button', { name: 'Cancel' }).click();

    await page.goto(`${base}/acme/forge-web/-/members`, { waitUntil: 'networkidle' });
    await h1('Members');
    await page.getByLabel('Find a user').fill('han');
    await page.getByRole('button', { name: 'Search' }).click();
    await page.getByRole('button', { name: 'Add Hanako Sato' }).waitFor();
    await check('27-members');
    await page.getByRole('button', { name: 'Add Hanako Sato' }).click();
    await page.getByText('Hanako Sato was added.').waitFor();
    await page.locator('tr', { hasText: 'Jiro Suzuki' }).getByRole('button', { name: 'Remove' }).click();
    await page.getByRole('dialog').getByRole('heading', { name: 'Remove this member?' }).waitFor();
    await check('28-member-remove-confirm');
    await page.getByRole('dialog').getByRole('button', { name: 'Cancel' }).click();

    await page.goto(`${base}/taro/-/settings`, { waitUntil: 'networkidle' });
    await h1('Account settings');
    await page.getByLabel('Name', { exact: true }).fill('journey token');
    await page.getByRole('button', { name: 'Create token' }).click();
    const secret = page.getByTestId('token-secret');
    await secret.waitFor();
    if ((await page.getByTestId('token-value').innerText()) !== SECRET) problems.push('token secret not shown');
    await check('29-token-secret');
    await page.getByRole('button', { name: 'I have stored it' }).click();
    await page.getByRole('row', { name: /journey token/ }).waitFor();
    await check('30-tokens');
    await page.goto(`${base}/taro/-/settings`, { waitUntil: 'networkidle' });
    await h1('Account settings');
    await page.getByRole('row', { name: /journey token/ }).waitFor();
    if ((await page.locator('body').innerText()).includes('JOURNEYSYNTHETICSECRET')) {
      problems.push('the token secret is still on the page after a reload');
    }
    await page.getByRole('link', { name: 'My sessions' }).click();
    await page.getByText('This session').waitFor();
    await check('31-sessions');
    await page.getByRole('button', { name: /^End session started/ }).nth(1).click();
    await page.getByRole('dialog').getByRole('heading', { name: 'End this session?' }).waitFor();
    await check('32-session-confirm');
    await page.getByRole('dialog').getByRole('button', { name: 'Cancel' }).click();
    await page.getByRole('link', { name: 'Deleted projects' }).click();
    await page.getByText('No project is waiting for deletion.').waitFor();
    await check('33-deleted-projects');
    await page.getByRole('link', { name: 'Administration' }).first().click();
    await page.getByText('Hanako Sato').first().waitFor();
    await check('34-admin');
    await page.getByRole('button', { name: 'Disable Taro Yamada' }).click();
    await page.getByRole('dialog').getByRole('button', { name: 'Disable' }).click();
    const guard = page.getByRole('dialog').getByRole('alert');
    await guard.waitFor();
    const guardText = await guard.innerText();
    if (!guardText.includes('no administrator or group Owner') || !guardText.includes('req-disable-self') || /FG-AUTH|force/i.test(guardText)) {
      problems.push(`guard refusal text: ${guardText}`);
    }
    await check('35-admin-refused');
    await page.getByRole('dialog').getByRole('button', { name: 'Cancel' }).click();
    await page.setViewportSize({ width: 375, height: 800 });
    for (const [path, name, heading] of [
      ['/acme/forge-web/-/settings', '36-settings-375', 'Settings'],
      ['/acme/forge-web/-/members', '37-members-375', 'Members'],
      ['/taro/-/settings?tab=admin', '38-admin-375', 'Account settings'],
    ]) {
      await page.goto(`${base}${path}`, { waitUntil: 'networkidle' });
      await h1(heading);
      const overflow = await page.evaluate(
        () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
      );
      if (overflow > 0) problems.push(`${name}: the page scrolls sideways by ${overflow}px at 375px`);
      await check(name);
    }
    await page.setViewportSize({ width: 1100, height: 800 });

    // 6d. Merge requests (U1): list -> create -> overview (status line) ->
    // changes -> a line comment -> resolve it on the overview -> the status
    // line changes. The fake refuses any unsafe request without the CSRF header.
    const statusLine = page.getByTestId('mr-status');
    const statusIs = (text, step) =>
      page
        .waitForFunction(
          (want) => document.querySelector('[data-testid="mr-status"]')?.textContent === want,
          text,
          { timeout: 10_000 },
        )
        .catch(async () => {
          problems.push(`${step}: status line is "${await statusLine.textContent()}", expected "${text}"`);
        });
    await page.goto(`${base}/acme/forge-web/-/pulls`, { waitUntil: 'networkidle' });
    await h1('Merge requests');
    await page.getByRole('link', { name: 'Describe the press line' }).waitFor();
    await check('40-mr-list');
    await page.getByRole('link', { name: 'New merge request' }).first().click();
    await h1('New merge request');
    await expectPath('/acme/forge-web/-/pulls/new', 'mr create');
    await page.getByLabel('Branch with the changes').selectOption('dev');
    if ((await page.getByLabel('Branch to merge into').inputValue()) !== 'main') {
      problems.push('mr create: the target did not start at the default branch');
    }
    await page.getByLabel('Title').fill('Add the pump spec');
    await check('41-mr-create');
    await page.getByRole('button', { name: 'Create merge request' }).click();
    await expectPath('/acme/forge-web/-/pull/2', 'mr created');
    await page.getByRole('heading', { level: 1, name: /Add the pump spec/ }).waitFor();
    await statusIs('Ready to merge', 'mr overview');
    await check('42-mr-overview');
    await page.getByRole('navigation', { name: 'Merge request views' }).getByRole('link', { name: 'Changes' }).click();
    await expectPath('/acme/forge-web/-/pull/2/changes', 'mr changes');
    await page.getByRole('button', { name: 'Comment on line 3 of src/lib.rs' }).click();
    await page.getByLabel('Your comment: line 3 of src/lib.rs').fill('Why is the sum here?');
    await page.getByRole('button', { name: 'Comment', exact: true }).click();
    await page.getByText('Why is the sum here?').waitFor();
    await statusIs('Unresolved threads: 1', 'mr after the line comment');
    await check('43-mr-changes');
    await page.getByRole('navigation', { name: 'Merge request views' }).getByRole('link', { name: 'Overview' }).click();
    await expectPath('/acme/forge-web/-/pull/2', 'mr back to overview');
    await page.getByRole('button', { name: 'Open the first unresolved thread' }).click();
    const focused = await page.evaluate(() => document.activeElement?.getAttribute('data-testid') ?? '');
    if (!focused.startsWith('thread-')) problems.push(`mr next: the focus is on "${focused}", not a thread`);
    await page.getByRole('button', { name: 'Resolve thread' }).click();
    await statusIs('Ready to merge', 'mr after resolve');
    // The resolved thread is folded behind its count, which takes the focus.
    await page.getByText('Resolved threads: 1', { exact: true }).waitFor();
    const afterResolve = await page.evaluate(() => document.activeElement?.textContent ?? '');
    if (afterResolve !== 'Resolved threads: 1') problems.push(`mr resolve: the focus is on "${afterResolve}"`);
    // Cancel on a line comment gives the focus back to that line's button.
    await page.goto(`${base}/acme/forge-web/-/pull/2/changes`, { waitUntil: 'networkidle' });
    await page.getByRole('button', { name: 'Comment on line 2 of src/lib.rs' }).click();
    await page.getByRole('button', { name: 'Cancel' }).click();
    const back = await page.evaluate(() => document.activeElement?.getAttribute('aria-label') ?? '');
    if (back !== 'Comment on line 2 of src/lib.rs') problems.push(`mr cancel: the focus is on "${back}"`);
    await page.goto(`${base}/acme/forge-web/-/pull/2`, { waitUntil: 'networkidle' });
    await page.getByText('Resolved threads: 1', { exact: true }).waitFor();
    await check('44-mr-resolved');
    await page.setViewportSize({ width: 375, height: 800 });
    for (const [path, name] of [
      ['/acme/forge-web/-/pulls', '45-mr-list-375'],
      ['/acme/forge-web/-/pull/2', '46-mr-overview-375'],
      ['/acme/forge-web/-/pull/2/changes', '47-mr-changes-375'],
    ]) {
      await page.goto(`${base}${path}`, { waitUntil: 'networkidle' });
      await page.getByRole('heading', { level: 1 }).first().waitFor();
      const overflow = await page.evaluate(
        () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
      );
      if (overflow > 0) problems.push(`${name}: the page scrolls sideways by ${overflow}px at 375px`);
      await check(name);
    }
    await page.setViewportSize({ width: 1100, height: 800 });

    // 6e. Approve and merge (U2): !5 is a draft with three reasons. The first
    // approve finds a version pushed meanwhile (409; the reload reads again but
    // the buttons stay hidden behind "a new version was pushed" until its diff
    // was shown on Changes) -> approve the new head ->
    // one reason -> mark as ready -> merge: the first request is refused 403 in
    // the dialog -> merge again -> queued -> executing -> applied. The fake
    // refuses any unsafe request without the CSRF header.
    const conditions = page.getByRole('region', { name: 'Merge conditions' });
    const reasonItems = conditions.getByTestId('mr-conditions').locator('li');
    await page.goto(`${base}/acme/forge-web/-/pull/5`, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { level: 1, name: /Add the pump spec/ }).waitFor();
    await statusIs('It is still a draft', 'merge: draft');
    await reasonItems.nth(2).waitFor();
    if ((await reasonItems.count()) !== 3) problems.push(`merge: ${await reasonItems.count()} reasons, expected 3`);
    await check('48-merge-conditions');
    await conditions.getByRole('button', { name: 'Approve' }).click();
    const staleView = conditions.getByRole('alert');
    await staleView.waitFor();
    const staleViewText = await staleView.innerText();
    if (!staleViewText.includes('A new version was pushed') || !staleViewText.includes('req-stale-view') || staleViewText.includes('FG-')) {
      problems.push(`approve 409 text: ${staleViewText}`);
    }
    await check('49-approve-stale');
    await conditions.getByRole('button', { name: 'Reload the merge request' }).click();
    await conditions.getByRole('alert').waitFor({ state: 'detached' });
    await page.getByTestId('mr-new-version').filter({ hasText: 'A new version was pushed.' }).waitFor();
    if ((await conditions.getByRole('button', { name: 'Approve' }).count()) !== 0) {
      problems.push('merge: approve is offered for a version that was not shown');
    }
    await check('49a-new-version');
    await conditions.getByRole('button', { name: 'Review the new version' }).click();
    await expectPath('/acme/forge-web/-/pull/5/changes', 'merge: review the new version');
    await page.getByText('Version 1 (the latest)').waitFor();
    await page.getByRole('navigation', { name: 'Merge request views' }).getByRole('link', { name: 'Overview' }).click();
    await conditions.getByRole('button', { name: 'Approve' }).click();
    await conditions.getByText('You approved this version.').waitFor();
    await conditions.getByRole('button', { name: 'Withdraw my approval' }).waitFor();
    if ((await reasonItems.count()) !== 1) problems.push(`merge: ${await reasonItems.count()} reasons after approving, expected 1`);
    await page.getByRole('button', { name: 'Mark as ready' }).click();
    await statusIs('Ready to merge', 'merge: ready');
    await conditions.getByText('Every condition is met.').waitFor();
    await conditions.getByRole('button', { name: 'Merge…' }).click();
    const mergeDialog = page.getByRole('dialog');
    await mergeDialog.getByRole('radio', { name: 'Squash' }).check();
    await check('50-merge-dialog');
    await mergeDialog.getByRole('button', { name: 'Merge', exact: true }).click();
    const mergeRefused = mergeDialog.getByRole('alert');
    await mergeRefused.waitFor();
    const mergeRefusedText = await mergeRefused.innerText();
    if (!mergeRefusedText.includes('You may not merge into main right now.') || !mergeRefusedText.includes('req-merge-403') || mergeRefusedText.includes('FG-')) {
      problems.push(`merge 403 text: ${mergeRefusedText}`);
    }
    await check('51-merge-refused');
    await mergeDialog.getByRole('button', { name: 'Cancel' }).click();
    await mergeDialog.waitFor({ state: 'hidden' });
    await page.waitForFunction(() => document.activeElement?.textContent === 'Merge…', null, { timeout: 5000 }).catch(async () => {
      problems.push(`merge cancel: the focus is on "${await page.evaluate(() => document.activeElement?.textContent ?? '')}"`);
    });
    await conditions.getByRole('button', { name: 'Merge…' }).click();
    await page.getByRole('dialog').getByRole('button', { name: 'Merge', exact: true }).click();
    await page
      .waitForFunction(
        () => document.querySelector('[data-testid="mr-merge-progress"]')?.textContent?.includes('Merged into main.') === true,
        null,
        { timeout: 20_000 },
      )
      .catch(async () => {
        problems.push(`merge: progress is "${await page.getByTestId('mr-merge-progress').textContent()}"`);
      });
    const progressFocus = await page.evaluate(() => document.activeElement?.getAttribute('data-testid') ?? '');
    if (progressFocus !== 'mr-merge-progress') problems.push(`merge: the focus is on "${progressFocus}", not the progress`);
    await check('52-merged');
    await page.setViewportSize({ width: 375, height: 800 });
    await page.goto(`${base}/acme/forge-web/-/pull/5`, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { level: 1 }).first().waitFor();
    const mergedOverflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    if (mergedOverflow > 0) problems.push(`53-merged-375: the page scrolls sideways by ${mergedOverflow}px at 375px`);
    await check('53-merged-375');
    await page.setViewportSize({ width: 1100, height: 800 });

    // 6f. Webhooks and notifications (U3a). The URL is write-only (the page
    // shows the origin, never the path with the receiver's token); the secret
    // is announced, focused and shown once; a stopped webhook says why and what
    // to press; a redelivery still in progress is refused 429 with its reason.
    await page.goto(`${base}/acme/forge-web/-/webhooks`, { waitUntil: 'networkidle' });
    await h1('Webhooks');
    await page.getByText('No webhooks yet. Add one below.').waitFor();
    await check('60-webhooks-empty');
    await page.getByLabel('Payload URL').fill('https://evil.test/x');
    await page.getByRole('button', { name: 'Add webhook' }).click();
    const notAllowed = page.getByRole('alert').filter({ hasText: 'This destination is not allowed.' });
    await notAllowed.waitFor();
    if (!(await notAllowed.innerText()).includes('req-allow')) problems.push('webhook refusal: request id is missing');
    await check('61-webhook-refused');
    await page.getByLabel('Payload URL').fill('https://hooks.example.test:8443/t/receiver-token');
    // Event types are checkboxes: turn "All events" off and pick one event.
    await page.getByRole('checkbox', { name: /All events/ }).uncheck();
    await page.getByRole('checkbox', { name: 'merge_request.opened' }).check();
    await page.getByRole('button', { name: 'Add webhook' }).click();
    const secretPanel = page.getByTestId('webhook-secret');
    await secretPanel.waitFor();
    if ((await page.getByTestId('webhook-secret-value').innerText()) !== HOOK_SECRET) {
      problems.push('webhook secret: not shown after create');
    }
    if ((await page.evaluate(() => document.activeElement?.getAttribute('data-testid'))) !== 'webhook-secret') {
      problems.push('webhook secret: the panel did not take the focus');
    }
    await page.getByText('The secret is shown below, once.').first().waitFor({ state: 'attached' });
    await check('62-webhook-secret');
    await page.getByRole('button', { name: 'I have saved it' }).click();
    await secretPanel.waitFor({ state: 'detached' });
    const hooksText = await page.locator('main').innerText();
    if (hooksText.includes(HOOK_SECRET) || hooksText.includes('receiver-token')) {
      problems.push('webhook list shows the secret or the write-only URL path');
    }
    await page.getByText('https://hooks.example.test:8443', { exact: true }).waitFor();
    await check('63-webhooks-list');
    await page.getByRole('button', { name: 'Disable https://hooks.example.test:8443' }).click();
    await page.getByText('Turned off by a person. Press Enable to start again.').waitFor();
    await check('64-webhook-stopped');
    await page.getByRole('button', { name: 'Enable https://hooks.example.test:8443' }).click();
    await page.getByText('Enabled again. Its failure count was reset.').waitFor();
    await page.getByRole('button', { name: 'Edit https://hooks.example.test:8443' }).click();
    await page.getByRole('dialog').getByRole('checkbox', { name: /All events/ }).check();
    await check('65-webhook-edit');
    await page.getByRole('dialog').getByRole('button', { name: 'Save', exact: true }).click();
    await page.getByText('Saved.').first().waitFor();
    await page.getByRole('button', { name: 'Rotate secret of https://hooks.example.test:8443' }).click();
    await page.getByRole('dialog').getByRole('heading', { name: 'Rotate the secret?' }).waitFor();
    await check('66-webhook-rotate-confirm');
    await page.getByRole('dialog').getByRole('button', { name: 'Rotate secret', exact: true }).click();
    await secretPanel.waitFor();
    if ((await page.getByTestId('webhook-secret-value').innerText()) !== ROTATED_SECRET) {
      problems.push('webhook secret: the rotated secret is not shown');
    }
    // The panel waits for the closing dialog before it takes the focus.
    await page
      .waitForFunction(() => document.activeElement?.getAttribute('data-testid') === 'webhook-secret', null, { timeout: 3000 })
      .catch(() => problems.push('webhook secret: the panel did not take the focus after a rotation'));
    await page.getByRole('button', { name: 'I have saved it' }).click();

    await page.getByRole('link', { name: 'Deliveries of https://hooks.example.test:8443' }).click();
    await h1('Delivery history');
    await page.getByRole('row', { name: /merge_request\.opened/ }).first().waitFor();
    await check('67-deliveries');
    await page.getByRole('button', { name: 'Redeliver merge_request.opened (#25)' }).click();
    const inProgress = page.getByRole('alert').filter({ hasText: 'still in progress' });
    await inProgress.waitFor();
    if (!(await inProgress.innerText()).includes('req-redeliver-429')) problems.push('redeliver 429: request id is missing');
    await check('68-redeliver-refused');
    await page.getByRole('button', { name: 'Redeliver merge_request.opened (#25)' }).click();
    await page.getByText('Queued for delivery again.').waitFor();
    await page.getByRole('button', { name: 'Show older deliveries' }).click();
    await page.getByRole('button', { name: 'Redeliver merge_request.opened (#1)' }).waitFor();
    await check('69-deliveries-older');
    await page.getByRole('link', { name: 'Back to webhooks' }).click();
    await h1('Webhooks');
    await page.getByRole('button', { name: 'Delete https://hooks.example.test:8443' }).click();
    await page.getByRole('dialog').getByRole('heading', { name: 'Delete this webhook?' }).waitFor();
    await check('70-webhook-delete-confirm');
    await page.getByRole('dialog').getByRole('button', { name: 'Delete webhook' }).click();
    await page.getByText('The webhook was deleted.').waitFor();

    await page.goto(`${base}/taro/-/settings?tab=notifications`, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { level: 2, name: 'Notifications' }).waitFor();
    await check('71-notifications');
    await page.getByLabel('Send me notification mail').uncheck();
    await page.getByLabel('Language of the mail').selectOption('ja');
    await page.getByRole('button', { name: 'Save notification settings' }).click();
    await page.getByText('Notification settings were saved.').waitFor();
    await check('72-notifications-saved');

    // 6f'. In-app notifications (U4): the bell names the unread count and
    // leads to the list; one item is marked read (the count follows), then
    // the rest with "Mark all as read"; the unread filter is then empty.
    // 375 px: no sideways scroll.
    await page.getByRole('link', { name: 'Notifications (2 unread)' }).click();
    await h1('Notifications');
    await expectPath('/-/notifications', 'the bell');
    await page.getByText('Hanako Suzuki commented on your merge request').waitFor();
    await check('74-inbox');
    await page.getByRole('button', { name: 'Mark as read (Show the clone URL)' }).first().click();
    await page.getByText('Marked as read.').waitFor();
    await page.getByRole('link', { name: 'Notifications (1 unread)' }).waitFor();
    await page.getByRole('button', { name: 'Mark all as read' }).click();
    await page.getByText('All notifications were marked as read.').waitFor();
    await page.getByRole('link', { name: 'Notifications', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Unread only' }).click();
    await page.getByText('You have no unread notifications.').waitFor();
    await check('75-inbox-unread-empty');
    await page.setViewportSize({ width: 375, height: 800 });
    await page.goto(`${base}/-/notifications`, { waitUntil: 'networkidle' });
    await h1('Notifications');
    {
      const overflow = await page.evaluate(
        () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
      );
      if (overflow > 0) problems.push(`76-inbox-375: the page scrolls sideways by ${overflow}px at 375px`);
    }
    await check('76-inbox-375');
    await page.setViewportSize({ width: 1100, height: 800 });

    // 6g. The act-as confirmation popup (U3): no navigation, the button is
    // dead for the first second and after that until a real press on it arms it (a click right after is refused),
    // one press, then "Done". The same state cannot be used twice.
    const popup = `${base}/-/confirm/projects/7/merge-requests/3?intent=approve&sha=${ACT_AS_HEAD}&integration=4&state=${ACT_AS_STATE}`;
    await page.goto(popup, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { level: 1, name: 'Confirm on your behalf' }).waitFor();
    const approveBtn = page.getByRole('button', { name: 'Approve', exact: true });
    await approveBtn.waitFor();
    if ((await approveBtn.getAttribute('aria-disabled')) !== 'true') problems.push('act-as: the button works at once');
    if ((await page.getByRole('navigation').count()) !== 0) problems.push('act-as: the popup shows navigation');
    await check('73-act-as-confirm');
    await page.waitForTimeout(1200);
    if ((await approveBtn.getAttribute('aria-disabled')) !== 'true') problems.push('act-as: armed without a new pointer action');
    await page.mouse.move(40, 40);
    if ((await approveBtn.getAttribute('aria-disabled')) !== 'true') problems.push('act-as: armed by a pointer move');
    // The first click arms it and is refused; the second, later, counts.
    await approveBtn.click({ force: true });
    await page.getByText(/Press once more/).waitFor();
    await page.waitForTimeout(400);
    await approveBtn.click({ force: true });
    await page.getByRole('heading', { name: 'Done' }).waitFor();
    await check('74-act-as-done');
    await page.goto(popup, { waitUntil: 'networkidle' });
    await approveBtn.waitFor();
    await page.waitForTimeout(1200);
    await approveBtn.click({ force: true });
    await page.waitForTimeout(400);
    await approveBtn.click({ force: true });
    const used = page.getByRole('alert').filter({ hasText: 'already used' });
    await used.waitFor();
    if (!(await used.innerText()).includes('req-act-as-used')) problems.push('act-as: used-link request id is missing');
    await check('75-act-as-used');

    // 6g. Service accounts (admin). A taken username is refused with its reason
    // and request id; the secret of a new token is announced, focused and shown
    // once with a copy button; a refused issue shows no secret; revoke and
    // disable ask first; a disabled account says why it gets no token.
    await page.goto(`${base}/taro/-/settings?tab=service-accounts`, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { level: 2, name: 'Service accounts' }).waitFor();
    await page.getByRole('rowheader', { name: /CI runner/ }).waitFor();
    await check('80-service-accounts');
    await page.getByLabel('Username').fill('sa-ci-runner');
    await page.getByLabel('Description').fill('Duplicate on purpose');
    await page.getByRole('button', { name: 'Create service account' }).click();
    const taken = page.getByRole('alert').filter({ hasText: 'That username is already in use.' });
    await taken.waitFor();
    if (!(await taken.innerText()).includes('req-sa-dup')) problems.push('service account refusal: request id is missing');
    await check('81-service-account-refused');
    await page.getByLabel('Username').fill('sa-kanflow');
    await page.getByLabel('Display name').fill('Kanflow');
    await page.getByLabel('Description').fill('PM integration');
    await page.getByRole('button', { name: 'Create service account' }).click();
    await page.getByText('The service account Kanflow was created.').waitFor({ state: 'attached' });
    await page.getByRole('rowheader', { name: /Kanflow/ }).waitFor();
    await check('82-service-account-created');
    await page.getByRole('link', { name: 'Manage tokens of Kanflow' }).click();
    await page.getByRole('heading', { level: 2, name: 'Kanflow' }).waitFor();
    await page.getByText('This account has no tokens yet.').waitFor();
    await check('83-service-account-detail');
    await page.getByLabel('Name', { exact: true }).fill('refuse-me');
    await page.getByRole('button', { name: 'Issue token' }).click();
    const sessionNeeded = page.getByRole('alert').filter({ hasText: 'A browser session is needed for this.' });
    await sessionNeeded.waitFor();
    if (!(await sessionNeeded.innerText()).includes('req-sa-session')) problems.push('token refusal: request id is missing');
    await check('84-token-refused');
    await page.getByLabel('Name', { exact: true }).fill('sync');
    await page.getByLabel('merge_execute').check();
    await page.getByRole('button', { name: 'Issue token' }).click();
    const saPanel = page.getByTestId('token-secret');
    await saPanel.waitFor();
    if ((await saPanel.getByTestId('token-value').innerText()) !== SA_SECRET) problems.push('service account token: the secret is not shown');
    if (!(await saPanel.innerText()).includes('only time the token is shown')) problems.push('service account token: the one-time warning is missing');
    await saPanel.getByRole('button', { name: 'Copy the token' }).waitFor();
    if (!(await saPanel.evaluate((el) => el === document.activeElement))) problems.push('service account token: the secret panel does not have the focus');
    await check('85-service-account-secret');
    await page.getByRole('button', { name: 'I have stored it' }).click();
    await page.getByRole('rowheader', { name: 'sync' }).waitFor();
    if ((await page.content()).includes(SA_SECRET)) problems.push('service account token: the secret is still on the page after "I have stored it"');
    await page.getByRole('button', { name: 'Revoke sync' }).click();
    await page.getByRole('dialog').getByRole('heading', { name: 'Revoke this token?' }).waitFor();
    await check('86-token-revoke-confirm');
    await page.getByRole('dialog').getByRole('button', { name: 'Revoke' }).click();
    await page.getByText('Revoked', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Disable Kanflow' }).click();
    await page.getByRole('dialog').getByRole('heading', { name: 'Disable this service account?' }).waitFor();
    await page.getByRole('dialog').getByRole('button', { name: 'Disable' }).click();
    await page.getByRole('note').filter({ hasText: 'is disabled, so no token can be issued' }).waitFor();
    await check('87-service-account-disabled');
    await page.setViewportSize({ width: 375, height: 800 });
    for (const [path, name] of [
      ['/taro/-/settings?tab=service-accounts', '88-service-accounts-375'],
      ['/taro/-/settings?tab=service-accounts&account=4', '89-service-account-375'],
    ]) {
      await page.goto(`${base}${path}`, { waitUntil: 'networkidle' });
      await h1('Account settings');
      const overflow = await page.evaluate(
        () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
      );
      if (overflow > 0) problems.push(`${name}: the page scrolls sideways by ${overflow}px at 375px`);
      await check(name);
    }
    await page.setViewportSize({ width: 1100, height: 800 });

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
    problems.push(`step failed: ${e instanceof Error ? e.message.split('\n').slice(0, 3).join(' | ') : String(e)}`);
  } finally {
    await browser.close();
    server.close();
  }
  for (const u of fake.unexpected) problems.push(`unexpected request (no fake answers it; add one in scripts/journey-*.mjs): ${u}`);
  // Every read the UI's source makes must have a fake, visited by a step or not.
  for (const r of probeGets(fake.handle, fake.unexpected, COOKIE)) {
    problems.push(`route the UI calls has no fake (add one in scripts/journey-*.mjs): ${r}`);
  }
  if (problems.length > 0) {
    for (const p of problems) console.error(`journey: ${p}`);
    process.exit(1);
  }
  console.log('journey: ok (no CSP violations, console errors, page errors or axe violations)');
}

await main();
