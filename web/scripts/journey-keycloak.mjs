// The real-Keycloak browser journey of M3 P5 (plan 11 §7, the M2 carry-over):
// sign in through Keycloak -> sign out -> sign in again -> the user is
// disabled in Keycloak -> the very next request is refused.
//
//   node ops/test-env/test-env.mjs up        # the test environment (runbook m3-test-environment.md)
//   GS_M3_TESTENV=ops/test-env/.local/journey.env.json npm run journey:keycloak
//
// GS_M3_TESTENV points at the JSON file `test-env.mjs up` writes (URLs and the
// synthetic Keycloak admin login). Without it the journey is SKIPPED (exit 0),
// so a CI without the environment stays green; `npm run journey` runs this too.
//
// "Takes effect on the next request": GitStation re-checks the identity
// provider at most every 5 minutes per session (ADR-0021 §7). The journey does
// not wait: it ages the session's `refresh_checked_at` in the test database
// (the test-only step; the container is this environment's own), then makes the
// next request. As a control, the same request with an aged session but an
// enabled user must still succeed, so the refusal is caused by the disable.
//
// Only the synthetic user `dora` is touched, and it is re-enabled at the end.
import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';

import { chromium } from 'playwright';

const USER = 'dora';
const PASSWORD = 'synthetic-password';

const envFile = process.env.GS_M3_TESTENV;
if (!envFile) {
  console.log('journey-keycloak: skipped (GS_M3_TESTENV is not set; start ops/test-env and point it at .local/journey.env.json)');
  process.exit(0);
}
if (!existsSync(envFile)) {
  console.error(`journey-keycloak: GS_M3_TESTENV points at a missing file: ${envFile}`);
  process.exit(1);
}
const env = JSON.parse(readFileSync(envFile, 'utf8'));
if (env.composeProject !== 'gs-m3-test') {
  console.error(`journey-keycloak: refusing to run: composeProject is ${JSON.stringify(env.composeProject)}, expected "gs-m3-test"`);
  process.exit(1);
}
const gsOrigin = new URL(env.gitstation).origin;

async function adminToken() {
  const res = await fetch(`${env.keycloak}/realms/master/protocol/openid-connect/token`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams({ grant_type: 'password', client_id: 'admin-cli', username: env.adminUser, password: env.adminPassword }),
    signal: AbortSignal.timeout(10_000),
  });
  if (!res.ok) throw new Error(`Keycloak admin login failed: ${res.status}`);
  return (await res.json()).access_token;
}

async function setEnabled(enabled) {
  const token = await adminToken();
  const auth = { Authorization: `Bearer ${token}` };
  const found = await fetch(`${env.keycloak}/admin/realms/${env.realm}/users?username=${USER}&exact=true`, { headers: auth, signal: AbortSignal.timeout(10_000) });
  const [user] = await found.json();
  if (!user) throw new Error(`synthetic user ${USER} is not in realm ${env.realm}`);
  const put = await fetch(`${env.keycloak}/admin/realms/${env.realm}/users/${user.id}`, {
    method: 'PUT',
    headers: { ...auth, 'Content-Type': 'application/json' },
    body: JSON.stringify({ ...user, enabled }),
    signal: AbortSignal.timeout(10_000),
  });
  if (put.status !== 204) throw new Error(`Keycloak user update failed: ${put.status}`);
  return user.id;
}

/** Make the live sessions of ONE Keycloak subject look last checked 10 minutes ago, so the next request refreshes. */
function ageSessions(sub) {
  if (!/^[0-9a-f-]{36}$/.test(sub)) throw new Error('unexpected Keycloak subject id');
  const r = spawnSync(
    'docker',
    ['compose', '-p', env.composeProject, '-f', env.composeFile, '--env-file', env.composeEnv, 'exec', '-T', 'gs-test-pg',
      'psql', '-U', 'postgres', '-d', 'forge', '-v', 'ON_ERROR_STOP=1', '-tAc',
      `UPDATE forge.web_sessions SET refresh_checked_at = now() - interval '10 minutes' WHERE revoked_at IS NULL AND oidc_sub = '${sub}' RETURNING 1`],
    { encoding: 'utf8' },
  );
  if (r.status !== 0) throw new Error(`could not age the sessions: ${r.stderr}`);
  return r.stdout.trim().split('\n').filter(Boolean).length;
}

const problems = [];
const log = (m) => console.log(`journey-keycloak: ${m}`);
const must = (cond, msg) => {
  if (!cond) problems.push(msg);
  return cond;
};

let shot;
async function main() {
  const sub = await setEnabled(true); // a rerun after a failed run starts from a known state
  const browser = await chromium.launch();
  try {
    const context = await browser.newContext({ locale: 'en-US', viewport: { width: 1100, height: 800 } });
    const page = await context.newPage();
    page.on('pageerror', (err) => problems.push(`page error: ${err.message}`));
    await page.exposeFunction('__cspViolation', (v) => problems.push(`CSP violation: ${v}`));
    await page.addInitScript((origin) => {
      if (location.origin !== origin) return; // the Keycloak pages are not ours
      document.addEventListener('securitypolicyviolation', (e) => window.__cspViolation(`${e.violatedDirective} ${e.blockedURI}`));
    }, gsOrigin);
    page.on('console', (msg) => {
      if (msg.type() !== 'error') return;
      if (new URL(page.url()).origin !== gsOrigin) return;
      if (/Failed to load resource: the server responded with a status of 401/.test(msg.text())) return; // signed out on purpose
      problems.push(`console error: ${msg.text()}`);
    });

    shot = (name) => page.screenshot({ path: `${process.env.SHOTS}/keycloak-${name}.png`, fullPage: true });
    const sessionStatus = () => page.evaluate(async () => (await fetch('/api/v1/session', { credentials: 'same-origin' })).status);

    const signIn = async (step) => {
      await page.goto(env.gitstation, { waitUntil: 'networkidle' });
      await page.getByRole('heading', { name: 'Please sign in' }).waitFor({ timeout: 15_000 });
      await page.getByRole('button', { name: 'Sign in with the IoT portal' }).click();
      await page.waitForURL((u) => u.origin === new URL(env.keycloak).origin, { timeout: 15_000 });
      await page.locator('#username').fill(USER);
      await page.locator('#password').fill(PASSWORD);
      await page.locator('#kc-login').click();
      await page.waitForURL((u) => u.origin === gsOrigin, { timeout: 20_000 });
      await page.getByRole('button', { name: /Dora Disposable/ }).waitFor({ timeout: 15_000 });
      must((await sessionStatus()) === 200, `${step}: no session after sign-in`);
      log(`${step}: signed in through Keycloak`);
    };

    // 1. Sign in through the real Keycloak (authorization code + PKCE).
    await signIn('1 sign-in');

    // 2. Sign out: the form post ends the GitStation session and Keycloak's.
    await page.getByRole('button', { name: /Dora Disposable/ }).click();
    await page.getByRole('button', { name: 'Sign out' }).click();
    await page.getByRole('heading', { name: 'Please sign in' }).waitFor({ timeout: 20_000 });
    must(new URL(page.url()).origin === gsOrigin, '2 sign-out: did not return to GitStation');
    must((await sessionStatus()) === 401, '2 sign-out: the session still answers after sign-out');
    log('2 sign-out: back at the sign-in screen, session refused');

    // 3. Sign in again (a new Keycloak session; the form is asked again).
    await signIn('3 sign-in again');

    // 4. Control: an aged session of an ENABLED user keeps working (the refresh succeeds).
    must(ageSessions(sub) >= 1, '4 control: no live session to age');
    must((await sessionStatus()) === 200, '4 control: an enabled user lost the session on refresh');
    log('4 control: aged session of an enabled user still works');

    // 5. Disable the user in Keycloak; the next request (after the check is due) is refused.
    await setEnabled(false);
    log('5 Keycloak: user disabled');
    ageSessions(sub);
    const after = await sessionStatus();
    must(after === 401, `5 disable: the next request answered ${after}, expected 401`);
    log(`5 disable: next request answered ${after}`);

    // 6. The disabled user cannot sign in again.
    await page.goto(env.gitstation, { waitUntil: 'networkidle' });
    await page.getByRole('heading', { name: 'Please sign in' }).waitFor({ timeout: 15_000 });
    await page.getByRole('button', { name: 'Sign in with the IoT portal' }).click();
    await page.waitForURL((u) => u.origin === new URL(env.keycloak).origin, { timeout: 15_000 });
    await page.locator('#username').fill(USER);
    await page.locator('#password').fill(PASSWORD);
    await page.locator('#kc-login').click();
    await page.locator('#input-error, .kc-feedback-text, .alert-error').first().waitFor({ timeout: 15_000 }).catch(() => {
      problems.push('6 disabled sign-in: Keycloak showed no error');
    });
    must(new URL(page.url()).origin !== gsOrigin, '6 disabled sign-in: reached GitStation');
    log('6 disabled sign-in: refused by Keycloak');
  } catch (e) {
    if (process.env.SHOTS) await shot?.(`failed`).catch(() => {});
    problems.push(`step failed: ${e instanceof Error ? e.message.split('\n')[0] : String(e)}`);
  } finally {
    await browser.close();
    await setEnabled(true).catch((e) => problems.push(`cleanup (re-enable ${USER}) failed: ${e.message}`));
  }
  if (problems.length > 0) {
    for (const p of problems) console.error(`journey-keycloak: ${p}`);
    process.exit(1);
  }
  console.log('journey-keycloak: ok (keycloak_disable_takes_effect_on_next_request)');
}

await main();
