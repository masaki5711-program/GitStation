// synthetic — shared helpers for the merge load test (ops/loadtest/README.md).
// Node 22, built-in modules only. Never prints the token.
import { readFileSync, writeFileSync, mkdirSync, appendFileSync } from 'node:fs';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { performance } from 'node:perf_hooks';

const pexec = promisify(execFile);

export const BASE = process.env.GS_LT_BASE ?? 'http://127.0.0.1:28080';
export const GIT_BASE = process.env.GS_LT_GIT_BASE ?? BASE;
export const USER = process.env.GS_LT_USER ?? 'dora';
const TOKEN_FILE = process.env.GS_LT_TOKEN_FILE;
if (!TOKEN_FILE) throw new Error('GS_LT_TOKEN_FILE (a file holding one fgp_ token) is required');
const TOKEN = readFileSync(TOKEN_FILE, 'utf8').trim();
if (!/^fgp_[A-Za-z0-9_]+$/.test(TOKEN)) throw new Error('GS_LT_TOKEN_FILE does not hold one fgp_ token');
export const OUT = process.env.GS_LT_OUT ?? 'ops/loadtest/.out';
mkdirSync(OUT, { recursive: true });

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const now = () => Date.now();

/** One API call; returns {status, body, ms, requestId}. Never throws on HTTP errors. */
export async function api(method, path, body, { timeoutMs = 60000 } = {}) {
  const t0 = performance.now();
  let res;
  try {
    res = await fetch(BASE + path, {
      method,
      headers: { Authorization: `Bearer ${TOKEN}`, 'Content-Type': 'application/json', Accept: 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(timeoutMs),
    });
  } catch (e) {
    return { status: 0, body: { error: String(e?.name ?? e) }, ms: performance.now() - t0, requestId: null };
  }
  const text = await res.text();
  let parsed = text;
  try { parsed = JSON.parse(text); } catch { /* not json */ }
  return { status: res.status, body: parsed, ms: performance.now() - t0, requestId: res.headers.get('x-request-id') ?? parsed?.request_id ?? null };
}

export async function apiOk(method, path, body, opts) {
  const r = await api(method, path, body, opts);
  if (r.status < 200 || r.status > 299) {
    throw new Error(`${method} ${path} -> ${r.status} ${JSON.stringify(r.body).slice(0, 400)}`);
  }
  return r.body;
}

const basic = Buffer.from(`${USER}:${TOKEN}`).toString('base64');
/** git with the token as an extra header (never in argv URLs or .git/config). */
export async function git(cwd, args, { timeoutMs = 300000, env = {} } = {}) {
  const t0 = performance.now();
  try {
    const r = await pexec('git', ['-c', `http.extraHeader=Authorization: Basic ${basic}`, '-c', 'core.autocrlf=false', '-c', 'user.name=loadtest', '-c', 'user.email=loadtest@example.invalid', ...args], {
      cwd, timeout: timeoutMs, maxBuffer: 64 << 20,
      env: { ...process.env, GIT_TERMINAL_PROMPT: '0', ...env },
    });
    return { ok: true, ms: performance.now() - t0, stdout: r.stdout, stderr: r.stderr };
  } catch (e) {
    return { ok: false, ms: performance.now() - t0, stdout: e.stdout ?? '', stderr: String(e.stderr ?? e.message).replaceAll(basic, '***') };
  }
}
export async function gitOk(cwd, args, opts) {
  const r = await git(cwd, args, opts);
  if (!r.ok) throw new Error(`git ${args.slice(0, 3).join(' ')} failed: ${r.stderr.slice(0, 600)}`);
  return r;
}

export function pct(arr, p) {
  if (!arr.length) return null;
  const s = [...arr].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.ceil((p / 100) * s.length) - 1)];
}
export function stats(arr) {
  return { n: arr.length, p50: pct(arr, 50), p95: pct(arr, 95), max: arr.length ? Math.max(...arr) : null };
}
export function log(file, obj) {
  appendFileSync(`${OUT}/${file}`, JSON.stringify({ t: new Date().toISOString(), ...obj }) + '\n');
}
export function save(file, obj) { writeFileSync(`${OUT}/${file}`, JSON.stringify(obj, null, 2)); }

/** Resolve a path to {kind, namespace_id, project_id} or null. */
export async function resolve(path) {
  const r = await api('GET', `/api/v1/namespaces/resolve?path=${encodeURIComponent(path)}`);
  return r.status === 200 ? r.body : null;
}
/** Group by path (created when missing): {namespace_id}. */
export async function ensureGroup(path) {
  const got = await resolve(path);
  if (got) return got;
  const g = await apiOk('POST', '/api/v1/groups', { path, display_name: `Load test (${path})`, description: 'synthetic load test' });
  return { namespace_id: g.namespace_id, group_id: g.id };
}
/** Project id (created when missing). */
export async function ensureProject(groupPath, path) {
  const got = await resolve(`${groupPath}/${path}`);
  if (got && got.project_id) return { id: got.project_id, created: false };
  const g = await ensureGroup(groupPath);
  const p = await apiOk('POST', '/api/v1/projects', { namespace_id: g.namespace_id, path, display_name: path, description: 'synthetic load test' });
  return { id: p.id, created: true };
}
export const repoUrl = (groupPath, project) => `${GIT_BASE}/${groupPath}/${project}.git`;

/** Poll until fn() returns truthy, or timeout. */
export async function until(fn, { timeoutMs, everyMs = 2000 }) {
  const end = now() + timeoutMs;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (now() > end) return null;
    await sleep(everyMs);
  }
}
