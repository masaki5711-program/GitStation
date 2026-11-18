#!/usr/bin/env node
// synthetic — merge load test for the GitStation test environment
// (ops/loadtest/README.md). Scenarios:
//   a  documents: N MRs, each editing a different document, in bursts
//   b  documents with conflicts: M MRs on one document (overlapping and not)
//   c  system code: K MRs through the merge queue with a tiny CI; some break
//      the combined build only together with another MR (semantic conflict)
//   d  P parallel git pushes to different branches, then clone timing
//   a-burst  documents: N MRs created at once (no waves); the drain rate
//   a-xref  documents: N pairs of MRs, each green alone, that break a
//      reference together (one deletes a document, the other cites it), and N
//      plain revisions; the train's document lint (plan 18 S1) keeps every
//      first-parent commit of main free of broken references
//   e  soak: --count merges at --rate per minute with --burst minutes; samples
//      the backlog, the DB size and the repository size; says if the backlog grows
//   spawn-bench  local only (no server, no token): per-call time of
//      `git rev-parse HEAD` and `git merge-tree --write-tree` on this host
//   sample  forge CPU/memory, PG connections, job backlog, free memory every 10 s
//
// Usage (Git Bash or a Linux shell, from the worktree root):
//   GS_LT_TOKEN_FILE=<file with one fgp_ token> GS_LT_WORK=<scratch dir> \
//     node ops/loadtest/merge-load.mjs <a|a-burst|a-xref|b|c|d|e|sample> [--n 200] [--burst 50] [--tag run1]
//   GS_LT_WORK=<scratch dir> node ops/loadtest/merge-load.mjs spawn-bench [--n 100]
// Results: $GS_LT_OUT/<scenario>-<tag>.json (+ .jsonl event logs).
//
// Exit codes: 0 the run can be trusted; 1 it failed or did not finish (also:
// the queue never settled); 2 a wrong command line or environment (nothing
// was started); 3 STOP; 4 it finished and wrote its result, but the result
// must not be read as a pass (`verdict.reasons` in the result file).
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { performance } from 'node:perf_hooks';
import * as L from './lib.mjs';
import * as D from './docs-fixture.mjs';

const pexec = promisify(execFile);
const args = process.argv.slice(2);
const scenario = args[0];
const opt = (name, dflt) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : dflt;
};

// ---------------------------------------------------------------- startup checks
// Everything here runs before the token is read, a directory is made or a
// request is sent: a wrong command line ends with exit 2 and nothing started.
const USAGE = 'usage: merge-load.mjs <a|a-burst|a-xref|b|c|d|e|spawn-bench|sample> [--n N] [--burst N] [--tag T]\n'
  + '  e: [--count 10000] [--rate 21] [--burst 100] [--burst-every 30] [--duration MIN] [--docs 500] [--sample-every 30]\n'
  + '  --preflight: check the command line and the environment, then exit 0 without running';
/** A wrong command line or environment: say why and exit 2. */
function usage(message) {
  console.error(message);
  process.exit(2);
}
// the numeric options of each scenario with their minimum (all are integers)
const MR_OPTS = { n: 1, gap: 0, timeout: 1, 'retry-busy': 0 };
const NUMERIC = {
  a: { ...MR_OPTS, burst: 1, 'person-from': 0 },
  'a-burst': { ...MR_OPTS, 'person-from': 0 },
  'a-xref': { ...MR_OPTS, burst: 1 },
  b: MR_OPTS,
  c: { ...MR_OPTS, burst: 1 },
  d: { n: 1 },
  e: { count: 1, rate: 1, burst: 0, 'burst-every': 1, duration: 0, docs: 1, 'sample-every': 1, 'repo-size-every': 1, timeout: 1, 'retry-busy': 0 },
  'spawn-bench': { n: 1, docs: 2 },
  sample: {},
};
const TEXT_OPTS = ['tag', 'group', 'file', 'digest', 'from', 'https-base', 'ca-file', 'keep', 'preflight'];
if (!Object.hasOwn(NUMERIC, scenario ?? '')) usage(USAGE);
for (const a of args.slice(1)) {
  if (a.startsWith('--') && !TEXT_OPTS.includes(a.slice(2)) && !Object.hasOwn(NUMERIC[scenario], a.slice(2))) usage(`${a} is not an option of ${scenario}\n${USAGE}`);
}
/** `--name` as an integer of at least the scenario's minimum (exit 2 otherwise); `dflt` when it is not given. */
function num(name, dflt) {
  const min = NUMERIC[scenario][name];
  if (min === undefined) throw new Error(`--${name} is not a numeric option of ${scenario}`);
  const i = args.indexOf(`--${name}`);
  if (i < 0) return dflt;
  const raw = args[i + 1];
  // digits only: no sign, fraction, exponent or blank ("abc" and "" would be NaN and 0)
  const v = /^\d{1,15}$/.test(raw ?? '') ? Number(raw) : NaN;
  if (!(v >= min)) usage(`--${name} must be an integer >= ${min} (got ${raw === undefined ? 'no value' : JSON.stringify(raw)})`);
  return v;
}
for (const name of Object.keys(NUMERIC[scenario])) num(name, 0);
// the tag names files, directories (one is removed recursively) and projects
const NAME_RE = /^[A-Za-z0-9._-]+$/;
const TAG = opt('tag', new Date().toISOString().slice(0, 16).replace(/[:T]/g, ''));
if (!NAME_RE.test(TAG ?? '')) usage(`--tag must match ${NAME_RE} (got ${TAG === undefined ? 'no value' : JSON.stringify(TAG)})`);
const SAMPLE_FILE = opt('file', `samples-${TAG}.jsonl`);
if (!NAME_RE.test(SAMPLE_FILE ?? '')) usage(`--file must match ${NAME_RE}: a file name in the output directory, not a path`);
/** An on/off environment variable: unset, empty or 0 is off, 1 is on, anything else is refused. */
function flag(name) {
  const v = process.env[name] ?? '';
  if (!['', '0', '1'].includes(v)) usage(`${name} must be 0 or 1`);
  return v === '1';
}
const GROUP = opt('group', 'loadtest');
const WORK = process.env.GS_LT_WORK ?? path.join(L.OUT, 'work');
const PG = process.env.GS_LT_PG_CONTAINER ?? 'gs-m3-test-gs-test-pg-1';
// GS_LT_PSQL_DIRECT=1: `psql` of this host with the libpq environment
// (PGHOST, PGPORT, PGUSER, PGPASSFILE) instead of `docker exec` (a Linux host)
const PSQL_DIRECT = flag('GS_LT_PSQL_DIRECT');
const USES_SQL = !['d', 'spawn-bench'].includes(scenario);
// without PGHOST and PGPORT psql takes whatever PostgreSQL the default socket reaches
const directTargetMissing = () => ['PGHOST', 'PGPORT'].filter((v) => !process.env[v]);
if (PSQL_DIRECT && USES_SQL && directTargetMissing().length) {
  usage(`GS_LT_PSQL_DIRECT=1 needs ${directTargetMissing().join(' and ')} set explicitly: the database of the load test is named, never the default of this host`);
}
const PG_DB = process.env.GS_LT_PG_DB ?? 'forge';
// every session of the load test is read-only: a write fails in PostgreSQL
const READ_ONLY = '-c default_transaction_read_only=on';
// GS_LT_STORAGE_ROOT: forge's `storage.root` when it is on this host (the size of a repository)
const STORAGE_ROOT = process.env.GS_LT_STORAGE_ROOT ?? null;
const STOP_FILE = path.join(L.OUT, 'STOP');
const MIN_FREE_GB = Number(process.env.GS_LT_MIN_FREE_GB ?? 3);
if (!/^\d+(\.\d+)?$/.test(process.env.GS_LT_MIN_FREE_GB ?? '3')) usage('GS_LT_MIN_FREE_GB must be a number >= 0 (the memory floor in GB)');
// GS_LT_DOC_LINT_MISSING_FATAL=1: a first-parent commit of main without a
// `forge/doc-lint` run of its own fails the integrity check. Off by default,
// also after plan 18 S1: the train lint records its run on the train's last
// step only (the tree that lands), and only when a member changes a document —
// the steps before it are first-parent commits without a run of their own.
// `a-xref` proves those commits by reading them. It is reported either way.
const DOC_LINT_MISSING_FATAL = flag('GS_LT_DOC_LINT_MISSING_FATAL');
// a group (one minute of MRs) of the soak revises each document at most once
if (scenario === 'e' && num('docs', 500) < Math.max(num('rate', 21), num('burst', 100))) usage('--docs must be at least --rate and --burst');

/** Read-only SQL against the test database (superuser inside the container, or `psql` of this host). */
async function sql(q) {
  if (PSQL_DIRECT && directTargetMissing().length) throw new Error('GS_LT_PSQL_DIRECT=1 without PGHOST and PGPORT');
  const query = ['-v', 'ON_ERROR_STOP=1', '-AtF', '\t', '-c', q];
  const r = PSQL_DIRECT
    ? await pexec('psql', ['-X', '-d', PG_DB, ...query], {
      timeout: 60000, maxBuffer: 64 << 20,
      env: { ...process.env, PGOPTIONS: `${process.env.PGOPTIONS ?? ''} ${READ_ONLY}`.trim() },
    })
    : await pexec('docker', ['exec', '-e', `PGOPTIONS=${READ_ONLY}`, PG, 'psql', '-X', '-U', 'postgres', '-d', PG_DB, ...query], {
      timeout: 60000, maxBuffer: 64 << 20,
    });
  return r.stdout.trim() ? r.stdout.trim().split('\n').map((l) => l.split('\t')) : [];
}
const stopRequested = () => fs.existsSync(STOP_FILE);
function abortIfStopped() {
  if (stopRequested()) {
    console.error(`STOP file present (${STOP_FILE}): ${fs.readFileSync(STOP_FILE, 'utf8').trim()}`);
    process.exit(3);
  }
}
const iso = (ms) => new Date(ms).toISOString();
const secs = (a, b) => (Date.parse(b) - Date.parse(a)) / 1000;
const round1 = (o) => Object.fromEntries(Object.entries(o).map(([k, v]) => [k, typeof v === 'number' ? Math.round(v * 10) / 10 : v]));
const mean = (xs) => (xs.length ? xs.reduce((a, x) => a + x, 0) / xs.length : null);

// ---------------------------------------------------------------- sampler
/** Free memory and the forge processes on Linux, as the lines the sampler reads. */
function procfsLines() {
  const lines = [`free|${os.freemem() / 1024}`];
  for (const pid of fs.readdirSync('/proc').filter((d) => /^\d+$/.test(d))) {
    try {
      if (fs.readFileSync(`/proc/${pid}/comm`, 'utf8').trim() !== 'forge') continue;
      const role = fs.readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0').includes('worker') ? 'worker' : 'serve';
      const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
      // after "(comm) ": state is field 3, utime 14, stime 15 (clock ticks, 100 per second)
      const f = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
      const rssKb = Number(fs.readFileSync(`/proc/${pid}/status`, 'utf8').match(/^VmRSS:\s+(\d+) kB/m)?.[1] ?? 0);
      lines.push(`${pid}|${role}|${(Number(f[11]) + Number(f[12])) / 100}|${rssKb * 1024}`);
    } catch { /* the process ended between the listing and the read */ }
  }
  return lines.join('\n');
}
async function sampleOnce(prev) {
  const ps = process.platform !== 'win32' ? { stdout: procfsLines() } : await pexec('powershell', ['-NoProfile', '-Command',
    "$o=Get-CimInstance Win32_OperatingSystem; $p=Get-CimInstance Win32_Process -Filter \"name='forge.exe'\" | ForEach-Object { $g=Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue; if($g){ '{0}|{1}|{2}|{3}' -f $_.ProcessId,($(if($_.CommandLine -match ' worker'){'worker'}else{'serve'})),$g.CPU,$g.WorkingSet64 } }; 'free|' + $o.FreePhysicalMemory; $p"],
  { timeout: 30000 });
  const out = { t: new Date().toISOString(), procs: {} };
  for (const line of ps.stdout.trim().split(/\r?\n/)) {
    const f = line.split('|');
    if (f[0] === 'free') out.free_gb = Number(f[1]) / 1048576;
    else if (f.length === 4) {
      const cpu = Number(f[2]);
      const key = f[1];
      const p = prev?.procs?.[key];
      const dt = prev ? (Date.parse(out.t) - Date.parse(prev.t)) / 1000 : null;
      out.procs[key] = { pid: Number(f[0]), cpu_s: cpu, ws_mb: Math.round(Number(f[3]) / 1048576),
        cpu_pct: p && dt ? Math.round(((cpu - p.cpu_s) / dt) * 1000) / 10 : null };
    }
  }
  const conn = await sql("select coalesce(state,'?'), count(*) from pg_stat_activity where datname=current_database() group by 1");
  out.pg = Object.fromEntries(conn.map(([s, c]) => [s, Number(c)]));
  out.pg_total = conn.reduce((a, [, c]) => a + Number(c), 0);
  const jobs = await sql("select state, count(*) from forge.jobs where state not in ('succeeded') group by 1");
  out.jobs = Object.fromEntries(jobs.map(([s, c]) => [s, Number(c)]));
  const ci = await sql("select status, count(*) from forge.ci_jobs where status in ('pending','running') group by 1");
  out.ci_jobs = Object.fromEntries(ci.map(([s, c]) => [s, Number(c)]));
  return out;
}
// samples that fail this many times in a row end the sampler (10 s apart: five minutes)
const SAMPLER_FAILURES_END = 30;
async function sampler() {
  const file = path.join(L.OUT, SAMPLE_FILE);
  let prev = null;
  let failures = 0;
  for (;;) {
    try {
      const s = await sampleOnce(prev);
      fs.appendFileSync(file, JSON.stringify(s) + '\n');
      if (s.free_gb !== undefined && s.free_gb < MIN_FREE_GB && !stopRequested()) {
        fs.writeFileSync(STOP_FILE, `free memory ${s.free_gb.toFixed(2)} GB < ${MIN_FREE_GB} GB at ${s.t}\n`);
        console.error('free memory below the floor: STOP written');
      }
      prev = s;
      failures = 0;
    } catch (e) {
      const t = new Date().toISOString();
      const error = String(e.message).slice(0, 300);
      fs.appendFileSync(file, JSON.stringify({ t, sample_error: error }) + '\n');
      if (++failures >= SAMPLER_FAILURES_END) {
        // nobody watches the memory floor any more: the run stops instead of going on blind
        const why = `the sampler failed ${failures} times in a row at ${t}: the memory floor is not watched`;
        fs.appendFileSync(file, JSON.stringify({ t, sampler_failed: why }) + '\n');
        if (!stopRequested()) fs.writeFileSync(STOP_FILE, `${why}\n`);
        console.error(`${why} (STOP written). Last error: ${error}`);
        process.exit(1);
      }
    }
    await L.sleep(10000);
  }
}

// ---------------------------------------------------------------- shared
async function protectMain(projectId, requiredChecks) {
  const pr = await L.apiOk('POST', `/api/v1/projects/${projectId}/protected-refs`, { pattern: 'refs/heads/main', push_min_role: 'maintainer' });
  return L.apiOk('PUT', `/api/v1/projects/${projectId}/protected-refs/${pr.id}/merge-rule`, {
    required_approvals: 0, required_checks: requiredChecks, require_resolved_discussions: false,
    direct_push_forbidden: true, require_work_item_link: false, expected_updated_at: null,
  });
}
/** A new project with `files` (path -> content) on main, then main protected. */
async function seedProject(name, files, requiredChecks) {
  const proj = await L.ensureProject(GROUP, name);
  if (!proj.created) throw new Error(`${GROUP}/${name} exists already: pass another --tag`);
  const dir = path.join(WORK, name);
  fs.mkdirSync(dir, { recursive: true });
  await L.gitOk(dir, ['init', '-q', '-b', 'main']);
  for (const [p, c] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(dir, p)), { recursive: true });
    fs.writeFileSync(path.join(dir, p), c);
  }
  await L.gitOk(dir, ['add', '--', ...Object.keys(files)]);
  await L.gitOk(dir, ['commit', '-qm', 'seed: synthetic load-test content']);
  // the repository is provisioned by a background job: a push before it answers 503
  const tProv = Date.now();
  const ready = await L.until(async () => (await L.git(dir, ['ls-remote', L.repoUrl(GROUP, name)])).ok, { timeoutMs: 120000, everyMs: 1000 });
  if (!ready) throw new Error(`${GROUP}/${name}: repository not provisioned after 120 s`);
  console.log(`repository provisioned ${Math.round((Date.now() - tProv) / 100) / 10} s after create`);
  await L.gitOk(dir, ['push', '-q', L.repoUrl(GROUP, name), 'main']);
  await protectMain(proj.id, requiredChecks);
  return { id: proj.id, dir, name };
}
/** One commit per branch on top of main; all branches in one push (chunks of 50). */
async function makeBranches(p, branches /* [{name, files:{path:content|null}}] */) {
  const base = (await L.gitOk(p.dir, ['rev-parse', 'main'])).stdout.trim();
  for (const b of branches) {
    await L.gitOk(p.dir, ['checkout', '-q', '-B', b.name, base]);
    for (const [f, c] of Object.entries(b.files)) {
      const full = path.join(p.dir, f);
      if (c === null) fs.rmSync(full);
      else { fs.mkdirSync(path.dirname(full), { recursive: true }); fs.writeFileSync(full, c); }
    }
    await L.gitOk(p.dir, ['add', '-A', '--', ...Object.keys(b.files).map((f) => path.dirname(f))]);
    await L.gitOk(p.dir, ['commit', '-qm', b.message ?? `change ${b.name}`]);
  }
  await L.gitOk(p.dir, ['checkout', '-q', 'main']);
  for (let i = 0; i < branches.length; i += 50) {
    const refs = branches.slice(i, i + 50).map((b) => `${b.name}:refs/heads/${b.name}`);
    await L.gitOk(p.dir, ['push', '-q', L.repoUrl(GROUP, p.name), ...refs], { timeoutMs: 600000 });
  }
}
// --retry-busy N: a client that retries a 503 FG-GIT-0004 (git busy) up to N times, 1 s apart
const RETRY_BUSY = Object.hasOwn(NUMERIC[scenario], 'retry-busy') ? num('retry-busy', 0) : 0;
// --gap S: seconds between two waves of MRs
const GAP_S = Object.hasOwn(NUMERIC[scenario], 'gap') ? num('gap', 30) : 30;
async function createMr(p, b) {
  const t = Date.now();
  let r;
  let busyRetries = 0;
  for (;;) {
    r = await L.api('POST', `/api/v1/projects/${p.id}/merge-requests`, {
      title: b.title ?? b.name, description: 'synthetic load test', source_branch: b.name, target_branch: 'main', draft: false,
    });
    if (r.status !== 503 || r.body?.error_code !== 'FG-GIT-0004' || busyRetries >= RETRY_BUSY) break;
    busyRetries++;
    await L.sleep(1000);
  }
  const rec = { branch: b.name, t_create: iso(t), create_ms: Math.round(r.ms), create_status: r.status, busy_retries: busyRetries,
    iid: r.body?.iid ?? null, create_error: r.status === 201 ? null : r.body?.error_code ?? r.body?.error ?? r.status,
    create_request_id: r.requestId };
  return rec;
}
/** A person's "merge when ready" (POST …/merge-queue) on the MR's current head. */
async function enqueue(p, rec) {
  const head = await L.until(async () => {
    const m = await L.api('GET', `/api/v1/projects/${p.id}/merge-requests/${rec.iid}`);
    return m.body?.head_sha ?? null;
  }, { timeoutMs: 60000, everyMs: 500 });
  // FG-REVIEW-0007 while a required check (not ci/pipeline) is still running: retry every 2 s for 60 s
  const t0 = Date.now();
  const first = [];
  for (let attempt = 1; ; attempt++) {
    const r = await L.api('POST', `/api/v1/projects/${p.id}/merge-requests/${rec.iid}/merge-queue`, { sha: head });
    first.push(Math.round(r.ms));
    const code = r.status >= 200 && r.status < 300 ? null : r.body?.error_code ?? r.status;
    if (code !== 'FG-REVIEW-0007' || Date.now() - t0 > 60000) {
      Object.assign(rec, { enqueue_ms: first[0], enqueue_ms_all: first, enqueue_attempts: attempt, enqueue_status: r.status,
        enqueue_error: code, enqueue_request_id: r.requestId, enqueue_wait_s: (Date.now() - t0) / 1000 });
      return rec;
    }
    await L.sleep(2000);
  }
}
/** Wait until every MR is merged or its queue entry is final and no entry is live. */
async function waitSettled(p, iids, { timeoutMs, label }) {
  const t0 = Date.now();
  let last = '';
  for (;;) {
    abortIfStopped();
    const rows = await sql(`select mr.state, count(*) from forge.merge_requests mr where mr.project_id=${p.id} and mr.iid in (${iids.join(',')}) group by 1`);
    const live = await sql(`select count(*) from forge.merge_queue_entries where project_id=${p.id} and state in ('queued','testing')`);
    const st = Object.fromEntries(rows.map(([s, c]) => [s, Number(c)]));
    const line = `${label}: ${JSON.stringify(st)} live_entries=${live[0][0]} t=${Math.round((Date.now() - t0) / 1000)}s`;
    if (line.replace(/t=\d+s/, '') !== last) { console.log(line); last = line.replace(/t=\d+s/, ''); }
    const merged = st.merged ?? 0;
    if (merged === iids.length) return { settled: true, waited_s: (Date.now() - t0) / 1000 };
    if (Number(live[0][0]) === 0 && Date.now() - t0 > 90000) {
      // nothing live for a while: auto-merge may still be about to queue; give it 2 recheck rounds
      const idle = await L.until(async () => Number((await sql(`select count(*) from forge.merge_queue_entries where project_id=${p.id} and state in ('queued','testing')`))[0][0]) > 0, { timeoutMs: 60000, everyMs: 5000 });
      if (!idle) return { settled: true, waited_s: (Date.now() - t0) / 1000, not_merged: iids.length - merged };
    }
    if (Date.now() - t0 > timeoutMs) return { settled: false, waited_s: (Date.now() - t0) / 1000 };
    await L.sleep(5000);
  }
}
async function queueFacts(p) {
  const entries = await sql(`select e.id, mr.iid, e.state, e.batch_id, e.reason, e.error_code, e.isolate, e.created_at, e.finished_at, e.enqueued_by, e.credential_kind
    from forge.merge_queue_entries e join forge.merge_requests mr on mr.id=e.merge_request_id where e.project_id=${p.id} order by e.id`);
  const batches = await sql(`select id, state, size, reason, error_code, created_at, finished_at from forge.merge_queue_batches where project_id=${p.id} order by id`);
  const mrs = await sql(`select mr.iid, mr.state, mr.created_at, m.finished_at, m.state, m.error_code, m.reason from forge.merge_requests mr
    left join forge.merge_request_merges m on m.merge_request_id=mr.id and m.state='applied' where mr.project_id=${p.id} order by mr.iid`);
  const am = await sql(`select mr.iid, a.state, a.reason, a.error_code, a.class from forge.auto_merge_states a join forge.merge_requests mr on mr.id=a.merge_request_id where a.project_id=${p.id}`);
  return {
    entries: entries.map(([id, iid, state, batch, reason, code, isolate, created, finished, by, cred]) => ({ id: +id, iid: +iid, state, batch: batch ? +batch : null, reason: reason || null, error_code: code || null, isolate: isolate || null, created, finished: finished || null, enqueued_by: +by, credential_kind: cred })),
    batches: batches.map(([id, state, size, reason, code, created, finished]) => ({ id: +id, state, size: +size, reason: reason || null, error_code: code || null, created, finished: finished || null })),
    mrs: mrs.map(([iid, state, created, mergedAt, mstate, code, reason]) => ({ iid: +iid, state, created, merged_at: mergedAt || null })),
    auto_merge: am.map(([iid, state, reason, code, cls]) => ({ iid: +iid, state, reason, error_code: code || null, class: cls || null })),
  };
}
/** Per-batch durations from the batches' own timestamps (created to finished; seconds). */
function batchDurations(batches) {
  const done = batches.filter((b) => b.finished);
  const dur = (b) => secs(b.created, b.finished);
  const grouped = (rows, key) => {
    const g = {};
    for (const b of rows) (g[key(b)] ??= []).push(dur(b));
    return Object.fromEntries(Object.entries(g).map(([k, v]) => [k, round1(L.stats(v))]));
  };
  const landed = done.filter((b) => b.state === 'landed');
  // one batch at a time per queue: from one batch's end to the next one's row
  const gaps = [];
  for (let i = 1; i < batches.length; i++) if (batches[i - 1].finished) gaps.push(secs(batches[i - 1].finished, batches[i].created));
  return {
    all_s: round1(L.stats(done.map(dur))),
    by_state_s: grouped(done, (b) => b.state),
    landed_by_size_s: grouped(landed, (b) => b.size),
    landed_per_item_s: round1(L.stats(landed.map((b) => dur(b) / b.size))),
    gap_to_next_batch_s: round1(L.stats(gaps)),
  };
}
function summarize(recs, facts) {
  const byIid = new Map(recs.filter((r) => r.iid).map((r) => [r.iid, r]));
  const firstEntry = new Map();
  for (const e of facts.entries) if (!firstEntry.has(e.iid)) firstEntry.set(e.iid, e);
  const enqToMerged = [];
  const createToMerged = [];
  const mergedTimes = [];
  for (const m of facts.mrs) {
    if (!byIid.has(m.iid) || !m.merged_at) continue;
    const e = firstEntry.get(m.iid);
    if (e) enqToMerged.push(secs(e.created, m.merged_at));
    createToMerged.push(secs(byIid.get(m.iid).t_create, m.merged_at));
    mergedTimes.push(Date.parse(m.merged_at));
  }
  const firstCreate = Math.min(...recs.map((r) => Date.parse(r.t_create)));
  const lastMerged = mergedTimes.length ? Math.max(...mergedTimes) : null;
  // the drain: merges per minute from the last create (its answer) to the last merge
  const lastCreate = Math.max(...recs.map((r) => Date.parse(r.t_create) + r.create_ms));
  const drained = mergedTimes.filter((t) => t > lastCreate).length;
  const drain = lastMerged && lastMerged > lastCreate
    ? { last_create: iso(lastCreate), last_merged: iso(lastMerged), drain_s: Math.round((lastMerged - lastCreate) / 100) / 10,
      merged_after_last_create: drained, per_min: Math.round((drained / ((lastMerged - lastCreate) / 60000)) * 10) / 10 }
    : null;
  const perMinute = {};
  for (const t of mergedTimes) { const k = Math.floor((t - firstCreate) / 60000); perMinute[k] = (perMinute[k] ?? 0) + 1; }
  const codes = {};
  const bump = (k) => { if (k) codes[k] = (codes[k] ?? 0) + 1; };
  for (const r of recs) { bump(r.create_error && `create:${r.create_error}`); bump(r.enqueue_error && `enqueue:${r.enqueue_error}`); }
  for (const e of facts.entries) bump(e.error_code && `entry:${e.error_code}:${e.reason}`);
  for (const b of facts.batches) bump(b.error_code && `batch:${b.error_code}:${b.reason}`);
  for (const a of facts.auto_merge) bump(a.error_code && `auto_merge:${a.error_code}:${a.reason}`);
  const batchSizes = {};
  for (const b of facts.batches) batchSizes[`${b.size}:${b.state}`] = (batchSizes[`${b.size}:${b.state}`] ?? 0) + 1;
  const round = round1;
  return {
    mrs: recs.length,
    merged: mergedTimes.length,
    enqueue_to_merged_s: round(L.stats(enqToMerged)),
    create_to_merged_s: round(L.stats(createToMerged)),
    create_to_enqueue_s: round(L.stats([...firstEntry.values()].filter((e) => byIid.has(e.iid)).map((e) => secs(byIid.get(e.iid).t_create, e.created)))),
    enqueued_by_person: facts.entries.filter((e) => byIid.has(e.iid) && e.credential_kind !== 'auto_merge' && e.enqueued_by === 1).length,
    span_s: lastMerged ? Math.round((lastMerged - firstCreate) / 100) / 10 : null,
    throughput_per_min: lastMerged ? Math.round((mergedTimes.length / ((lastMerged - firstCreate) / 60000)) * 10) / 10 : null,
    drain,
    merged_per_minute: perMinute,
    create_ms: round(L.stats(recs.map((r) => r.create_ms))),
    enqueue_ms: round(L.stats(recs.filter((r) => r.enqueue_ms_all).flatMap((r) => r.enqueue_ms_all))),
    enqueue_final: recs.filter((r) => r.enqueue_attempts).reduce((a, r) => { const k = r.enqueue_error ?? 'ok'; a[k] = (a[k] ?? 0) + 1; return a; }, {}),
    batches: { count: facts.batches.length, by_size_state: batchSizes, duration: batchDurations(facts.batches) },
    entries: { count: facts.entries.length, by_state: facts.entries.reduce((a, e) => ((a[e.state] = (a[e.state] ?? 0) + 1), a), {}) },
    error_codes: codes,
  };
}
async function burstCreate(p, branches, burst, { enqueueByPerson }) {
  const recs = [];
  for (let i = 0; i < branches.length; i += burst) {
    abortIfStopped();
    const slice = branches.slice(i, i + burst);
    const made = await Promise.all(slice.map((b) => createMr(p, b)));
    if (enqueueByPerson) await Promise.all(made.filter((r) => r.iid).map((r) => enqueue(p, r)));
    for (const r of made) { recs.push(r); L.log(`${scenario}-${TAG}.jsonl`, r); }
    console.log(`burst at MR ${made[0]?.branch}: ${made.length} MRs (create p95 ${L.pct(made.map((r) => r.create_ms), 95)} ms)`);
    if (i + burst < branches.length) await L.sleep(GAP_S * 1000);
  }
  return recs;
}
async function gitShow(p, ref, file) {
  await L.gitOk(p.dir, ['fetch', '-q', L.repoUrl(GROUP, p.name), `+refs/heads/main:refs/remotes/origin/main`]);
  return (await L.gitOk(p.dir, ['show', `origin/main:${file}`])).stdout;
}
/** `read()` again every 5 s while `open(rows)` (something still runs), for at most `waitMs`. */
async function readWhenFinal(read, open, waitMs) {
  const end = Date.now() + waitMs;
  for (;;) {
    const rows = await read();
    if (!open(rows) || Date.now() >= end) return rows;
    await L.sleep(5000);
  }
}
/**
 * What must hold on `main` after every scenario (handoff 2026-11-17, plan 18
 * §1): no conflict marker in main's tree, no push pipeline on a first-parent
 * commit that did not succeed, and (documents) no `forge/doc-lint` run on a
 * first-parent commit that is not success — every commit, not only the tip.
 * Pipelines and runs still in flight right after the last merge are waited
 * for (`waitMs`); one that is still not final then counts as not succeeded.
 *
 * A commit without a check run of its own is `missing`
 * (`missing_merged_head_success` counts those whose merged head, the second
 * parent, has a success). `missing` is always reported; it fails the check
 * only with GS_LT_DOC_LINT_MISSING_FATAL=1 (the train lint of plan 18 S1
 * records its run on the train's last step only).
 * `failures` lists what failed; `ok` is `failures` being empty.
 */
async function mainIntegrity(p, { docLint, waitMs = 300000 }) {
  await L.gitOk(p.dir, ['fetch', '-q', L.repoUrl(GROUP, p.name), '+refs/heads/main:refs/remotes/origin/main']);
  const line = (await L.gitOk(p.dir, ['rev-list', '--first-parent', '--parents', 'origin/main'])).stdout.trim().split('\n').map((l) => l.split(' '));
  const onLine = new Set(line.map(([sha]) => sha));
  // A file counts when it has the start or the end line of a conflict (git
  // writes both around every conflict). A `=======` line alone does not: it
  // is also the underline of a Markdown setext heading.
  // git grep: exit 0 = a match, 1 = none
  const grep = await L.git(p.dir, ['grep', '-l', '-I', '-E', CONFLICT_MARKER.source, 'origin/main', '--']);
  if (!grep.ok && grep.code !== 1) throw new Error(`git grep failed: ${grep.stderr.slice(0, 300)}`);
  const marked = grep.ok ? grep.stdout.trim().split('\n').map((f) => f.replace(/^origin\/main:/, '')) : [];
  const pipes = new Map(await readWhenFinal(
    () => sql(`select sha, status from forge.ci_pipelines where project_id=${p.id} and source='push' order by id`),
    (rows) => rows.some(([sha, st]) => onLine.has(sha) && (st === 'pending' || st === 'running')), waitMs));
  const notGreen = line.filter(([sha]) => pipes.has(sha) && pipes.get(sha) !== 'succeeded').map(([sha]) => ({ sha, status: pipes.get(sha) }));
  const out = {
    main_sha: line[0][0], first_parent_commits: line.length,
    conflict_marker_files: marked,
    push_pipelines_not_succeeded: notGreen.length, push_pipelines_not_succeeded_first: notGreen.slice(0, 20),
  };
  let lint = null;
  if (docLint) {
    const runs = new Map(await readWhenFinal(
      () => sql(`select head_sha, coalesce(conclusion, status) from forge.check_runs where project_id=${p.id} and name='forge/doc-lint' order by id`),
      (rows) => rows.some(([sha, c]) => onLine.has(sha) && (c === 'queued' || c === 'in_progress')), waitMs));
    lint = { success: 0, not_success: [], missing: 0, missing_merged_head_success: 0, missing_first: [], tip: runs.get(line[0][0]) ?? null };
    for (const [sha, , mergedHead] of line) {
      const own = runs.get(sha);
      if (own === 'success') lint.success++;
      else if (own) lint.not_success.push({ sha, conclusion: own });
      else {
        lint.missing++;
        if (mergedHead && runs.get(mergedHead) === 'success') lint.missing_merged_head_success++;
        if (lint.missing_first.length < 20) lint.missing_first.push(sha);
      }
    }
    lint.every_first_parent_commit_success = lint.success === line.length;
    lint.not_success_count = lint.not_success.length;
    lint.not_success = lint.not_success.slice(0, 50);
    lint.missing_fatal = DOC_LINT_MISSING_FATAL;
    out.doc_lint_first_parent = lint;
  }
  out.failures = integrityFailures({ markerFiles: marked.length, pipelinesNotSucceeded: notGreen.length,
    lintNotSuccess: lint?.not_success_count ?? 0, lintMissing: lint?.missing ?? 0, missingFatal: DOC_LINT_MISSING_FATAL });
  out.ok = out.failures.length === 0;
  return out;
}
// ---- the verdict: pure functions (facts in, reasons and the exit code out)
/** The start or the end line of a conflict as git writes it (POSIX ERE, also a JS pattern). */
const CONFLICT_MARKER = /^(<<<<<<<|>>>>>>>)( |$)/m;
/** What fails the integrity of main, from its counts. A missing doc-lint run fails it only when `missingFatal`. */
function integrityFailures({ markerFiles, pipelinesNotSucceeded, lintNotSuccess, lintMissing, missingFatal }) {
  const failures = [];
  if (markerFiles) failures.push(`${markerFiles} file(s) with a conflict marker`);
  if (pipelinesNotSucceeded) failures.push(`${pipelinesNotSucceeded} first-parent commit(s) whose push pipeline did not succeed`);
  if (lintNotSuccess) failures.push(`${lintNotSuccess} first-parent commit(s) whose forge/doc-lint run is not success`);
  if (lintMissing && missingFatal) failures.push(`${lintMissing} first-parent commit(s) without a forge/doc-lint run (GS_LT_DOC_LINT_MISSING_FATAL=1)`);
  return failures;
}
/** The created MRs that are not merged, as `!iid`. */
const notMerged = (recs, merged) => recs.filter((r) => r.iid && !merged.has(r.iid)).map((r) => `!${r.iid}`);
/**
 * Scenario b against its plan: every non-overlapping MR merges, and of the
 * overlapping ones (all rewrite step 1) exactly one does — the others are the
 * conflicts the scenario plants, not failures.
 */
function offPlanB(recs, merged) {
  const off = notMerged(recs.filter((r) => r.kind === 'separate'), merged).map((x) => `${x} (non-overlapping) did not merge`);
  const overlap = recs.filter((r) => r.iid && r.kind === 'overlap');
  const won = overlap.filter((r) => merged.has(r.iid)).length;
  if (overlap.length && won !== 1) off.push(`${won} of ${overlap.length} overlapping MRs merged (exactly one must)`);
  return off;
}
/**
 * Scenario c against its plan: of each planted pair exactly one MR merges
 * (whichever comes second breaks the combined build and is dropped: the stop
 * the scenario plants), and every other MR merges.
 */
function offPlanC(recs, merged, pairs) {
  const present = pairs.filter(([a, b]) => [a, b].every((i) => recs.some((r) => r.i === i)));
  const paired = new Set(present.flatMap(([a, b]) => [a, b]));
  const off = notMerged(recs.filter((r) => !paired.has(r.i)), merged).map((x) => `${x} did not merge`);
  for (const [a, b, how] of present) {
    const both = [a, b].map((i) => recs.find((r) => r.i === i));
    if (both.some((r) => !r.iid)) continue; // one was not created: counted as a failed create
    const won = both.filter((r) => merged.has(r.iid)).length;
    if (won !== 1) off.push(`planted pair ${a}/${b} (${how}): ${won} merged (exactly one must)`);
  }
  return off;
}
/** A reference to a fixture document, `[[LT-0001]]` (the same pattern as a POSIX ERE for `git grep`). */
const DOC_REF = /\[\[(LT-\d{4})\]\]/g;
const DOC_REF_ERE = '\\[\\[LT-[0-9]{4}\\]\\]';
/**
 * The broken references of one tree of the fixture. `paths`: its files;
 * `cites`: one `<path>:[[LT-0001]]` line per reference in it. A reference is
 * broken when the tree has no document of that id. Returns `<path> -> <id>`.
 */
function brokenRefs(paths, cites) {
  const ids = new Set(paths.map((f) => /(LT-\d{4})\.md$/.exec(f)?.[1]).filter(Boolean));
  const broken = [];
  for (const line of cites) {
    const at = line.lastIndexOf(':[[');
    for (const m of line.slice(at + 1).matchAll(DOC_REF)) if (!ids.has(m[1])) broken.push(`${line.slice(0, Math.max(at, 0))} -> ${m[1]}`);
  }
  return broken;
}
/** The train lint's removal (plan 18 S1): the entry's code and reason. */
const LINT_REMOVAL = { code: 'FG-CI-0032', reason: 'doc_lint_failed' };
/**
 * Scenario a-xref against its plan: every plain revision merges and is never
 * removed by the train lint; of each pair (`kind` delete / cite, pair `k`)
 * exactly one MR merges, and the other one left the queue exactly once with
 * FG-CI-0032 `doc_lint_failed` — one removal per cause, none anywhere else.
 */
function offPlanXref(recs, merged, entries) {
  const removals = (iid) => entries.filter((e) => e.iid === iid && e.error_code === LINT_REMOVAL.code && e.reason === LINT_REMOVAL.reason).length;
  const plain = recs.filter((r) => r.kind === 'plain');
  const off = notMerged(plain, merged).map((x) => `${x} (plain revision) did not merge`);
  for (const r of plain) if (r.iid && removals(r.iid)) off.push(`!${r.iid} (plain revision) was removed by the train lint`);
  const pairs = [...new Set(recs.filter((r) => r.kind !== 'plain').map((r) => r.k))].sort((a, b) => a - b);
  for (const k of pairs) {
    const both = recs.filter((r) => r.kind !== 'plain' && r.k === k);
    if (both.length !== 2 || both.some((r) => !r.iid)) continue; // one was not created: counted as a failed create
    const won = both.filter((r) => merged.has(r.iid));
    if (won.length !== 1) { off.push(`pair ${k}: ${won.length} merged (exactly one must)`); continue; }
    const lost = both.find((r) => !merged.has(r.iid));
    if (removals(lost.iid) !== 1 || removals(won[0].iid) !== 0) {
      off.push(`pair ${k}: ${removals(lost.iid)} ${LINT_REMOVAL.code} removal(s) of !${lost.iid} (not merged) and ${removals(won[0].iid)} of !${won[0].iid} (merged); exactly one of the MR that did not merge`);
    }
  }
  return off;
}
/**
 * Can the result of a run be trusted? `code` is the exit code: 0 = yes;
 * 3 = stopped by the STOP file; 1 = it failed, or the queue never settled (an
 * unfinished run); 4 = it finished, but it must not be read as a pass.
 *   recs          one record per create attempt (`iid` null: not created)
 *   createErrors  creates that threw; skipped: MRs whose branch was not pushed
 *   offPlan       what went against the scenario's plan (`notMerged`, `offPlanB`, `offPlanC`, `offPlanXref`)
 *   lost          merged revisions that are not on main
 *   extra         further reasons of the scenario
 */
function verdict({ settle, integrity, recs, createErrors = 0, skipped = 0, offPlan = [], lost = 0, extra = [], stopped = null, failure = null }) {
  const reasons = [];
  if (stopped) reasons.push(`stopped: ${stopped}`);
  if (failure) reasons.push(`failed: ${failure}`);
  const unsettled = !stopped && !failure && settle?.settled !== true;
  if (unsettled) reasons.push('the queue did not settle before --timeout: the numbers are of an unfinished run');
  const failed = recs.filter((r) => !r.iid);
  if (failed.length || createErrors) {
    const codes = {};
    for (const r of failed) codes[r.create_error ?? r.create_status] = (codes[r.create_error ?? r.create_status] ?? 0) + 1;
    reasons.push(`${failed.length + createErrors} of ${recs.length + createErrors} merge requests were not created ${JSON.stringify(codes)}`);
  }
  if (skipped) reasons.push(`${skipped} merge requests were skipped: their branches could not be pushed`);
  if (offPlan.length) reasons.push(`against the scenario's plan (${offPlan.length}): ${offPlan.slice(0, 10).join('; ')}${offPlan.length > 10 ? '; ...' : ''}`);
  if (lost) reasons.push(`${lost} merged revision(s) are not on main`);
  for (const f of integrity.failures) reasons.push(`main: ${f}`);
  reasons.push(...extra);
  const code = reasons.length === 0 ? 0 : stopped ? 3 : failure || unsettled ? 1 : 4;
  return { ok: code === 0, code, reasons };
}
/** Is `target` strictly inside `dir`, both resolved first (`..` segments are gone)? */
function isInside(dir, target) {
  const rel = path.relative(path.resolve(dir), path.resolve(target));
  return rel !== '' && rel !== '..' && !rel.startsWith(`..${path.sep}`) && !path.isAbsolute(rel);
}
// ---- end of the pure functions
/** After the result file is written: say why the run is not to be trusted and set the exit code. */
function finish(v) {
  for (const r of v.reasons) console.error(`NOT OK: ${r}`);
  // not process.exit(): what was printed is flushed first
  process.exitCode = v.code;
}
/** The iids of the merged MRs in `facts`. */
const mergedIids = (facts) => new Set(facts.mrs.filter((x) => x.merged_at).map((x) => x.iid));
/** Remove a scratch directory with everything in it — only one that lies inside GS_LT_WORK. */
function removeUnderWork(target) {
  if (!isInside(WORK, target)) {
    console.error(`not removed: ${path.resolve(target)} is not inside the work directory ${path.resolve(WORK)}`);
    process.exitCode = 1;
    return;
  }
  fs.rmSync(target, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
}
/** The integrity of main in one line (the lists stay in the result file). */
const integrityLine = (g) => JSON.stringify({ ...g, push_pipelines_not_succeeded_first: undefined,
  doc_lint_first_parent: g.doc_lint_first_parent && { ...g.doc_lint_first_parent, not_success: undefined, missing_first: undefined } });

// ---------------------------------------------------------------- A, A-burst
/** `a`: waves of --burst MRs, --gap seconds apart. `a-burst`: all --n MRs at once. */
async function scenarioA({ atOnce = false } = {}) {
  const key = atOnce ? 'a-burst' : 'a';
  const n = num('n', 200);
  const burst = atOnce ? n : num('burst', 50);
  // MRs >= this index are enqueued by a person too (a-burst: none, auto-merge alone drains it)
  const personFrom = num('person-from', atOnce ? n : Math.floor(n / 2));
  const files = { '.forge/doctypes/lt.procedure.yml': D.DOCTYPE };
  for (let i = 1; i <= n; i++) files[D.docPath(i)] = D.doc(i);
  const p = await seedProject(`docs-${atOnce ? 'ab' : 'a'}-${TAG}`, files, ['forge/doc-lint']);
  console.log(`project ${GROUP}/${p.name} id ${p.id}: ${n} documents, main requires forge/doc-lint`);
  const branches = [];
  for (let i = 1; i <= n; i++) {
    branches.push({ name: `a/doc-${i}`, title: `${D.docId(i)} 改訂`, files: { [D.docPath(i)]: D.doc(i, { edits: { 3: `手順 3: 改訂 ${TAG}。設備 ${i} の部位 3 を測定器で確かめる。` } }) } });
  }
  const tPush = Date.now();
  await makeBranches(p, branches);
  console.log(`pushed ${n} branches in ${Math.round((Date.now() - tPush) / 1000)} s`);
  const t0 = Date.now();
  const recs = [];
  for (let i = 0; i < n; i += burst) {
    const part = branches.slice(i, i + burst);
    const person = i >= personFrom;
    const got = await burstCreate(p, part, burst, { enqueueByPerson: person });
    recs.push(...got);
    if (i + burst < n) await L.sleep(GAP_S * 1000);
  }
  const settle = await waitSettled(p, recs.filter((r) => r.iid).map((r) => r.iid), { timeoutMs: num('timeout', 3600) * 1000, label: key });
  const facts = await queueFacts(p);
  const lint = await sql(`select c.conclusion, count(*) from forge.check_runs c where c.project_id=${p.id} and c.name='forge/doc-lint' group by 1`);
  // integrity: every document on main carries its revision
  const missing = [];
  await L.gitOk(p.dir, ['fetch', '-q', L.repoUrl(GROUP, p.name), '+refs/heads/main:refs/remotes/origin/main']);
  for (let i = 1; i <= n; i++) {
    const body = (await L.gitOk(p.dir, ['show', `origin/main:${D.docPath(i)}`])).stdout;
    if (!body.includes(`改訂 ${TAG}`)) missing.push(i);
  }
  const listed = await L.api('GET', `/api/v1/projects/${p.id}/documents`);
  const docsListed = { status: listed.status, count: listed.body?.documents?.length ?? null, ms: Math.round(listed.ms) };
  const integrity = await mainIntegrity(p, { docLint: true });
  // every created MR merges, and the revision of a merged MR is on main
  const merged = mergedIids(facts);
  const lost = missing.filter((i) => merged.has(recs.find((r) => r.branch === `a/doc-${i}`)?.iid));
  const v = verdict({ settle, integrity, recs, offPlan: notMerged(recs, merged).map((x) => `${x} did not merge`), lost: lost.length });
  const result = { scenario: key, tag: TAG, verdict: v, docs_listed_on_main: docsListed, project: `${GROUP}/${p.name}`, project_id: p.id, n, burst, person_from: personFrom,
    started: iso(t0), settle, summary: summarize(recs, facts), doc_lint_runs: Object.fromEntries(lint), revisions_missing_on_main: missing,
    merged_revisions_missing_on_main: lost, integrity, facts };
  L.save(`${key}-${TAG}.json`, result);
  console.log(JSON.stringify(result.summary, null, 1), 'missing on main:', missing.length);
  console.log('integrity of main:', integrityLine(integrity));
  finish(v);
}

// ---------------------------------------------------------------- A-xref
/** The broken references of every first-parent commit of main (local git on the fetched history). */
async function brokenRefsOnMain(p) {
  await L.gitOk(p.dir, ['fetch', '-q', L.repoUrl(GROUP, p.name), '+refs/heads/main:refs/remotes/origin/main']);
  const line = (await L.gitOk(p.dir, ['rev-list', '--first-parent', 'origin/main'])).stdout.trim().split('\n');
  const bad = [];
  for (const sha of line) {
    const paths = (await L.gitOk(p.dir, ['ls-tree', '-r', '--name-only', sha, '--', 'docs/procedures'])).stdout.trim().split('\n');
    // git grep: exit 0 = a match, 1 = none; a line is `<sha>:<path>:<match>`
    const grep = await L.git(p.dir, ['grep', '-o', '-I', '-E', DOC_REF_ERE, sha, '--', 'docs/procedures']);
    if (!grep.ok && grep.code !== 1) throw new Error(`git grep failed: ${grep.stderr.slice(0, 300)}`);
    const cites = grep.ok ? grep.stdout.trim().split('\n').map((l) => l.slice(sha.length + 1)) : [];
    const broken = brokenRefs(paths, cites);
    if (broken.length) bad.push({ sha, count: broken.length, broken: broken.slice(0, 20) });
  }
  return { first_parent_commits: line.length, commits_with_broken_refs: bad.length, first: bad.slice(0, 20) };
}
/**
 * `a-xref`: --n pairs and --n plain revisions, created --burst at a time.
 * Pair k: one MR deletes document 3k-2, the other makes document 3k-1 cite
 * it. Each is green alone (`forge/doc-lint` on its head); together they leave
 * a broken reference, whichever lands second. Document 3k gets a plain revision.
 */
async function scenarioXref() {
  const n = num('n', 20);
  const files = { '.forge/doctypes/lt.procedure.yml': D.DOCTYPE };
  for (let i = 1; i <= 3 * n; i++) files[D.docPath(i)] = D.doc(i);
  const p = await seedProject(`docs-ax-${TAG}`, files, ['forge/doc-lint']);
  console.log(`project ${GROUP}/${p.name} id ${p.id}: ${3 * n} documents, ${n} pairs and ${n} plain revisions, main requires forge/doc-lint`);
  const branches = [];
  for (let k = 1; k <= n; k++) {
    const [gone, citing, plain] = [3 * k - 2, 3 * k - 1, 3 * k];
    branches.push({ name: `x/delete-${k}`, kind: 'delete', k, doc: gone, title: `${D.docId(gone)} 廃止`, files: { [D.docPath(gone)]: null } });
    branches.push({ name: `x/cite-${k}`, kind: 'cite', k, doc: citing, title: `${D.docId(citing)} 改訂（参照の追加）`,
      files: { [D.docPath(citing)]: D.doc(citing, { purpose: `この手順書 ${D.docId(citing)} は [[${D.docId(gone)}]] の点検の後に行う（改訂 ${TAG}）。` }) } });
    branches.push({ name: `x/plain-${k}`, kind: 'plain', k, doc: plain, title: `${D.docId(plain)} 改訂`,
      files: { [D.docPath(plain)]: D.doc(plain, { edits: { 3: `手順 3: 改訂 ${TAG}。設備 ${plain} の部位 3 を測定器で確かめる。` } }) } });
  }
  await makeBranches(p, branches);
  const t0 = Date.now();
  // a person asks for every MR too: which MR of a pair comes first is left to the queue
  const recs = await burstCreate(p, branches, num('burst', branches.length), { enqueueByPerson: true });
  for (const r of recs) { const b = branches.find((x) => x.name === r.branch); Object.assign(r, { kind: b.kind, k: b.k, doc: b.doc }); }
  const settle = await waitSettled(p, recs.filter((r) => r.iid).map((r) => r.iid), { timeoutMs: num('timeout', 1800) * 1000, label: 'a-xref' });
  const facts = await queueFacts(p);
  const merged = mergedIids(facts);
  // the proof: no first-parent commit of main has a broken reference
  const xref = await brokenRefsOnMain(p);
  // every merged MR's change is on main (a deletion: the document is gone)
  const onMain = new Set((await L.gitOk(p.dir, ['ls-tree', '-r', '--name-only', 'origin/main', '--', 'docs/procedures'])).stdout.trim().split('\n'));
  const lost = [];
  for (const r of recs) {
    if (!merged.has(r.iid)) continue;
    const file = D.docPath(r.doc);
    if (r.kind === 'delete') { if (onMain.has(file)) lost.push(r.iid); continue; }
    const body = onMain.has(file) ? (await L.gitOk(p.dir, ['show', `origin/main:${file}`])).stdout : '';
    if (!body.includes(`改訂 ${TAG}`)) lost.push(r.iid);
  }
  const integrity = await mainIntegrity(p, { docLint: true });
  const lintRemovals = facts.entries.filter((e) => e.error_code === LINT_REMOVAL.code);
  const v = verdict({ settle, integrity, recs, offPlan: offPlanXref(recs, merged, facts.entries), lost: lost.length,
    extra: xref.commits_with_broken_refs ? [`main: ${xref.commits_with_broken_refs} first-parent commit(s) with a broken reference (the train lint did not stop them)`] : [] });
  const result = { scenario: 'a-xref', tag: TAG, verdict: v, project: `${GROUP}/${p.name}`, project_id: p.id, pairs: n, started: iso(t0), settle,
    summary: summarize(recs, facts), broken_refs_on_main: xref,
    lint_removals: { count: lintRemovals.length, by_reason: lintRemovals.reduce((a, e) => ((a[e.reason] = (a[e.reason] ?? 0) + 1), a), {}), iids: lintRemovals.map((e) => e.iid) },
    merged_by_kind: recs.reduce((a, r) => { const kk = `${r.kind}:${merged.has(r.iid) ? 'merged' : 'not_merged'}`; a[kk] = (a[kk] ?? 0) + 1; return a; }, {}),
    merged_revisions_missing_on_main: lost, integrity, facts };
  L.save(`a-xref-${TAG}.json`, result);
  console.log(JSON.stringify({ ...result.summary, merged_by_kind: result.merged_by_kind, lint_removals: result.lint_removals.count, broken_refs_on_main: xref.commits_with_broken_refs }, null, 1));
  console.log('integrity of main:', integrityLine(integrity));
  finish(v);
}

// ---------------------------------------------------------------- B
async function scenarioB() {
  const m = num('n', 30);
  const steps = m + 10;
  const files = { '.forge/doctypes/lt.procedure.yml': D.DOCTYPE, [D.docPath(1)]: D.doc(1, { steps }) };
  for (let i = 2; i <= 5; i++) files[D.docPath(i)] = D.doc(i);
  const p = await seedProject(`docs-b-${TAG}`, files, ['forge/doc-lint']);
  console.log(`project ${GROUP}/${p.name} id ${p.id}: ${m} MRs on ${D.docPath(1)} (${steps} steps)`);
  // even k: overlapping (all rewrite step 1); odd k: non-overlapping (each its own step k+5)
  const branches = [];
  for (let k = 1; k <= m; k++) {
    const overlap = k % 2 === 0;
    const edits = overlap ? { 1: `手順 1: 版 ${k} の書き方（重なる改訂）。` } : { [k + 5]: `手順 ${k + 5}: 版 ${k} の改訂（重ならない）。` };
    branches.push({ name: `b/${overlap ? 'overlap' : 'separate'}-${k}`, kind: overlap ? 'overlap' : 'separate', k,
      files: { [D.docPath(1)]: D.doc(1, { steps, edits }) } });
  }
  await makeBranches(p, branches);
  const recs = await burstCreate(p, branches, m, { enqueueByPerson: false });
  for (const r of recs) r.kind = branches.find((b) => b.name === r.branch).kind;
  const settle = await waitSettled(p, recs.filter((r) => r.iid).map((r) => r.iid), { timeoutMs: num('timeout', 900) * 1000, label: 'B' });
  const facts = await queueFacts(p);
  // what a person sees on each MR that did not merge
  const views = [];
  for (const r of recs) {
    const mr = facts.mrs.find((x) => x.iid === r.iid);
    if (mr?.merged_at) continue;
    const st = await L.api('GET', `/api/v1/projects/${p.id}/merge-requests/${r.iid}/status`);
    const am = await L.api('GET', `/api/v1/projects/${p.id}/merge-requests/${r.iid}/auto-merge`);
    const mg = await L.api('GET', `/api/v1/projects/${p.id}/merge-requests/${r.iid}/mergeability`);
    views.push({ iid: r.iid, kind: r.kind, reasons: st.body?.reasons, mergeability: mg.body?.status, auto_merge: am.body?.auto_merge, entries: facts.entries.filter((e) => e.iid === r.iid) });
  }
  // integrity of main: every merged MR's edit present, the document lints
  const text = await gitShow(p, 'main', D.docPath(1));
  const lost = [];
  for (const r of recs) {
    const mr = facts.mrs.find((x) => x.iid === r.iid);
    if (!mr?.merged_at) continue;
    const b = branches.find((x) => x.name === r.branch);
    if (!text.includes(`版 ${b.k} `)) lost.push(r.iid);
  }
  const mainSha = (await L.gitOk(p.dir, ['rev-parse', 'origin/main'])).stdout.trim();
  const mainChecks = await sql(`select name, conclusion from forge.check_runs where project_id=${p.id} and head_sha='${mainSha}'`);
  const conflictMarkers = CONFLICT_MARKER.test(text);
  // the document as the portal parses it on main (every step block still there)
  const dv = await L.api('GET', `/api/v1/projects/${p.id}/documents/${D.docId(1)}`);
  const docOnMain = { status: dv.status, step_ids: new Set((JSON.stringify(dv.body).match(/"id":"S-\d+"/g) ?? [])).size, expected_steps: steps };
  const integrity = await mainIntegrity(p, { docLint: true });
  // the overlapping MRs but one are meant to stop (offPlanB); anything else that stopped is not
  const v = verdict({ settle, integrity, recs, offPlan: offPlanB(recs, mergedIids(facts)), lost: lost.length,
    extra: conflictMarkers ? ['the document on main carries a conflict marker'] : [] });
  const result = { scenario: 'B', tag: TAG, verdict: v, project: `${GROUP}/${p.name}`, project_id: p.id, m, settle, integrity,
    summary: summarize(recs, facts),
    merged_by_kind: recs.reduce((a, r) => { const mr = facts.mrs.find((x) => x.iid === r.iid); const k = `${r.kind}:${mr?.merged_at ? 'merged' : mr?.state}`; a[k] = (a[k] ?? 0) + 1; return a; }, {}),
    lost_edits: lost, conflict_markers_on_main: conflictMarkers, doc_on_main: docOnMain, main_sha: mainSha, main_checks: mainChecks, not_merged_views: views, facts };
  L.save(`b-${TAG}.json`, result);
  console.log(JSON.stringify({ ...result.summary, merged_by_kind: result.merged_by_kind, lost: lost.length, conflictMarkers }, null, 1));
  console.log('integrity of main:', integrityLine(integrity));
  finish(v);
}

// ---------------------------------------------------------------- C
const CI_YML = (digest) => `# synthetic — tiny CI for the merge load test
version: 1
stages: [test]
jobs:
  test:
    stage: test
    image: registry:5000/ci/alpine@${digest}
    timeout: 5m
    tags: [linux, m4]
    script:
      - sh tests/check.sh
`;
// every mods/*.env: NAME=, PORT=, optional REQUIRES=<module>. Green when names
// and ports are unique and every REQUIRES names an existing module.
const CHECK_SH = `#!/bin/sh
# synthetic — semantic consistency of mods/*.env (load test)
set -eu
fail=0
names=$(for f in mods/*.env; do sed -n 's/^NAME=//p' "$f"; done | sort)
ports=$(for f in mods/*.env; do sed -n 's/^PORT=//p' "$f"; done | sort)
d=$(printf '%s\\n' "$names" | uniq -d); [ -z "$d" ] || { echo "duplicate NAME: $d"; fail=1; }
d=$(printf '%s\\n' "$ports" | uniq -d); [ -z "$d" ] || { echo "duplicate PORT: $d"; fail=1; }
for f in mods/*.env; do
  r=$(sed -n 's/^REQUIRES=//p' "$f")
  [ -z "$r" ] || [ -f "mods/$r.env" ] || { echo "$f requires missing module $r"; fail=1; }
done
[ "$fail" = 0 ] && echo "checks ok: $(ls mods | wc -l) modules"
exit $fail
`;
const modFile = (name, port, requires) => `NAME=${name}\nPORT=${port}\n${requires ? `REQUIRES=${requires}\n` : ''}`;
async function scenarioC() {
  const k = num('n', 40);
  const digest = opt('digest', 'sha256:26ba97bef0be0407a46e30df34e515e86116fa7b59efbe0265446ec43ef68138');
  const files = { '.forge-ci.yml': CI_YML(digest), 'tests/check.sh': CHECK_SH,
    'mods/base-a.env': modFile('base-a', 8001), 'mods/base-b.env': modFile('base-b', 8002), 'mods/base-c.env': modFile('base-c', 8003) };
  const p = await seedProject(`sys-c-${TAG}`, files, ['ci/pipeline']);
  console.log(`project ${GROUP}/${p.name} id ${p.id}: ${k} MRs, main requires ci/pipeline`);
  // semantic pairs (each MR green alone): [first, breaker, how]
  const pairs = [[5, 8, 'dup_port'], [17, 19, 'removed_dependency'], [30, 33, 'dup_name']];
  const breaker = new Map(pairs.map(([a, b, how]) => [b, { with: a, how }]));
  const branches = [];
  for (let i = 1; i <= k; i++) {
    let f = { [`mods/m${i}.env`]: modFile(`m${i}`, 9000 + i) };
    if (i === 5) f = { 'mods/m5.env': modFile('m5', 9105) };
    if (i === 8) f = { 'mods/m8.env': modFile('m8', 9105) };
    if (i === 17) f = { 'mods/m17.env': modFile('m17', 9017), 'mods/base-b.env': null };
    if (i === 19) f = { 'mods/m19.env': modFile('m19', 9019, 'base-b') };
    if (i === 30) f = { 'mods/m30.env': modFile('alpha', 9030) };
    if (i === 33) f = { 'mods/m33.env': modFile('alpha', 9033) };
    branches.push({ name: `c/mod-${i}`, i, files: f, breaker: breaker.get(i) ?? null });
  }
  await makeBranches(p, branches);
  const recs = await burstCreate(p, branches, num('burst', k), { enqueueByPerson: true });
  for (const r of recs) { const b = branches.find((x) => x.name === r.branch); r.i = b.i; r.breaker = b.breaker; }
  const settle = await waitSettled(p, recs.filter((r) => r.iid).map((r) => r.iid), { timeoutMs: num('timeout', 5400) * 1000, label: 'C' });
  const facts = await queueFacts(p);
  // main never red: every commit on main's first-parent line has a green ci/pipeline (push pipeline)
  await L.gitOk(p.dir, ['fetch', '-q', L.repoUrl(GROUP, p.name), '+refs/heads/main:refs/remotes/origin/main']);
  const line = (await L.gitOk(p.dir, ['rev-list', '--first-parent', 'origin/main'])).stdout.trim().split('\n');
  const pipes = await sql(`select sha, source, status, created_at, finished_at, merge_request_id from forge.ci_pipelines where project_id=${p.id} order by id`);
  const pushOnMain = new Map(pipes.filter((x) => x[1] === 'push').map((x) => [x[0], x[2]]));
  const mainRed = line.filter((s) => pushOnMain.has(s) && pushOnMain.get(s) !== 'succeeded');
  const mainUnknown = line.filter((s) => !pushOnMain.has(s));
  // re-run the check on main's tree locally (the definitive "main is green")
  await L.gitOk(p.dir, ['checkout', '-q', '--detach', 'origin/main']);
  const local = await L.git(p.dir, ['-c', 'core.autocrlf=false', 'status', '--short']);
  let localCheck;
  try { localCheck = (await pexec('sh', ['tests/check.sh'], { cwd: p.dir, timeout: 60000 })).stdout.trim(); } catch (e) {
    // an exit code: the check ran and main's tree fails it. Otherwise it did not run (no `sh`, the timeout): said, not judged
    localCheck = typeof e.code === 'number' ? `FAILED: ${e.stdout}` : `NOT RUN: ${String(e.message).slice(0, 200)}`;
  }
  await L.gitOk(p.dir, ['checkout', '-q', 'main']);
  const jobs = await sql(`select j.status, count(*), round(avg(extract(epoch from j.finished_at-j.started_at))::numeric,1), round(avg(extract(epoch from j.started_at-j.created_at))::numeric,1) from forge.ci_jobs j where j.project_id=${p.id} group by 1`);
  const bySource = pipes.reduce((a, x) => { const kk = `${x[1]}:${x[2]}`; a[kk] = (a[kk] ?? 0) + 1; return a; }, {});
  const breakers = recs.filter((r) => r.breaker).map((r) => ({ iid: r.iid, i: r.i, ...r.breaker,
    merged: !!facts.mrs.find((x) => x.iid === r.iid)?.merged_at, entries: facts.entries.filter((e) => e.iid === r.iid) }));
  const partners = pairs.map(([a]) => ({ i: a, iid: recs.find((r) => r.i === a)?.iid, merged: !!facts.mrs.find((x) => x.iid === recs.find((r) => r.i === a)?.iid)?.merged_at }));
  const integrity = await mainIntegrity(p, { docLint: false });
  // one MR of each planted pair is meant to be dropped (offPlanC); anything else that stopped is not
  const v = verdict({ settle, integrity, recs, offPlan: offPlanC(recs, mergedIids(facts), pairs),
    extra: localCheck.startsWith('FAILED') ? [`tests/check.sh fails on main's tree: ${localCheck.slice(0, 200)}`] : [] });
  const result = { scenario: 'C', tag: TAG, verdict: v, project: `${GROUP}/${p.name}`, project_id: p.id, k, settle, integrity,
    summary: summarize(recs, facts), pipelines_by_source_status: bySource,
    ci_jobs: jobs.map(([s, c, run, wait]) => ({ status: s, count: +c, avg_run_s: +run, avg_wait_s: +wait })),
    main_first_parent_commits: line.length, main_red_push_pipelines: mainRed, main_without_push_pipeline: mainUnknown.length,
    local_check_on_main: localCheck, breakers, partners, facts, pipelines: pipes };
  L.save(`c-${TAG}.json`, result);
  console.log(JSON.stringify({ ...result.summary, bySource, jobs: result.ci_jobs, mainRed: mainRed.length, localCheck, breakers: breakers.map((b) => [b.i, b.merged, b.entries.map((e) => `${e.state}/${e.error_code}/${e.reason}`)]) }, null, 1));
  console.log('integrity of main:', integrityLine(integrity));
  finish(v);
}

// ---------------------------------------------------------------- D
async function scenarioD() {
  const n = num('n', 20);
  const httpsBase = opt('https-base', process.env.GS_LT_HTTPS_BASE);
  const caFile = opt('ca-file', process.env.GS_LT_CA_FILE);
  const from = opt('from', null); // an existing local clone to seed history from (e.g. scenario A's)
  const proj = await L.ensureProject(GROUP, `push-d-${TAG}`);
  const name = `push-d-${TAG}`;
  const dir = path.join(WORK, name);
  fs.mkdirSync(dir, { recursive: true });
  if (from) await L.gitOk(WORK, ['clone', '-q', '--no-local', from, dir]);
  else {
    await L.gitOk(dir, ['init', '-q', '-b', 'main']);
    fs.writeFileSync(path.join(dir, 'README.md'), '# synthetic push test\n');
    await L.gitOk(dir, ['add', 'README.md']);
    await L.gitOk(dir, ['commit', '-qm', 'seed']);
  }
  await L.gitOk(dir, ['push', '-q', L.repoUrl(GROUP, name), 'HEAD:refs/heads/main'], { timeoutMs: 600000 });
  // n independent clones, each with 5 commits of ~100 KB synthetic data
  const clones = [];
  for (let i = 1; i <= n; i++) {
    const c = path.join(WORK, `${name}-c${i}`);
    await L.gitOk(WORK, ['clone', '-q', '--no-local', dir, c]);
    await L.gitOk(c, ['checkout', '-q', '-b', `d/push-${i}`]);
    for (let j = 1; j <= 5; j++) {
      const lines = [];
      for (let x = 0; x < 2000; x++) lines.push(`push ${i} commit ${j} line ${x} ${((i * 7919 + j * 104729 + x * 31) % 1000003).toString(36)}`);
      fs.mkdirSync(path.join(c, 'data'), { recursive: true });
      fs.writeFileSync(path.join(c, 'data', `p${i}-c${j}.txt`), lines.join('\n') + '\n');
      await L.gitOk(c, ['add', 'data']);
      await L.gitOk(c, ['commit', '-qm', `push ${i} commit ${j}`]);
    }
    clones.push({ i, dir: c });
  }
  const t0 = Date.now();
  const pushes = await Promise.all(clones.map(async (c) => {
    const r = await L.git(c.dir, ['push', '-q', L.repoUrl(GROUP, name), `d/push-${c.i}`], { timeoutMs: 600000 });
    return { i: c.i, ok: r.ok, ms: Math.round(r.ms), stderr: r.ok ? '' : r.stderr.slice(0, 300) };
  }));
  const wall = Date.now() - t0;
  const refs = (await L.gitOk(dir, ['ls-remote', L.repoUrl(GROUP, name)])).stdout.trim().split('\n').length;
  const cloneTimes = [];
  const tryClone = async (label, url, extra) => {
    const target = path.join(WORK, `${name}-clone-${label}-${cloneTimes.length}`);
    const r = await L.git(WORK, [...extra, 'clone', '-q', '--mirror', url, target], { timeoutMs: 600000 });
    let size = null;
    if (r.ok) size = (await L.gitOk(target, ['count-objects', '-vH'])).stdout.match(/size-pack: (.*)/)?.[1];
    cloneTimes.push({ label, ok: r.ok, ms: Math.round(r.ms), size_pack: size, stderr: r.ok ? '' : r.stderr.slice(0, 300) });
  };
  for (let k = 0; k < 3; k++) await tryClone('http-loopback', L.repoUrl(GROUP, name), []);
  if (httpsBase) {
    for (let k = 0; k < 3; k++) await tryClone('https-proxy', `${httpsBase}/${GROUP}/${name}.git`, caFile ? ['-c', 'http.sslBackend=openssl', '-c', `http.sslCAInfo=${caFile}`] : []);
  }
  const result = { scenario: 'D', tag: TAG, project: `${GROUP}/${name}`, project_id: proj.id, n, push_wall_ms: wall,
    push_ms: L.stats(pushes.map((p) => p.ms)), push_failures: pushes.filter((p) => !p.ok), remote_refs: refs, clones: cloneTimes, pushes };
  L.save(`d-${TAG}.json`, result);
  console.log(JSON.stringify({ ...result, pushes: undefined }, null, 1));
}

// ---------------------------------------------------------------- E (soak)
const STEPS = 10; // `::: step` blocks per fixture document
// this many creates, or samples, that fail in a row end the soak (exit 1)
const SOAK_CREATE_FAILURES_END = 20;
const SOAK_SAMPLE_FAILURES_END = 10;
/** MR number k (1-based) revises one step of one document; (document, step) repeats after docs × STEPS MRs. */
const soakChange = (k, docs) => ({ k, doc: ((k - 1) % docs) + 1, step: (Math.floor((k - 1) / docs) % STEPS) + 1, branch: `e/m-${k}`, mark: `rev ${TAG} #${k}` });
/** `text` (a fixture document as it is on main) with the change's step rewritten. */
function reviseStep(text, c) {
  const lines = text.split('\n');
  const at = lines.indexOf(`::: step S-${c.step} role=作業者`);
  if (at < 0) throw new Error(`${D.docPath(c.doc)}: step S-${c.step} not found on main`);
  lines[at + 1] = `手順 ${c.step}: 改訂 (${c.mark})。設備 ${c.doc} の部位 ${c.step} を測定器で確かめる。`;
  return lines.join('\n');
}
/** The group's branches: one commit each on top of main as it is now, pushed in one push per 50. */
async function pushSoakBranches(p, group) {
  const url = L.repoUrl(GROUP, p.name);
  await L.gitOk(p.dir, ['fetch', '-q', url, '+refs/heads/main:refs/remotes/origin/main'], { timeoutMs: 120000 });
  const base = (await L.localGitOk(p.dir, ['rev-parse', '--verify', 'refs/remotes/origin/main'])).stdout.trim();
  const texts = await L.readBlobs(p.dir, group.map((c) => `${base}:${D.docPath(c.doc)}`));
  // local refs are reused by every group (refs/lt/slot-N): the clone does not grow by a ref per MR
  await L.commitOnTop(p.dir, base, group.map((c, j) => ({
    ref: `refs/lt/slot-${j}`, path: D.docPath(c.doc), text: reviseStep(texts[j], c), message: `${D.docId(c.doc)} S-${c.step} (${c.mark})`,
  })));
  for (let i = 0; i < group.length; i += 50) {
    const refs = group.slice(i, i + 50).map((c, j) => `refs/lt/slot-${i + j}:refs/heads/${c.branch}`);
    await L.gitOk(p.dir, ['push', '-q', url, ...refs], { timeoutMs: 300000 });
  }
}
/** Bytes and files under `dir` (a bare repository on this host). */
async function dirBytes(dir) {
  let bytes = 0;
  let files = 0;
  const stack = [dir];
  while (stack.length) {
    const d = stack.pop();
    let entries;
    try { entries = await fs.promises.readdir(d, { withFileTypes: true }); } catch (e) {
      if (d === dir) throw e;
      continue; // a directory git removed meanwhile (a temporary pack, gc)
    }
    for (const e of entries) {
      const full = path.join(d, e.name);
      if (e.isDirectory()) stack.push(full);
      else if (e.isFile()) {
        try { bytes += (await fs.promises.stat(full)).size; files++; } catch { /* a file git removed meanwhile */ }
      }
    }
  }
  return { bytes, files };
}
/** Where forge keeps repository `id` under its storage root (forge-git `StorageRoot::path_for`). */
const repoDirOf = (root, id) => path.join(root, 'repos', createHash('sha256').update(String(id)).digest('hex').slice(0, 2), `${id}.git`);
/** One read of the backlog: merge requests, live queue entries, jobs, the database's size. */
async function soakCounts(p) {
  const rows = await sql(`select 'mr', state, count(*)::text from forge.merge_requests where project_id=${p.id} group by 2
    union all select 'entry', state, count(*)::text from forge.merge_queue_entries where project_id=${p.id} and state in ('queued','testing') group by 2
    union all select 'job', state, count(*)::text from forge.jobs where state <> 'succeeded' group by 2
    union all select 'ready', 'count', count(*)::text from forge.jobs where state='queued' and run_after <= now()
    union all select 'ready', 'oldest_s', coalesce(round(extract(epoch from now() - min(run_after)))::text, '0') from forge.jobs where state='queued' and run_after <= now()
    union all select 'db', 'bytes', pg_database_size(current_database())::text`);
  const of = (kind) => Object.fromEntries(rows.filter((r) => r[0] === kind).map((r) => [r[1], Number(r[2])]));
  const mr = of('mr');
  const entries = of('entry');
  const total = Object.values(mr).reduce((a, x) => a + x, 0);
  return {
    mrs: total, merged: mr.merged ?? 0, not_merged: total - (mr.merged ?? 0),
    queue_depth: (entries.queued ?? 0) + (entries.testing ?? 0), queue: entries,
    jobs: of('job'), jobs_ready: of('ready').count ?? 0, jobs_oldest_ready_s: of('ready').oldest_s ?? 0,
    db_bytes: of('db').bytes ?? null,
  };
}
const topTables = async () => Object.fromEntries((await sql(`select c.relname, pg_total_relation_size(c.oid) from pg_class c join pg_namespace n on n.oid=c.relnamespace
  where n.nspname='forge' and c.relkind='r' order by 2 desc limit 15`)).map(([name, bytes]) => [name, Number(bytes)]));
/** Least-squares slope of y over x (per unit of x); null with fewer than 2 points. */
function slope(points) {
  if (points.length < 2) return null;
  const mx = mean(points.map(([x]) => x));
  const my = mean(points.map(([, y]) => y));
  const den = points.reduce((a, [x]) => a + (x - mx) ** 2, 0);
  return den === 0 ? null : points.reduce((a, [x, y]) => a + (x - mx) * (y - my), 0) / den;
}
/**
 * Does the backlog grow while MRs arrive? Over the samples of the arrival
 * phase: the mean of the last quarter against the mean of the second quarter
 * (the first is the warm-up). `grows` = late > 1.5 × early + one minute of
 * arrivals; null with fewer than 8 samples (a rehearsal checks the code, not this).
 */
function backlogTrend(samples, rate) {
  const trend = (key) => {
    const pts = samples.filter((x) => typeof x[key] === 'number').map((x) => [x.min, x[key]]);
    const q = Math.floor(pts.length / 4);
    const ys = (rows) => rows.map(([, y]) => y);
    const early = q ? mean(ys(pts.slice(q, 2 * q))) : null;
    const late = q ? mean(ys(pts.slice(3 * q))) : null;
    const per = slope(pts);
    return round1({ samples: pts.length, early_mean: early, late_mean: late, max: pts.length ? Math.max(...ys(pts)) : null,
      slope_per_min: per === null ? null : Math.round(per * 100) / 100,
      grows: pts.length >= 8 ? late > 1.5 * early + rate : null });
  };
  const out = { not_merged: trend('not_merged'), queue_depth: trend('queue_depth'), jobs_ready: trend('jobs_ready') };
  const verdicts = Object.values(out).map((t) => t.grows);
  out.grows = verdicts.includes(null) ? null : verdicts.includes(true);
  out.rule = 'arrival phase: mean of the last quarter > 1.5 x mean of the second quarter + one minute of arrivals (--rate)';
  return out;
}
async function scenarioE() {
  // every value was checked at startup (NUMERIC; --docs against --rate and --burst)
  const count = num('count', 10000);
  const rate = num('rate', 21); // MRs per minute, sustained
  const burstRate = num('burst', 100); // MRs in a burst minute (0: no bursts)
  const burstEvery = num('burst-every', 30); // every N-th minute is a burst minute
  const durationMin = num('duration', 0); // stop creating after this many minutes (0: until --count)
  const docs = num('docs', 500);
  const sampleEveryMs = num('sample-every', 30) * 1000;
  const repoSizeEveryMs = num('repo-size-every', 300) * 1000;
  const drainTimeoutMs = num('timeout', 3600) * 1000;
  const files = { '.forge/doctypes/lt.procedure.yml': D.DOCTYPE };
  for (let i = 1; i <= docs; i++) files[D.docPath(i)] = D.doc(i);
  const p = await seedProject(`docs-e-${TAG}`, files, ['forge/doc-lint']);
  console.log(`project ${GROUP}/${p.name} id ${p.id}: ${docs} documents; ${count} MRs at ${rate}/min${burstRate ? `, ${burstRate} in every ${burstEvery}th minute` : ''}`);
  const repoId = Number((await sql(`select id from forge.repositories where project_id=${p.id}`))[0]?.[0]);
  const repoDir = STORAGE_ROOT && Number.isInteger(repoId) ? repoDirOf(STORAGE_ROOT, repoId) : null;
  if (!repoDir) console.log('GS_LT_STORAGE_ROOT is not set: the repository size is not sampled');
  const tablesStart = await topTables();

  const t0 = Date.now();
  const state = { done: false, stop: null, created: 0, failed: 0, skipped: 0, errors: 0 };
  // why the run cannot go on (the API, git or the database is gone): set once, it ends every loop
  let failure = null;
  const fail = (why) => {
    if (failure !== null) return;
    failure = why;
    console.error(`failed: ${why}`);
  };
  const samples = [];
  const sampleOnceE = async (withRepo) => {
    const x = { t: new Date().toISOString(), min: Math.round(((Date.now() - t0) / 60000) * 100) / 100, created: state.created, create_failed: state.failed,
      ...(await soakCounts(p)), free_gb: Math.round((os.freemem() / 2 ** 30) * 100) / 100 };
    if (withRepo && repoDir) Object.assign(x, await dirBytes(repoDir).then((r) => ({ repo_bytes: r.bytes, repo_files: r.files })));
    samples.push(x);
    L.log(`e-${TAG}-samples.jsonl`, x);
    return x;
  };
  // the STOP file (written by the sampler of run-all.sh, by a person, or here on the memory floor)
  const stopNow = () => {
    if (!state.stop && stopRequested()) state.stop = fs.readFileSync(STOP_FILE, 'utf8').trim() || 'STOP file';
    return state.stop !== null;
  };
  const ended = () => stopNow() || failure !== null;
  const pause = async (ms) => { for (const end = Date.now() + ms; Date.now() < end && !state.done && !ended();) await L.sleep(Math.min(1000, Math.max(1, end - Date.now()))); };
  const sampling = (async () => {
    let lastRepo = 0;
    let sampleFailures = 0;
    while (!state.done && !ended()) {
      try {
        const withRepo = Date.now() - lastRepo >= repoSizeEveryMs;
        const x = await sampleOnceE(withRepo);
        if (withRepo) lastRepo = Date.now();
        sampleFailures = 0;
        if (x.free_gb < MIN_FREE_GB && !stopRequested()) {
          fs.writeFileSync(STOP_FILE, `free memory ${x.free_gb} GB < ${MIN_FREE_GB} GB at ${x.t}\n`);
          console.error('free memory below the floor: STOP written');
        }
      } catch (e) {
        L.log(`e-${TAG}-samples.jsonl`, { sample_error: String(e.message).slice(0, 300) });
        // a soak without samples measures nothing, and nobody watches the memory floor
        if (++sampleFailures >= SOAK_SAMPLE_FAILURES_END) fail(`the backlog could not be sampled ${sampleFailures} times in a row: ${String(e.message).slice(0, 200)}`);
      }
      await pause(sampleEveryMs);
    }
  })();

  // ---- arrivals: minute m creates --rate MRs (a burst minute: --burst), evenly spaced
  const recs = [];
  const pending = [];
  let scheduled = 0;
  let maxLagS = 0;
  let prepFailures = 0;
  let createFailures = 0; // in a row, in the order the answers come back
  const createOne = async (c) => {
    let created = false;
    try {
      const rec = await createMr(p, { name: c.branch, title: `${D.docId(c.doc)} S-${c.step} 改訂` });
      Object.assign(rec, { k: c.k, doc: c.doc, step: c.step });
      recs.push(rec);
      created = rec.iid !== null;
      if (created) state.created++; else state.failed++;
      L.log(`e-${TAG}.jsonl`, rec);
    } catch (e) {
      // never an unhandled rejection: the failure is counted
      state.errors++;
      console.error(`MR ${c.k}: ${String(e.message).slice(0, 200)}`);
    }
    // an API that takes no merge request ends the soak: it does not run out the clock
    createFailures = created ? 0 : createFailures + 1;
    if (createFailures >= SOAK_CREATE_FAILURES_END) fail(`${createFailures} merge requests in a row were not created`);
  };
  for (let m = 0; scheduled < count && (!durationMin || m < durationMin) && !ended(); m++) {
    const startAt = t0 + m * 60000;
    if (startAt > Date.now()) await pause(startAt - Date.now());
    else maxLagS = Math.max(maxLagS, (Date.now() - startAt) / 1000);
    if (ended()) break;
    const burstMinute = burstRate > 0 && (m + 1) % burstEvery === 0;
    const size = Math.min(burstMinute ? burstRate : rate, count - scheduled);
    const group = Array.from({ length: size }, (_, j) => soakChange(scheduled + j + 1, docs));
    scheduled += size;
    try {
      await pushSoakBranches(p, group);
      prepFailures = 0;
    } catch (e) {
      // one failed fetch/push skips this minute's MRs; five in a row end the run
      state.skipped += size;
      prepFailures++;
      L.log(`e-${TAG}.jsonl`, { minute: m, skipped: size, prepare_error: String(e.message).slice(0, 300) });
      console.error(`minute ${m}: branches not pushed, ${size} MRs skipped (${prepFailures} in a row)`);
      if (prepFailures >= 5) { fail('the branches could not be pushed five minutes in a row'); break; }
      continue;
    }
    const gapMs = Math.max(0, startAt + 60000 - Date.now()) / size;
    for (let j = 0; j < size && !ended(); j++) {
      pending.push(createOne(group[j]));
      if (j + 1 < size) await L.sleep(gapMs);
    }
    if (m % 10 === 0 || burstMinute) console.log(`E minute ${m}${burstMinute ? ' (burst)' : ''}: scheduled ${scheduled}/${count}, created ${state.created}, failed ${state.failed}`);
  }
  await Promise.all(pending);
  const arrivalsEnd = Date.now();

  // ---- drain: until every created MR is merged, or nothing is live and nothing moved for 150 s
  let settle = { settled: false };
  let lastMerged = -1;
  let lastChange = Date.now();
  let readFailures = 0;
  while (!ended()) {
    let c;
    try {
      c = await soakCounts(p);
      readFailures = 0;
    } catch (e) {
      // the database did not answer: the result is still written from what is there
      if (++readFailures >= 10) { fail(`the backlog could not be read ten times in a row: ${String(e.message).slice(0, 200)}`); break; }
      await pause(5000);
      continue;
    }
    if (c.merged !== lastMerged) { lastMerged = c.merged; lastChange = Date.now(); }
    const waited = (Date.now() - arrivalsEnd) / 1000;
    if (c.merged >= state.created) { settle = { settled: true, waited_s: waited }; break; }
    if (c.queue_depth === 0 && Date.now() - lastChange > 150000) { settle = { settled: true, waited_s: waited, not_merged: state.created - c.merged }; break; }
    if (Date.now() - arrivalsEnd > drainTimeoutMs) { settle = { settled: false, waited_s: waited, not_merged: state.created - c.merged }; break; }
    await pause(5000);
  }
  state.done = true;
  await sampling;
  const last = await sampleOnceE(true).catch(() => samples.at(-1) ?? {});

  // ---- result (also after a STOP: what was measured so far)
  const facts = await queueFacts(p);
  const merged = mergedIids(facts);
  // after a STOP or a failure nothing is waited for: what is there is written
  const integrity = await mainIntegrity(p, { docLint: true, waitMs: ended() ? 0 : 300000 });
  // every merged revision is on main, unless a later merged MR revised the same step again
  const grep = await L.git(p.dir, ['grep', '-h', '-F', `rev ${TAG} #`, 'origin/main', '--', 'docs/procedures']);
  if (!grep.ok && grep.code !== 1) throw new Error(`git grep failed: ${grep.stderr.slice(0, 300)}`);
  const onMain = new Set([...grep.stdout.matchAll(/ #(\d+)\)/g)].map((x) => Number(x[1])));
  const latest = new Map();
  for (const r of recs) {
    if (!merged.has(r.iid)) continue;
    const key = `${r.doc}/${r.step}`;
    if (!latest.has(key) || latest.get(key) < r.k) latest.set(key, r.k);
  }
  const missing = [...latest.values()].filter((k) => !onMain.has(k)).sort((a, b) => a - b);
  const first = samples.find((x) => x.db_bytes) ?? last;
  const repoFirst = samples.find((x) => x.repo_bytes !== undefined);
  const perMerge = (a, b) => (merged.size ? Math.round((b - a) / merged.size) : null);
  const tablesEnd = await topTables();
  const arrivalSamples = samples.filter((x) => Date.parse(x.t) <= arrivalsEnd);
  // every created MR merges (a group revises each document once: nothing is planted to stop)
  const v = verdict({ settle, integrity, recs, createErrors: state.errors, skipped: state.skipped,
    offPlan: notMerged(recs, merged).map((x) => `${x} did not merge`), lost: missing.length, stopped: state.stop, failure });
  const result = {
    scenario: 'e', tag: TAG, verdict: v, project: `${GROUP}/${p.name}`, project_id: p.id,
    params: { count, rate, burst: burstRate, burst_every: burstEvery, duration_min: durationMin, docs, sample_every_s: sampleEveryMs / 1000 },
    started: iso(t0), arrivals_end: iso(arrivalsEnd), arrival_min: Math.round(((arrivalsEnd - t0) / 60000) * 10) / 10,
    stopped: state.stop, failure, scheduled, created: state.created, create_failed: state.failed, create_errors: state.errors, skipped: state.skipped,
    max_schedule_lag_s: Math.round(maxLagS * 10) / 10,
    arrival_rate_per_min: Math.round((state.created / ((arrivalsEnd - t0) / 60000)) * 10) / 10,
    settle, summary: summarize(recs, facts),
    backlog: backlogTrend(arrivalSamples, rate),
    growth: {
      db_bytes: { start: first.db_bytes, end: last.db_bytes, per_merge: perMerge(first.db_bytes, last.db_bytes) },
      repo_bytes: repoFirst ? { start: repoFirst.repo_bytes, end: last.repo_bytes, per_merge: perMerge(repoFirst.repo_bytes, last.repo_bytes), files_end: last.repo_files } : null,
      tables_bytes: Object.fromEntries(Object.entries(tablesEnd).map(([name, bytes]) => [name, { start: tablesStart[name] ?? null, end: bytes }])),
    },
    integrity, revisions_missing_on_main: missing, samples, facts,
  };
  L.save(`e-${TAG}.json`, result);
  console.log(JSON.stringify({ ...result, samples: undefined, facts: undefined, integrity: undefined }, null, 1));
  console.log('integrity of main:', integrityLine(integrity), 'missing on main:', missing.length);
  finish(v);
}

// ---------------------------------------------------------------- spawn-bench
/**
 * What one git process costs on this host (plan 18 §2.2 E1, E2): in a
 * throwaway bare repository under GS_LT_WORK, --n times `git rev-parse HEAD`
 * and --n times the trial merge forge runs (`git merge-tree --write-tree
 * --name-only -z <target> <head>`), one after the other. No server, no token.
 */
async function spawnBench() {
  const n = num('n', 100);
  const docs = num('docs', 200);
  const root = path.join(WORK, `spawn-bench-${TAG}-${process.pid}`);
  const work = path.join(root, 'work');
  const bare = path.join(root, 'bare.git');
  fs.mkdirSync(work, { recursive: true });
  try {
    const version = (await L.localGitOk(root, ['version'])).stdout.trim();
    // a document repository like scenario A's: main and a branch each revise another document
    await L.localGitOk(work, ['init', '-q', '-b', 'main']);
    const write = (i, edits) => {
      const full = path.join(work, D.docPath(i));
      fs.mkdirSync(path.dirname(full), { recursive: true });
      fs.writeFileSync(full, D.doc(i, { edits }));
    };
    for (let i = 1; i <= docs; i++) write(i, {});
    await L.localGitOk(work, ['add', '--', 'docs']);
    await L.localGitOk(work, ['commit', '-qm', 'seed: synthetic spawn-bench content']);
    await L.localGitOk(work, ['checkout', '-q', '-b', 'side']);
    write(1, { 3: '手順 3: 改訂（枝）。' });
    await L.localGitOk(work, ['commit', '-qam', 'side: revise document 1']);
    await L.localGitOk(work, ['checkout', '-q', 'main']);
    write(2, { 3: '手順 3: 改訂（main）。' });
    await L.localGitOk(work, ['commit', '-qam', 'main: revise document 2']);
    await L.localGitOk(root, ['clone', '-q', '--bare', work, bare]);
    const target = (await L.localGitOk(bare, ['rev-parse', 'refs/heads/main'])).stdout.trim();
    const head = (await L.localGitOk(bare, ['rev-parse', 'refs/heads/side'])).stdout.trim();
    const env = { ...process.env, GIT_TERMINAL_PROMPT: '0' };
    /** --n runs of `git <argv>` in the bare repository, one at a time: ms per call. */
    const timed = async (argv) => {
      const ms = [];
      for (let i = 0; i < n + 5; i++) {
        const t = performance.now();
        await pexec('git', argv, { cwd: bare, env, timeout: 60000 });
        if (i >= 5) ms.push(performance.now() - t); // the first five warm the file cache
      }
      const st = L.stats(ms);
      const r2 = (v) => Math.round(v * 100) / 100;
      return { n: st.n, p50_ms: r2(st.p50), p95_ms: r2(st.p95), max_ms: r2(st.max), mean_ms: r2(mean(ms)) };
    };
    const revParse = await timed(['rev-parse', 'HEAD']);
    const mergeTree = await timed(['merge-tree', '--write-tree', '--name-only', '-z', target, head]);
    const result = {
      scenario: 'spawn-bench', tag: TAG, at: new Date().toISOString(), platform: process.platform, arch: process.arch, os_release: os.release(),
      cpus: os.cpus().length, node: process.version, git: version, documents: docs,
      rev_parse_head: revParse, merge_tree_write_tree: mergeTree,
      // E1 = one spawn; E2 = the merge-tree work beyond the spawn
      e1_spawn_ms_p50: revParse.p50_ms, e2_merge_tree_work_ms_p50: Math.round((mergeTree.p50_ms - revParse.p50_ms) * 100) / 100,
    };
    L.save(`spawn-bench-${TAG}.json`, result);
    console.log(JSON.stringify(result, null, 1));
  } finally {
    if (!args.includes('--keep')) removeUnderWork(root);
  }
}

// ---------------------------------------------------------------- main
const run = {
  a: scenarioA, 'a-burst': () => scenarioA({ atOnce: true }), 'a-xref': scenarioXref, b: scenarioB, c: scenarioC, d: scenarioD, e: scenarioE,
  'spawn-bench': spawnBench, sample: sampler,
}[scenario];
if (!run) usage(USAGE);
// --preflight: the checks above passed; nothing is read, made or sent (run-all.sh, before it takes the lock)
if (args.includes('--preflight')) {
  console.log(`preflight ok: ${scenario} --tag ${TAG}`);
  process.exit(0);
}
abortIfStopped();
// everything but the local bench talks to the server: fail now without a token
if (scenario !== 'spawn-bench') {
  try { L.requireToken(); } catch (e) { usage(String(e.message)); }
}
fs.mkdirSync(WORK, { recursive: true });
await run();
