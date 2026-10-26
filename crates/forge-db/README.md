# forge-db

**Responsibility**: the PostgreSQL layer (ADR-0002) — connecting with TLS and
every timeout applied, and mapping driver errors to registered codes. Later:
migrations (FG-DB-0003/0004), the Outbox and the job queue.

**Public API**
- `Db::connect(&DatabaseConfig) -> Result<Db, ForgeError>` — pool from
  `forge_core::config` (ADR-0013); checks PostgreSQL ≥ 16; logs
  `forge.db.connected` with `db_target` (host:port/dbname), `tls_mode`,
  `server_version_num`.
- `Db::pool()`, `Db::acquire()`, `Db::ping()`, `Db::target()`.
- `query_error(query: &'static str, sqlx::Error) -> ForgeError` — use at
  every statement: `.map_err(|e| query_error("repo.insert", e))`. `query` is
  a static name, never the SQL text.

**Bounded waits**: `connect_timeout` (whole connect), `acquire_timeout`
(pool), and per session `statement_timeout`, `lock_timeout`,
`idle_in_transaction_session_timeout`. `application_name = forge`.

**Error codes owned**: FG-DB-0001 DbUnavailable (`reason`, `target`),
FG-DB-0002 DbQueryFailed (`query`, `sqlstate`, `reason`). Reserved:
FG-DB-0003/0004 (migrations), FG-DB-0005 (Outbox sequencer, ADR-0012).

**Runbook**: `ops/runbooks/db-unavailable.md`.

**Tests**: need the development database (ADR-0011) —
`docker compose -f ops/dev/compose.yml up -d --wait` in `Git/`, then
`cargo test -p forge-db`. Without it the DB tests **fail** with that command
(they never skip). Override the target with `FORGE_TEST_DATABASE_URL`
(password `forge_dev`, TLS disabled).

**Gotchas**
- TLS defaults to `verify-full`; there is no `prefer`. The compose database
  has no TLS, so development uses `database.tls_mode = "disable"` (a warning is
  logged for a non-loopback host).
- No `query!` macros yet, so there is no `.sqlx/` offline data; it arrives
  with the first table, together with `cargo sqlx prepare --check` in
  `/verify`.
- sqlx is pinned to 0.8.6 (0.9 needs Rust 1.94 and is not yet evaluated).
