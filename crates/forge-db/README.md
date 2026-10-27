# forge-db

**Responsibility**: the PostgreSQL layer (ADR-0002, ADR-0018) — connecting with TLS and
every timeout applied, the write / read pool split and the migration
connection, mapping driver errors to registered codes, migrations
(ADR-0014) and the bootstrap checks. Later: the Outbox and the job queue.

**Public API**
- `Db::connect(&DatabaseConfig) -> Result<Db, ForgeError>` — pool from
  `forge_core::config` (ADR-0013); checks PostgreSQL ≥ 16; logs
  `forge.db.connected` with `db_target` (host:port/dbname), `tls_mode`,
  `server_version_num`.
- Two pools, both logged in as `forge_app` (`database.url`): the write pool
  stays inside this crate (`write_pool()`, typed commands from step C3);
  `Db::pool()` returns the **`ReadPool`** (sessions run as `forge_ro` with
  `default_transaction_read_only = on`, set in the startup packet and
  verified by a probe at connect). Sizes: `database.write_max_connections`,
  `database.read_max_connections` (10 each).
- `Db::acquire()` (read pool), `Db::ping()` (both pools), `Db::close()`,
  `Db::target()`.
- `Db::connect_migrator(&DatabaseConfig) -> MigratorConn` — one connection on
  `database.migrate_url` (`forge_migrator`, `application_name = forge-migrate`),
  `SET ROLE forge_owner`; refuses a login that is a superuser or cannot
  become the owner (FG-SYS-0002 `reason=wrong_role`, `role_check`).
- `query_error` (write pool / migration connection) and `read_query_error`
  (read pool) — use at every statement:
  `.map_err(|e| query_error("repo.insert", e))`. `query` is a static name,
  never the SQL text.
- `bootstrap::check(&Db, privileged_members)` / `bootstrap::check_connections`
  — what `forge doctor` reports as `db.bootstrap` / `db.connections`.
  `bootstrap::FILES` embeds `ops/db/bootstrap/*.sql`; `BOOTSTRAP_VERSION` must
  match the version 03 writes (tested).
- Feature `test-support` (dev-dependency of other crates only):
  `test_support::FreshDb` — a database bootstrapped with the real files,
  logins with synthetic passwords.

**Bounded waits**: `connect_timeout` (whole connect), `acquire_timeout`
(pool), and per session `statement_timeout`, `lock_timeout`,
`idle_in_transaction_session_timeout`. `application_name = forge`.

**Error codes owned**: FG-DB-0001 DbUnavailable (`reason`, `target`),
FG-DB-0002 DbQueryFailed (`query`, `sqlstate`, `reason`), FG-DB-0003
MigrationFailed, FG-DB-0004 SchemaIncompatible, FG-DB-0009
AppendOnlyViolation (XF001 and the bootstrap guards, `reason` / `guard`). Reserved:
FG-DB-0005 (Outbox sequencer, ADR-0012).

**Runbooks**: `ops/runbooks/db-unavailable.md`, `db-bootstrap.md`,
`append-only-violation.md`.

**Tests**: need the development database (ADR-0011) —
`docker compose -f ops/dev/compose.yml up -d --wait` in `Git/`, then
`cargo test -p forge-db`. Without it the DB tests **fail** with that command
(they never skip). Override the target with `FORGE_TEST_DATABASE_URL`
(a superuser, password `forge_dev`, TLS disabled). The tests run
`ops/db/bootstrap/01` once per process (cluster roles — shared by every
test process, serialized by an advisory lock) and 02 + 03 for every
`FreshDb`.

**Gotchas**
- TLS: `verify-full` (default), `require`, `disable` — no `prefer`, no
  `verify-ca` (the rustls trust store also holds the public web roots;
  ADR-0013 records the remaining risk). `require` / `disable` to a
  non-loopback host also needs `database.tls_allow_unverified = true`
  (FG-SYS-0002 `unverified_remote` otherwise). The compose database
  has no TLS, so development uses `database.tls_mode = "disable"` (a warning is
  logged for a non-loopback host).
- No `query!` macros yet, so there is no `.sqlx/` offline data; it arrives
  with the first table, together with `cargo sqlx prepare --check` in
  `/verify`.
- sqlx is pinned to 0.8.6 (0.9 needs Rust 1.94 and is not yet evaluated).
- Options are built explicitly (host / port / user / dbname from the URL,
  `new_without_pgpass`); a set `PG*` variable (PGPASSWORD, PGSSLROOTCERT,
  PGOPTIONS, …) is FG-SYS-0002 `reason=libpq_env`.
- An outage is always FG-DB-0001, also for a later statement (lost
  connection, 08xxx, 57P01–57P03, 53300, 28xxx); FG-DB-0002 means the
  statement failed on a working connection. Use `fetch_optional` when "no
  row" is a normal outcome (`RowNotFound` is `reason=row_not_found`).
- Server messages are logged only for SQLSTATE classes without row data;
  constraint errors log `constraint` / `table`, never the values.
- `Cargo.lock` lists `rsa`, `sqlx-mysql` and `sqlx-sqlite` (via the sqlx
  `macros` feature); `cargo tree -i rsa` / `-i libsqlite3-sys` show they are
  not built. An audit tool reading the lockfile will flag RUSTSEC-2023-0071 —
  waived with this reason in `.cargo/audit.toml` (user, 2026-10-26).
- `Db::connect` makes one direct probe connection (real failure reason,
  version check) and then a lazy pool: `min_connections` are opened in the
  background, so a failure there surfaces later on `acquire` (FG-DB-0001).

**Migrations** (ADR-0014): `migrate(&mut MigratorConn, &MIGRATOR, timeout)`,
`status(&db, &MIGRATOR)` (read pool, never creates `_sqlx_migrations`),
`check_schema(&db, &MIGRATOR)` (what `forge serve` runs before binding:
FG-DB-0004 for pending / db_ahead / checksum_mismatch / dirty). Product
migrations live in `migrations/` (`YYYYMMDDHHMMSS_desc.sql`, LF, forward
only); test migrators in `tests/fixtures/migrator/`. Migration tests each
create a fresh database `forge_t_<uuid>` and drop it at the end.
`forge db status --check` exits 1 with FG-DB-0004 when serve would refuse.
Runbook: `ops/runbooks/db-migration.md`.
