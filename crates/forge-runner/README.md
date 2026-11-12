# forge-runner

**Responsibility**: the CI runner binary (M4 C4, plan 14 §3.1, ADR-0029). A
long-running process on a separate Linux host: it claims jobs from forge over
HTTPS with its runner token (`fgr_…`), fetches each job's pinned commit with
the job's own token (`fgj_…`), checks `.forge-ci.yml` against the pipeline's
`definition_hash`, and runs the job in a throwaway **rootless Podman**
container, streaming its log back. It registers nothing itself
(`forge admin runner register` on the forge host, runbook `ci-runner`).

**Dependency decision** (contract 9, ADR-0029): `forge-core` (ids, telemetry),
`forge-errors` (the registry) and `forge-client` (the HTTP client; forge-ci
through `forge_client::forge_ci`). Never a server crate, and nothing depends on
it (`forge-hosting/tests/dependency_direction.rs`).

## How a job runs (`job.rs`)

| Step | Rule | Failure |
|---|---|---|
| start-up and before every claim (`isolation.rs`) | Podman reads only the runner's own `<work_dir>/runner/podman/containers.conf` (`CONTAINERS_CONF` on every call, rewritten at start; empty `mounts.conf` and hooks). `podman create --name=forge-isolation-probe-<pid>` like a service (no `--volume`) from `<work_dir>/runner/empty-rootfs` (`--rootfs`, never started, no pull), `podman inspect`, `podman rm`: mounts, binds, devices, caps, sysctls, annotations, namespaces, shm and env names must be what the flags ask (ADR-0029 amendment 2026-11-12). Also in `check`; stale probes removed at start | FG-CI-0015 (`reason=unexpected_config` with `key`, `deviations` / `probe_failed` with `step` / `conf_unwritable`); the runner takes no job |
| claim | SHA 40 / 64 lower-case hex, a timeout, a `definition_hash` | FG-CI-0008 `claim_invalid` / `no_hash` |
| job directory | `<work_dir>/job/{work,script}` wiped, then created empty | FG-CI-0007 `workspace_unusable` |
| git | `init`; fetch of `refs/pipelines/<id>/head` from `/api/v1/runner/jobs/{job}/repo.git` (token only in git's environment configuration, `http.extraHeader`); the fetched commit must be the SHA | FG-CI-0009 (`step`, `reason`) |
| definition | `.forge-ci.yml` at the SHA: sha256 = `definition_hash`, parses (`LintContext::pinned_by_server`), holds the job with the claimed image and timeout | FG-CI-0008 — no container starts |
| container | `podman run --rm --network=none --pid=private --ipc=private --uts=private --cgroupns=private --env-host=false --http-proxy=false --init=false --shm-size=64m --read-only --cap-drop=all --security-opt=no-new-privileges … --entrypoint=/bin/sh -- <image@sha256> -eu /forge/script.sh`; the script is a read-only file, never argv | 125: FG-CI-0007; timeout: kill + rm, FG-CI-0010; other exit: `failed` with the code |
| services (C5s) | a job with `services`: `podman pod create --name=forge-job-<job>-<attempt> --network=none --share=net --memory=<m> --memory-swap=<m>` (only the loopback network namespace is shared) (the pod as a whole within one job's memory; pids stay per container — Podman has no pod pids limit; the pause image is built locally, never pulled); each service `podman create --pod=… --name=…-svc-<name>` with the job's confinement and limits (plus its `user`, `tmpfs`, `env`), then `podman start` (detached); the job container joins the pod (`--pod=`, loopback only). The runner does not wait for readiness. A service image outside `image_registries`: FG-CI-0011 with `service` | FG-CI-0014 (`reason=pod_create` / `service_start`, `service`, `step`) |
| secrets (C6) | the claim's `secrets` must be exactly the job's `secrets:` names, each value one line; written to `<work_dir>/job/secrets/job.env` (0700 / 0600, never mounted), passed as `podman create --env-file=…` and removed right after; masked values (and their base64 / percent encodings) replaced by `[masked]` in the log across chunk boundaries | FG-CI-0008 `secrets_mismatch` / `secret_invalid` — no container |
| needs' artifacts (C6) | a job with `needs`: before the script, `GET …/needs/artifacts[/{id}]` with the job's token, each file placed new at its path under `work` (never through a link, never over a file) | FG-CI-0020 (`reason`, `server_code`) |
| artifacts (C6) | after a script that exited 0: every regular file under `artifacts: { paths }` (links skipped and named, missing paths named), `PUT …/artifacts?path=` with the job's token | FG-CI-0019 over the bounds (nothing sent); FG-CI-0020 — the job fails |
| failed job | the last 50 lines of each service's log (`podman logs --tail=50`) appended as `[service <name>] …`, masked like the job's output | unreadable: one line and `forge.runner.service_log_unread` (warn) |
| always | `podman rm --force --ignore`, then `podman pod rm --force --ignore --time=0` for a job with services (retried once; still there: FG-CI-0007 `reason=pod_remove` and the runner stops like `workspace_dirty`), the job directory removed (through `podman unshare` when needed) | left over: FG-CI-0007 `workspace_dirty`, the runner stops |

The job's `timeout` is one deadline for the whole container phase (pod,
services, pulls, the script). The heartbeat (every 30 s) raises the job's stop flag on a cancel or a lost
lease (`FG-CI-0004`); the container is then killed and removed. Every
program starts through `process::ProcessRunner` (argv, cleared environment,
deadline, stop flag); the tests use a fake one and assert the argv.

## Public API

`config::RunnerConfig` (TOML + `FORGE_RUNNER_TOKEN`), `api::RunnerApi` /
`HttpApi`, `job::Executor`, `runner::Runner` (`run_once`, `run_forever`),
`git::GitPlan`, `isolation::check`, `podman::PodmanPlan`, `process::{Cmd, ProcessRunner,
SystemRunner}`, `workspace::Workspace`, `logsink::LogSink`.

## Error codes owned

FG-CI-0007 ContainerStartFailed, FG-CI-0008 JobDefinitionMismatch, FG-CI-0009
JobFetchFailed (warn), FG-CI-0010 JobTimedOut (info), FG-CI-0014
ServiceStartFailed (warn), FG-CI-0015 JobIsolationUnsafe (alert), FG-CI-0019
ArtifactTooLarge (info), FG-CI-0020 ArtifactTransferFailed (warn) — runbook
`ci-runner`.
Configuration problems are FG-SYS-0002 (`config_key`, `reason`).

## Tests

`cargo nextest run -p forge-runner` (no git server, no Podman: the fake
process runner; one test runs the real git). The real container test
`a_real_container_runs_without_network_and_is_removed` is ignored until the
Linux runner host exists (plan 14 §10.1): run it there with
`FORGE_RUNNER_TEST_IMAGE=<image@sha256:…> cargo nextest run -p forge-runner --run-ignored only`.
The services test `a_real_postgres_service_is_reached_over_loopback_and_nothing_leaks`
also needs `FORGE_RUNNER_TEST_PG_IMAGE=<postgres:16@sha256:…>`. Both ran in the
test-environment runner container (`ops/runner`, 2026-11-11).
