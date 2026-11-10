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
| claim | SHA 40 / 64 lower-case hex, a timeout, a `definition_hash` | FG-CI-0008 `claim_invalid` / `no_hash` |
| job directory | `<work_dir>/job/{work,script}` wiped, then created empty | FG-CI-0007 `workspace_unusable` |
| git | `init`; fetch of `refs/pipelines/<id>/head` from `/api/v1/runner/jobs/{job}/repo.git` (token only in git's environment configuration, `http.extraHeader`); the fetched commit must be the SHA | FG-CI-0009 (`step`, `reason`) |
| definition | `.forge-ci.yml` at the SHA: sha256 = `definition_hash`, parses (`LintContext::pinned_by_server`), holds the job with the claimed image and timeout | FG-CI-0008 — no container starts |
| container | `podman run --rm --network=none --read-only --cap-drop=all --security-opt=no-new-privileges … --entrypoint=/bin/sh -- <image@sha256> -eu /forge/script.sh`; the script is a read-only file, never argv | 125: FG-CI-0007; timeout: kill + rm, FG-CI-0010; other exit: `failed` with the code |
| always | `podman rm --force --ignore`, the job directory removed (through `podman unshare` when needed) | left over: FG-CI-0007 `workspace_dirty`, the runner stops |

The heartbeat (every 30 s) raises the job's stop flag on a cancel or a lost
lease (`FG-CI-0004`); the container is then killed and removed. Every
program starts through `process::ProcessRunner` (argv, cleared environment,
deadline, stop flag); the tests use a fake one and assert the argv.

## Public API

`config::RunnerConfig` (TOML + `FORGE_RUNNER_TOKEN`), `api::RunnerApi` /
`HttpApi`, `job::Executor`, `runner::Runner` (`run_once`, `run_forever`),
`git::GitPlan`, `podman::PodmanPlan`, `process::{Cmd, ProcessRunner,
SystemRunner}`, `workspace::Workspace`, `logsink::LogSink`.

## Error codes owned

FG-CI-0007 ContainerStartFailed, FG-CI-0008 JobDefinitionMismatch, FG-CI-0009
JobFetchFailed (warn), FG-CI-0010 JobTimedOut (info) — runbook `ci-runner`.
Configuration problems are FG-SYS-0002 (`config_key`, `reason`).

## Tests

`cargo nextest run -p forge-runner` (no git server, no Podman: the fake
process runner; one test runs the real git). The real container test
`a_real_container_runs_without_network_and_is_removed` is ignored until the
Linux runner host exists (plan 14 §10.1): run it there with
`FORGE_RUNNER_TEST_IMAGE=<image@sha256:…> cargo nextest run -p forge-runner --run-ignored only`.
