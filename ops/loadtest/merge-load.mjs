#!/usr/bin/env node
// synthetic — merge load test for the GitStation test environment
// (ops/loadtest/README.md). Scenarios:
//   a  documents: N MRs, each editing a different document, in bursts
//   b  documents with conflicts: M MRs on one document (overlapping and not)
//   c  system code: K MRs through the merge queue with a tiny CI; some break
//      the combined build only together with another MR (semantic conflict)
//   d  P parallel git pushes to different branches, then clone timing
//   sample  forge CPU/memory, PG connections, job backlog, free memory every 10 s
//
// Usage (Git Bash, from the worktree root):
//   GS_LT_TOKEN_FILE=<file with one fgp_ token> GS_LT_WORK=<scratch dir> \
//     node ops/loadtest/merge-load.mjs <a|b|c|d|sample> [--n 200] [--burst 50] [--tag run1]
// Results: $GS_LT_OUT/<scenario>-<tag>.json (+ .jsonl event logs).
import fs from 'node:fs';
import path from 'node:path';
import { execFile, spawn } from 'node:child_process';
import { promisify } from 'node:util';
import * as L from './lib.mjs';
import * as D from './docs-fixture.mjs';

const pexec = promisify(execFile);
const args = process.argv.slice(2);
const scenario = args[0];
const opt = (name, dflt) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : dflt;
};
const TAG = opt('tag', new Date().toISOString().slice(0, 16).replace(/[:T]/g, ''));
const GROUP = opt('group', 'loadtest');
const WORK = process.env.GS_LT_WORK ?? path.join(L.OUT, 'work');
const PG = process.env.GS_LT_PG_CONTAINER ?? 'gs-m3-test-gs-test-pg-1';
const STOP_FILE = path.join(L.OUT, 'STOP');
const MIN_FREE_GB = Number(process.env.GS_LT_MIN_FREE_GB ?? 3);
fs.mkdirSync(WORK, { recursive: true });

/** Read-only SQL against the test database (superuser inside the container). */
async function sql(q) {
  const r = await pexec('docker', ['exec', PG, 'psql', '-U', 'postgres', '-d', 'forge', '-AtF', '\t', '-c', q], {
    timeout: 30000, maxBuffer: 32 << 20,
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

// ---------------------------------------------------------------- sampler
async function sampleOnce(prev) {
  const ps = await pexec('powershell', ['-NoProfile', '-Command',
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
  const conn = await sql("select coalesce(state,'?'), count(*) from pg_stat_activity where datname='forge' group by 1");
  out.pg = Object.fromEntries(conn.map(([s, c]) => [s, Number(c)]));
  out.pg_total = conn.reduce((a, [, c]) => a + Number(c), 0);
  const jobs = await sql("select state, count(*) from forge.jobs where state not in ('succeeded') group by 1");
  out.jobs = Object.fromEntries(jobs.map(([s, c]) => [s, Number(c)]));
  const ci = await sql("select status, count(*) from forge.ci_jobs where status in ('pending','running') group by 1");
  out.ci_jobs = Object.fromEntries(ci.map(([s, c]) => [s, Number(c)]));
  return out;
}
async function sampler() {
  const file = opt('file', `samples-${TAG}.jsonl`);
  let prev = null;
  for (;;) {
    try {
      const s = await sampleOnce(prev);
      fs.appendFileSync(path.join(L.OUT, file), JSON.stringify(s) + '\n');
      if (s.free_gb !== undefined && s.free_gb < MIN_FREE_GB && !stopRequested()) {
        fs.writeFileSync(STOP_FILE, `free memory ${s.free_gb.toFixed(2)} GB < ${MIN_FREE_GB} GB at ${s.t}\n`);
        console.error('free memory below the floor: STOP written');
      }
      prev = s;
    } catch (e) {
      fs.appendFileSync(path.join(L.OUT, file), JSON.stringify({ t: new Date().toISOString(), sample_error: String(e.message).slice(0, 300) }) + '\n');
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
const RETRY_BUSY = Number(opt('retry-busy', 0));
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
  const round = (o) => Object.fromEntries(Object.entries(o).map(([k, v]) => [k, typeof v === 'number' ? Math.round(v * 10) / 10 : v]));
  return {
    mrs: recs.length,
    merged: mergedTimes.length,
    enqueue_to_merged_s: round(L.stats(enqToMerged)),
    create_to_merged_s: round(L.stats(createToMerged)),
    create_to_enqueue_s: round(L.stats([...firstEntry.values()].filter((e) => byIid.has(e.iid)).map((e) => secs(byIid.get(e.iid).t_create, e.created)))),
    enqueued_by_person: facts.entries.filter((e) => byIid.has(e.iid) && e.credential_kind !== 'auto_merge' && e.enqueued_by === 1).length,
    span_s: lastMerged ? Math.round((lastMerged - firstCreate) / 100) / 10 : null,
    throughput_per_min: lastMerged ? Math.round((mergedTimes.length / ((lastMerged - firstCreate) / 60000)) * 10) / 10 : null,
    merged_per_minute: perMinute,
    create_ms: round(L.stats(recs.map((r) => r.create_ms))),
    enqueue_ms: round(L.stats(recs.filter((r) => r.enqueue_ms_all).flatMap((r) => r.enqueue_ms_all))),
    enqueue_final: recs.filter((r) => r.enqueue_attempts).reduce((a, r) => { const k = r.enqueue_error ?? 'ok'; a[k] = (a[k] ?? 0) + 1; return a; }, {}),
    batches: { count: facts.batches.length, by_size_state: batchSizes },
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
    if (i + burst < branches.length) await L.sleep(Number(opt('gap', 30)) * 1000);
  }
  return recs;
}
async function gitShow(p, ref, file) {
  await L.gitOk(p.dir, ['fetch', '-q', L.repoUrl(GROUP, p.name), `+refs/heads/main:refs/remotes/origin/main`]);
  return (await L.gitOk(p.dir, ['show', `origin/main:${file}`])).stdout;
}

// ---------------------------------------------------------------- A
async function scenarioA() {
  const n = Number(opt('n', 200));
  const burst = Number(opt('burst', 50));
  const personFrom = Number(opt('person-from', Math.floor(n / 2))); // MRs >= this index are enqueued by a person too
  const files = { '.forge/doctypes/lt.procedure.yml': D.DOCTYPE };
  for (let i = 1; i <= n; i++) files[D.docPath(i)] = D.doc(i);
  const p = await seedProject(`docs-a-${TAG}`, files, ['forge/doc-lint']);
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
    if (i + burst < n) await L.sleep(Number(opt('gap', 30)) * 1000);
  }
  const settle = await waitSettled(p, recs.filter((r) => r.iid).map((r) => r.iid), { timeoutMs: Number(opt('timeout', 3600)) * 1000, label: 'A' });
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
  const result = { scenario: 'A', tag: TAG, docs_listed_on_main: docsListed, project: `${GROUP}/${p.name}`, project_id: p.id, n, burst, person_from: personFrom,
    started: iso(t0), settle, summary: summarize(recs, facts), doc_lint_runs: Object.fromEntries(lint), revisions_missing_on_main: missing, facts };
  L.save(`a-${TAG}.json`, result);
  console.log(JSON.stringify(result.summary, null, 1), 'missing on main:', missing.length);
}

// ---------------------------------------------------------------- B
async function scenarioB() {
  const m = Number(opt('n', 30));
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
  const settle = await waitSettled(p, recs.filter((r) => r.iid).map((r) => r.iid), { timeoutMs: Number(opt('timeout', 900)) * 1000, label: 'B' });
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
  const conflictMarkers = /^(<<<<<<<|=======|>>>>>>>)/m.test(text);
  // the document as the portal parses it on main (every step block still there)
  const dv = await L.api('GET', `/api/v1/projects/${p.id}/documents/${D.docId(1)}`);
  const docOnMain = { status: dv.status, step_ids: new Set((JSON.stringify(dv.body).match(/"id":"S-\d+"/g) ?? [])).size, expected_steps: steps };
  const result = { scenario: 'B', tag: TAG, project: `${GROUP}/${p.name}`, project_id: p.id, m, settle,
    summary: summarize(recs, facts),
    merged_by_kind: recs.reduce((a, r) => { const mr = facts.mrs.find((x) => x.iid === r.iid); const k = `${r.kind}:${mr?.merged_at ? 'merged' : mr?.state}`; a[k] = (a[k] ?? 0) + 1; return a; }, {}),
    lost_edits: lost, conflict_markers_on_main: conflictMarkers, doc_on_main: docOnMain, main_sha: mainSha, main_checks: mainChecks, not_merged_views: views, facts };
  L.save(`b-${TAG}.json`, result);
  console.log(JSON.stringify({ ...result.summary, merged_by_kind: result.merged_by_kind, lost: lost.length, conflictMarkers }, null, 1));
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
  const k = Number(opt('n', 40));
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
  const recs = await burstCreate(p, branches, Number(opt('burst', k)), { enqueueByPerson: true });
  for (const r of recs) { const b = branches.find((x) => x.name === r.branch); r.i = b.i; r.breaker = b.breaker; }
  const settle = await waitSettled(p, recs.filter((r) => r.iid).map((r) => r.iid), { timeoutMs: Number(opt('timeout', 5400)) * 1000, label: 'C' });
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
  try { localCheck = (await pexec('sh', ['tests/check.sh'], { cwd: p.dir })).stdout.trim(); } catch (e) { localCheck = `FAILED: ${e.stdout}`; }
  await L.gitOk(p.dir, ['checkout', '-q', 'main']);
  const jobs = await sql(`select j.status, count(*), round(avg(extract(epoch from j.finished_at-j.started_at))::numeric,1), round(avg(extract(epoch from j.started_at-j.created_at))::numeric,1) from forge.ci_jobs j where j.project_id=${p.id} group by 1`);
  const bySource = pipes.reduce((a, x) => { const kk = `${x[1]}:${x[2]}`; a[kk] = (a[kk] ?? 0) + 1; return a; }, {});
  const breakers = recs.filter((r) => r.breaker).map((r) => ({ iid: r.iid, i: r.i, ...r.breaker,
    merged: !!facts.mrs.find((x) => x.iid === r.iid)?.merged_at, entries: facts.entries.filter((e) => e.iid === r.iid) }));
  const partners = pairs.map(([a]) => ({ i: a, iid: recs.find((r) => r.i === a)?.iid, merged: !!facts.mrs.find((x) => x.iid === recs.find((r) => r.i === a)?.iid)?.merged_at }));
  const result = { scenario: 'C', tag: TAG, project: `${GROUP}/${p.name}`, project_id: p.id, k, settle,
    summary: summarize(recs, facts), pipelines_by_source_status: bySource,
    ci_jobs: jobs.map(([s, c, run, wait]) => ({ status: s, count: +c, avg_run_s: +run, avg_wait_s: +wait })),
    main_first_parent_commits: line.length, main_red_push_pipelines: mainRed, main_without_push_pipeline: mainUnknown.length,
    local_check_on_main: localCheck, breakers, partners, facts, pipelines: pipes };
  L.save(`c-${TAG}.json`, result);
  console.log(JSON.stringify({ ...result.summary, bySource, jobs: result.ci_jobs, mainRed: mainRed.length, localCheck, breakers: breakers.map((b) => [b.i, b.merged, b.entries.map((e) => `${e.state}/${e.error_code}/${e.reason}`)]) }, null, 1));
}

// ---------------------------------------------------------------- D
async function scenarioD() {
  const n = Number(opt('n', 20));
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

// ---------------------------------------------------------------- main
const run = { a: scenarioA, b: scenarioB, c: scenarioC, d: scenarioD, sample: sampler }[scenario];
if (!run) {
  console.error('usage: merge-load.mjs <a|b|c|d|sample> [--n N] [--burst N] [--tag T]');
  process.exit(2);
}
abortIfStopped();
await run();
