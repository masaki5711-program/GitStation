# forge-cli

**Responsibility**: the `forge` binary — operator commands now, the server
entry point from M1.

**Commands**

| Command | Purpose |
|---|---|
| `forge explain <CODE>` | cause / diagnosis / fix / runbook of an FG code |
| `forge errors list` | every registered code (id, HTTP, severity, cause) |
| `forge --version` | version |
| `forge admin repo-path <repo_id> [--storage-root]` | where a repository is stored (from its id, ADR-0009) and whether it exists |
| `forge admin token issue --user <name> --scopes <list> [--expires-in <n>d] [--name] [--agent]` | issue a personal access token (ADR-0019); printed **once** on stdout, never logged |
| `forge admin token revoke <token_id>` | revoke a token (`already revoked` writes nothing) |
| `forge admin token list --user <name>` | a user's tokens, newest first (never the secret) |
| `forge admin user disable <username> [--force]` | disable a user and revoke their active tokens; refused (FG-AUTH-0009) when it would leave no active human administrator or a top-level group without a direct active Owner, unless `--force` |
| `forge admin bootstrap-admin --user <name> [--create --email <e> [--display-name <n>]]` | make an active human the administrator (or create one) when forge has none: first install, recovery after `--force` |
| `forge admin repo reprovision <repo_id>` | enqueue the provisioning job of a repository still `provisioning` again (after its job died; FG-REPO-0012); refused for a ready repository (FG-REPO-0014 `repository_ready`) |

`forge serve` (connects the database and checks the schema before binding; never migrates; injects the database into forge-web as the readiness probe and as the token authenticator — `forge_db::tokens::authenticate` behind `forge_core::Authenticator`, ADR-0019 §7, wrapped in `forge_web::BoundedAuthenticator` with `max(1, database.write_max_connections / 2)` concurrent lookups), `forge worker` (the background tasks until SIGINT / SIGTERM: the outbox sequencer every 200 ms via `forge_db::sequencer::run_once`, numbering events only — audit rows wait for the per-process sink comparison of C9; a failure is logged once per streak with `task` and backs off up to 5 s; the C6 job runner and reaper; the C8 audit seal `audit_seal`: looks every 10 s, seals via `forge_db::seal::seal_due` a minute after the last seal or when > 1000 rows wait, at once again while `more` — it needs `audit.seal_keys_file`, without it the worker does not start, `FG-SYS-0002 reason=missing`; the D4 MR3b job handler `repository.provision` (60 s): reads the repository's token on the write pool, `StorageRoot::init_bare_or_adopt` (git from `PATH`, HOME pinned to the storage root, 20 s per git call, no hooks path until M1 phase 10), then `forge_db::repositories::mark_ready`; logs `forge.repo.provisioned` (`repo_id`, `project_id`, `outcome` created / adopted / already_ready, `attempt`, `duration_ms`) once; storage failures retry or dead-letter by `provision_error_is_retryable` (FG-REPO-0004 `adopt_refused` is dead at once), database failures retry on FG-DB-0001 / 0002 — so the worker also needs `storage.root` and does not start without it), `forge db status [--check]` / `forge db migrate` (ADR-0014), `forge doctor [--json]` (read-only checks: config, database, schema, outbox backlog and sequencer lag (FG-DB-0005), audit seal backlog / verification (FG-DB-0015 / FG-DB-0011; `warn` when seals exist but this host has no seal keys, so HMACs were not checked), job queue, subscribers, who administers forge (`users.administration`: FG-AUTH-0010 warn when there is no active human administrator or a top-level group has no direct active Owner; `warn` without a code when there are no users yet), repository provisioning (`repositories.provisioning`: FG-REPO-0012 alert when a repository waits in `provisioning` > 10 min or its job is not queued / running), ready repositories without a directory (`repositories.storage`: FG-REPO-0002, at most 10 000 checked per run — beyond that `warn` / `truncated`; skipped when the storage root does not open), storage, git; one line each — ok / warn / FAIL / skip — exits with the most severe failure). Logs go to stderr, command output to stdout. Planned: `forge diag bundle` (M1).

**`forge admin`** (step D3): `token issue` / `token revoke` are the first
mutating admin commands. The host operator is the trust root, so they run
outside `authorize()` (documented exception, ADR-0019 §8); `forge_db::tokens`
enforces the rules itself (the `admin` scope only for administrators, no bots,
no disabled owners, 1–365 days, 50 active tokens). In production running them
is an approved runbook step. The audit actor is the system actor labelled with
the OS account (`USERNAME` / `USER`). Log events `forge.admin.token.issued` /
`forge.admin.token.revoked` carry the token id and public id only. Scopes:
`read_repository`, `write_repository`, `read_api`, `api`, `admin`.
`user disable` and `bootstrap-admin` (D3 MR2, ADR-0019 §10–§11 and Amendment)
are mutating too: `forge_db::users` enforces the administration guard, and
`--force` (the first `--force` of the CLI) exists only here — HTTP never gets a
way past the guard. Log events `forge.admin.user.disabled` (`user_id`,
`forced`, `outcome`, `ownerless_groups`, `left_without_admin`) and
`forge.admin.user.admin_bootstrapped` (`user_id`, `created`) carry ids and
flags, never names. Runbook: `ops/runbooks/user-offboarding.md`.
`repo reprovision` (D4 MR3b) is mutating as well: `forge_db::repositories::reprovision`
locks the repository, refuses a ready one and writes the job with an audit-only
`repository.reprovision_requested` row; a live job holding the key writes nothing.
Log event `forge.admin.repo.reprovisioned` (`repo_id`, `project_id`, `job_id`,
`outcome` queued / already_live). Runbook: `ops/runbooks/repository-provisioning.md`.

**Boundary rule**: `main.rs::report` is the CLI's error boundary — it logs the
`ForgeError` once via `telemetry::log_failure` and prints only the localized
user message, the code and the 問い合わせ番号. Exit codes: 2 = usage/unknown
input, 1 = failure.

**Error codes owned**: uses `FG-SYS-0003` (UnknownErrorCode). `forge admin token`
raises FG-AUTH-0008 `reason=scope_unknown` / `expires_malformed` itself (the name
is not echoed); the rest come from `forge_db::tokens` (see the forge-db README).
`forge admin user disable` / `bootstrap-admin` surface FG-AUTH-0009 from
`forge_db::users`; `forge doctor` reports FG-AUTH-0010, FG-REPO-0012 and FG-REPO-0002
(`repositories.*`). `forge admin repo reprovision` surfaces FG-REPO-0014 / 0005 from
`forge_db::repositories`.
