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
// GS_LT_CONCURRENCY: API calls this client keeps open at once in a burst.
// The client is one socket peer, and forge refuses a peer's connections above
// `server.max_connections_per_peer` (default 64, FG-WEB-0008) before reading
// them; 200 people at 200 PCs are 200 peers. 32 leaves the poller and git
// their share. A burst still arrives at once from the queue's point of view.
export const CONCURRENCY = (() => {
  const raw = process.env.GS_LT_CONCURRENCY ?? '32';
  if (!/^[0-9]+$/.test(raw) || Number(raw) < 1 || Number(raw) > 1024) {
    throw new Error(`GS_LT_CONCURRENCY must be an integer from 1 to 1024 (got '${raw}')`);
  }
  return Number(raw);
})();
/** `Promise.all(items.map(fn))` with at most `limit` calls running; results keep the order of `items`. */
export async function mapLimit(items, limit, fn) {
  const out = new Array(items.length);
  let next = 0;
  const worker = async () => {
    for (;;) {
      const i = next++;
      if (i >= items.length) return;
      out[i] = await fn(items[i], i);
    }
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  return out;
}
let secret = null;
/** The token and its Basic form, read on first use (`spawn-bench` runs without one). */
function credentials() {
  if (secret) return secret;
  const file = process.env.GS_LT_TOKEN_FILE;
  if (!file) throw new Error('GS_LT_TOKEN_FILE (a file holding one fgp_ token) is required');
  const token = readFileSync(file, 'utf8').trim();
  if (!/^fgp_[A-Za-z0-9_]+$/.test(token)) throw new Error('GS_LT_TOKEN_FILE does not hold one fgp_ token');
  secret = { token, basic: Buffer.from(`${USER}:${token}`).toString('base64') };
  return secret;
}
/** Fail now, not at the first call, when the token is missing or malformed. */
export function requireToken() { credentials(); }
export const OUT = process.env.GS_LT_OUT ?? 'ops/loadtest/.out';
mkdirSync(OUT, { recursive: true });

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const now = () => Date.now();

/** One API call; returns {status, body, ms, requestId}. Never throws on HTTP errors. */
export async function api(method, path, body, { timeoutMs = 60000 } = {}) {
  const { token } = credentials();
  const t0 = performance.now();
  let res;
  try {
    res = await fetch(BASE + path, {
      method,
      headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json', Accept: 'application/json' },
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

/** git with the token as an extra header (never in argv URLs or .git/config). */
export async function git(cwd, args, { timeoutMs = 300000, env = {} } = {}) {
  const { basic } = credentials();
  const t0 = performance.now();
  try {
    const r = await pexec('git', ['-c', `http.extraHeader=Authorization: Basic ${basic}`, '-c', 'core.autocrlf=false', '-c', 'user.name=loadtest', '-c', 'user.email=loadtest@example.invalid', ...args], {
      cwd, timeout: timeoutMs, maxBuffer: 64 << 20,
      env: { ...process.env, GIT_TERMINAL_PROMPT: '0', ...env },
    });
    return { ok: true, ms: performance.now() - t0, stdout: r.stdout, stderr: r.stderr };
  } catch (e) {
    return { ok: false, code: e.code ?? null, ms: performance.now() - t0, stdout: e.stdout ?? '', stderr: String(e.stderr ?? e.message).replaceAll(basic, '***') };
  }
}
export async function gitOk(cwd, args, opts) {
  const r = await git(cwd, args, opts);
  if (!r.ok) throw new Error(`git ${args.slice(0, 3).join(' ')} failed: ${r.stderr.slice(0, 600)}`);
  return r;
}

const LOCAL_GIT = ['-c', 'core.autocrlf=false', '-c', 'user.name=loadtest', '-c', 'user.email=loadtest@example.invalid'];
/**
 * git on a local repository only: no credential, optional stdin (`input`),
 * `encoding: 'buffer'` for raw output. Returns {ok, code, ms, stdout, stderr}.
 */
export function localGit(cwd, args, { timeoutMs = 300000, input = null, encoding = 'utf8' } = {}) {
  const t0 = performance.now();
  return new Promise((done) => {
    const child = execFile('git', [...LOCAL_GIT, ...args], {
      cwd, timeout: timeoutMs, maxBuffer: 256 << 20, encoding,
      env: { ...process.env, GIT_TERMINAL_PROMPT: '0' },
    }, (err, stdout, stderr) => done({ ok: !err, code: err ? err.code ?? null : 0, ms: performance.now() - t0, stdout, stderr: String(stderr ?? '') || String(err?.message ?? '') }));
    if (input !== null) {
      // git ending before it read everything shows as its exit code, not here
      child.stdin.on('error', () => {});
      child.stdin.end(input);
    }
  });
}
export async function localGitOk(cwd, args, opts) {
  const r = await localGit(cwd, args, opts);
  if (!r.ok) throw new Error(`git ${args.slice(0, 3).join(' ')} failed: ${r.stderr.slice(0, 600)}`);
  return r;
}
/** The blobs `specs` (`<rev>:<path>`) name, as UTF-8 text, in one `cat-file --batch`. */
export async function readBlobs(cwd, specs) {
  const out = (await localGitOk(cwd, ['cat-file', '--batch'], { input: specs.map((x) => `${x}\n`).join(''), encoding: 'buffer' })).stdout;
  const texts = [];
  let off = 0;
  for (const spec of specs) {
    const nl = out.indexOf(0x0a, off);
    const [, type, size] = nl < 0 ? [] : out.subarray(off, nl).toString('utf8').split(' ');
    if (type !== 'blob') throw new Error(`${spec} is not a blob in the local clone`);
    const start = nl + 1;
    texts.push(out.subarray(start, start + Number(size)).toString('utf8'));
    off = start + Number(size) + 1;
  }
  return texts;
}
/**
 * One commit per change on top of `base`, written by one `fast-import`:
 * `changes` is [{ref, path, text, message}]; each `ref` is moved to its commit.
 */
export async function commitOnTop(cwd, base, changes) {
  const when = Math.floor(Date.now() / 1000);
  const parts = [];
  for (const c of changes) {
    const message = Buffer.from(`${c.message}\n`);
    const text = Buffer.from(c.text);
    parts.push(
      Buffer.from(`commit ${c.ref}\ncommitter loadtest <loadtest@example.invalid> ${when} +0000\ndata ${message.length}\n`), message,
      Buffer.from(`\nfrom ${base}\nM 100644 inline ${c.path}\ndata ${text.length}\n`), text, Buffer.from('\n\n'),
    );
  }
  await localGitOk(cwd, ['fast-import', '--quiet', '--force'], { input: Buffer.concat(parts) });
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
