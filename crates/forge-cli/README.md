# forge-cli

**Responsibility**: the `forge` binary — operator commands now, the server
entry point from M1.

**Commands**

| Command | Purpose |
|---|---|
| `forge explain <CODE>` | cause / diagnosis / fix / runbook of an FG code |
| `forge errors list` | every registered code (id, HTTP, severity, cause) |
| `forge --version` | version |

`forge serve` (connects the database and checks the schema before binding; never migrates), `forge db status [--check]` / `forge db migrate` (ADR-0014), `forge doctor [--json]` (read-only checks: config, database, schema, storage, git; one line each, exits with the most severe failure). Logs go to stderr, command output to stdout. Planned: `forge diag bundle` (M1).

**Boundary rule**: `main.rs::report` is the CLI's error boundary — it logs the
`ForgeError` once via `telemetry::log_failure` and prints only the localized
user message, the code and the 問い合わせ番号. Exit codes: 2 = usage/unknown
input, 1 = failure.

**Error codes owned**: uses `FG-SYS-0003` (UnknownErrorCode).
