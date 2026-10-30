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

`forge serve` (connects the database and checks the schema before binding; never migrates), `forge worker` (the background tasks until SIGINT / SIGTERM: the outbox sequencer every 200 ms via `forge_db::sequencer::run_once`, numbering events only — audit rows wait for the per-process sink comparison of C9; a failure is logged once per streak with `task` and backs off up to 5 s; the C6 job runner and reaper; the C8 audit seal `audit_seal`: looks every 10 s, seals via `forge_db::seal::seal_due` a minute after the last seal or when > 1000 rows wait, at once again while `more` — it needs `audit.seal_keys_file`, without it the worker does not start, `FG-SYS-0002 reason=missing`), `forge db status [--check]` / `forge db migrate` (ADR-0014), `forge doctor [--json]` (read-only checks: config, database, schema, outbox backlog and sequencer lag (FG-DB-0005), audit seal backlog / verification (FG-DB-0015 / FG-DB-0011; `warn` when seals exist but this host has no seal keys, so HMACs were not checked), job queue, storage, git; one line each — ok / warn / FAIL / skip — exits with the most severe failure). Logs go to stderr, command output to stdout. Planned: `forge diag bundle` (M1).

**`forge admin`** (step D3): `token issue` / `token revoke` are the first
mutating admin commands. The host operator is the trust root, so they run
outside `authorize()` (documented exception, ADR-0019 §8); `forge_db::tokens`
enforces the rules itself (the `admin` scope only for administrators, no bots,
no disabled owners, 1–365 days, 50 active tokens). In production running them
is an approved runbook step. The audit actor is the system actor labelled with
the OS account (`USERNAME` / `USER`). Log events `forge.admin.token.issued` /
`forge.admin.token.revoked` carry the token id and public id only. Scopes:
`read_repository`, `write_repository`, `read_api`, `api`, `admin`.

**Boundary rule**: `main.rs::report` is the CLI's error boundary — it logs the
`ForgeError` once via `telemetry::log_failure` and prints only the localized
user message, the code and the 問い合わせ番号. Exit codes: 2 = usage/unknown
input, 1 = failure.

**Error codes owned**: uses `FG-SYS-0003` (UnknownErrorCode). `forge admin token`
raises FG-AUTH-0008 `reason=scope_unknown` / `expires_malformed` itself (the name
is not echoed); the rest come from `forge_db::tokens` (see the forge-db README).
