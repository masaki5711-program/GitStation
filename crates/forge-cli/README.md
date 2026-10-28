# forge-cli

**Responsibility**: the `forge` binary — operator commands now, the server
entry point from M1.

**Commands**

| Command | Purpose |
|---|---|
| `forge explain <CODE>` | cause / diagnosis / fix / runbook of an FG code |
| `forge errors list` | every registered code (id, HTTP, severity, cause) |
| `forge --version` | version |

`forge serve` (connects the database and checks the schema before binding; never migrates), `forge worker` (the background tasks until SIGINT / SIGTERM: the outbox sequencer every 200 ms via `forge_db::sequencer::run_once`, numbering events only — audit rows wait for the per-process sink comparison of C9; a failure is logged once per streak with `task` and backs off up to 5 s; C6 jobs and C8 seals join as further `worker::Task` variants), `forge db status [--check]` / `forge db migrate` (ADR-0014), `forge doctor [--json]` (read-only checks: config, database, schema, outbox backlog and sequencer lag (FG-DB-0005), storage, git; one line each, exits with the most severe failure). Logs go to stderr, command output to stdout. Planned: `forge diag bundle` (M1).

**Boundary rule**: `main.rs::report` is the CLI's error boundary — it logs the
`ForgeError` once via `telemetry::log_failure` and prints only the localized
user message, the code and the 問い合わせ番号. Exit codes: 2 = usage/unknown
input, 1 = failure.

**Error codes owned**: uses `FG-SYS-0003` (UnknownErrorCode).
