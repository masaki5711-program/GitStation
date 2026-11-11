#!/usr/bin/env node
// Builds the CI job image (ops/ci-image/Dockerfile) with Docker, pushes it to
// the test registry of ops/runner (compose project `gs-runner-test`), and
// prints the digests to write in `.forge-ci.yml`. Also builds the PostgreSQL
// service image (postgres.Containerfile) inside the runner's Podman.
//
// The test registry publishes no port: it is reached as `registry:5000` from
// the compose network only. So the image goes `docker save` -> the runner
// container's `podman load` -> `podman push registry:5000/...`, the same way
// ops/runner/README.md step 3 fills the registry.
//
// Usage (repository root or anywhere in it):
//   node ops/ci-image/build-and-push.mjs [--skip-build] [--skip-postgres] [--jobs N]
//
// Node built-ins only; every command is an argument vector (no shell string
// on this host) and has a timeout.

import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..", "..");
const PROJECT = "gs-runner-test";
const REGISTRY = "registry:5000";
const MIN = 60_000;

function parseArgs(argv) {
  const opts = { build: true, postgres: true, jobs: 4 };
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === "--skip-build") opts.build = false;
    else if (a === "--skip-postgres") opts.postgres = false;
    else if (a === "--jobs") {
      const n = Number(argv[(i += 1)]);
      if (!Number.isInteger(n) || n < 1 || n > 32) fail(`--jobs needs 1..32, got ${argv[i]}`);
      opts.jobs = n;
    } else fail(`unknown argument ${a}`);
  }
  return opts;
}

function fail(msg) {
  process.stderr.write(`build-and-push: ${msg}\n`);
  process.exit(1);
}

/** Runs argv; resolves stdout (stderr streams through). Rejects on a non-zero exit or the timeout. */
function run(cmd, args, { timeoutMs = 5 * MIN, input, echo = true, stdin } = {}) {
  return new Promise((ok, ko) => {
    if (echo) process.stderr.write(`$ ${cmd} ${args.join(" ")}\n`);
    const child = spawn(cmd, args, { cwd: ROOT, stdio: [stdin ?? "pipe", "pipe", "inherit"] });
    let out = "";
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      ko(new Error(`${cmd} ${args[0]} timed out after ${timeoutMs / 1000}s`));
    }, timeoutMs);
    child.stdout.on("data", (d) => {
      out += d;
    });
    child.on("error", (e) => {
      clearTimeout(timer);
      ko(new Error(`${cmd}: ${e.message}`));
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      if (code === 0) ok(out);
      else ko(new Error(`${cmd} ${args.slice(0, 2).join(" ")} exited with ${code}`));
    });
    if (child.stdin && stdin === undefined) {
      if (input !== undefined) child.stdin.end(input);
      else child.stdin.end();
    }
  });
}

/** `docker save <ref>` piped into `docker exec -i <runner> podman load`. */
function saveIntoRunner(ref, runner, timeoutMs) {
  return new Promise((ok, ko) => {
    process.stderr.write(`$ docker save ${ref} | docker exec -i ${runner} podman load -q\n`);
    const save = spawn("docker", ["save", ref], { cwd: ROOT, stdio: ["ignore", "pipe", "inherit"] });
    const load = spawn("docker", ["exec", "-i", runner, "podman", "load", "-q"], {
      cwd: ROOT,
      stdio: ["pipe", "inherit", "inherit"],
    });
    save.stdout.pipe(load.stdin);
    let saveCode = null;
    const timer = setTimeout(() => {
      save.kill("SIGKILL");
      load.kill("SIGKILL");
      ko(new Error(`docker save | podman load timed out after ${timeoutMs / 1000}s`));
    }, timeoutMs);
    save.on("error", (e) => ko(new Error(`docker save: ${e.message}`)));
    load.on("error", (e) => ko(new Error(`docker exec: ${e.message}`)));
    save.on("close", (code) => {
      saveCode = code;
    });
    load.on("close", (code) => {
      clearTimeout(timer);
      if (saveCode !== 0 && saveCode !== null) ko(new Error(`docker save exited with ${saveCode}`));
      else if (code !== 0) ko(new Error(`podman load exited with ${code}`));
      else ok();
    });
  });
}

function toolchainChannel() {
  const text = readFileSync(join(ROOT, "rust-toolchain.toml"), "utf8");
  const m = /^channel\s*=\s*"([^"]+)"/m.exec(text);
  if (!m) fail("rust-toolchain.toml has no channel");
  return m[1];
}

function dockerfileRustVersion() {
  const text = readFileSync(join(HERE, "Dockerfile"), "utf8");
  const m = /^ARG RUST_VERSION=(\S+)/m.exec(text);
  if (!m) fail("ops/ci-image/Dockerfile has no ARG RUST_VERSION");
  return m[1];
}

async function runnerContainer() {
  const out = await run("docker", [
    "ps",
    "-q",
    "--filter",
    `label=com.docker.compose.project=${PROJECT}`,
    "--filter",
    "label=com.docker.compose.service=runner",
  ]);
  const ids = out.split(/\s+/).filter(Boolean);
  if (ids.length !== 1) {
    fail(`expected one running runner of compose project ${PROJECT}, found ${ids.length} (ops/runner/README.md)`);
  }
  return ids[0];
}

/** Pushes `local` (already in the runner's Podman) as `remote`; returns the manifest digest. */
async function pushFromRunner(runner, local, remote) {
  const script =
    'set -eu; d=$(mktemp); podman push -q --digestfile "$d" "$1" "$2"; cat "$d"; rm -f "$d"';
  const out = await run("docker", ["exec", runner, "sh", "-c", script, "push", local, remote], {
    timeoutMs: 20 * MIN,
  });
  const digest = out.trim().split(/\s+/).pop();
  if (!/^sha256:[0-9a-f]{64}$/.test(digest ?? "")) fail(`podman push printed no digest: ${out}`);
  return digest;
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const channel = toolchainChannel();
  const pinned = dockerfileRustVersion();
  if (channel !== pinned) {
    fail(`rust-toolchain.toml says ${channel} but the Dockerfile says ${pinned}: update RUST_VERSION and the base digest`);
  }
  const localTag = `gitstation/ci-rust:${channel}`;
  const remote = `${REGISTRY}/ci/rust:${channel}`;

  if (opts.build) {
    await run(
      "docker",
      [
        "build",
        "-f",
        "ops/ci-image/Dockerfile",
        "--build-arg",
        `CARGO_BUILD_JOBS=${opts.jobs}`,
        "-t",
        localTag,
        ".",
      ],
      { timeoutMs: 90 * MIN, stdin: "ignore" },
    );
  }
  const size = Number((await run("docker", ["image", "inspect", localTag, "--format", "{{.Size}}"])).trim());

  const runner = await runnerContainer();
  await saveIntoRunner(localTag, runner, 30 * MIN);
  const rustDigest = await pushFromRunner(runner, `docker.io/${localTag}`, remote);
  // The pushed copy is what jobs pull (by digest); drop the loaded one.
  await run("docker", ["exec", runner, "podman", "rmi", "--force", `docker.io/${localTag}`], { timeoutMs: 5 * MIN });

  let pgDigest = null;
  if (opts.postgres) {
    const containerfile = readFileSync(join(HERE, "postgres.Containerfile"));
    const pgLocal = "localhost/gitstation/ci-postgres:16";
    // The Containerfile arrives on stdin into an empty context directory.
    const build =
      'set -eu; d=$(mktemp -d); trap \'rm -rf "$d"\' EXIT; cat > "$d/Containerfile"; podman build -q -t "$1" "$d"';
    await run("docker", ["exec", "-i", runner, "sh", "-c", build, "build", pgLocal], {
      input: containerfile,
      timeoutMs: 15 * MIN,
    });
    pgDigest = await pushFromRunner(runner, pgLocal, `${REGISTRY}/ci/postgres:16-forge`);
    await run("docker", ["exec", runner, "podman", "rmi", "--force", pgLocal], { timeoutMs: 5 * MIN });
  }

  const lines = [
    `local image   ${localTag}  (${(size / 1024 ** 3).toFixed(2)} GiB as Docker reports it: compressed with the containerd store)`,
    `job image     ${REGISTRY}/ci/rust@${rustDigest}`,
  ];
  if (pgDigest) lines.push(`service image ${REGISTRY}/ci/postgres@${pgDigest}`);
  process.stdout.write(`${lines.join("\n")}\n`);
}

main().catch((e) => fail(e.message));
