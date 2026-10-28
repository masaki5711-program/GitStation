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
- `Db::within(operation, future)` — client-side bound (acquire + statement
  timeout) for short multi-statement operations; `Db::close()` is bounded too.
- The app login must not be a superuser / BYPASSRLS or reach forge_owner,
  forge_seq or forge_purge (`wrong_role` at connect). The bootstrap objects
  live in schema `forge_audit`, owned by the member-less role `forge_audit`
  (ADR-0018 amendment).
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
AppendOnlyViolation (XF001 and the bootstrap guards, `reason` / `guard`), FG-DB-0005
OutboxSequencerLag (`unsequenced_count`, `heartbeat_age_ms`, `lock_holder_pid`),
FG-DB-0011 AuditSealMismatch (`reason` recompute / chain / hmac / gap, `detail`,
`seal_id`), FG-DB-0015 AuditSealBacklog (`oldest_age_s`, `audit_sequencing`).

**Runbooks**: `ops/runbooks/db-unavailable.md`, `db-bootstrap.md`,
`append-only-violation.md`, `outbox-sequencer-lag.md`, `audit-seal-mismatch.md`,
`audit-seal-backlog.md`.

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

**Changes** (ADR-0016 §6, step C3): `change::ChangeTx` (crate-private) runs a
change on the write pool — state writes via `conn()` / `within()`, then
`commit(ChangeRecord)` writes the audit row and up to 100 events
(`DomainEvent`, UUIDv7 ids) within 30 s, then commits (own 10 s bound; a
timeout there is FG-DB-0002 `commit_outcome_unknown`). A repeated
`dedup_key` rolls the whole change back (`Committed::Deduped`). Invalid
changes are FG-DB-0010 before anything is written (rules in
`change/validate.rs`; client-supplied values are cut, never refused);
an event committed without an audit row in its transaction — derived events
included until C5 — is FG-DB-0013 (deferred trigger `events_audit_present`).
The typed commands (step D) are its only callers.

**Schema** (ADR-0016 §1, step C2): `forge.events` (outbox), `forge.audit_log`,
`forge.event_feed_state`, `forge.event_subscribers`,
`forge.subscriber_dead_letters`, `forge.jobs`; the DDL-status decision is
`forge_audit.ddl_audit_status()` (bootstrap, owned by forge_audit). The ledger `tables.toml` lists every table
(`guarded`, `kind`, `ro_columns`); rules for new migrations are in
`migrations/README.md`. events / audit_log are append-only (per-table guard
triggers, XF001 → FG-DB-0009): only forge_seq numbers rows, only forge_purge
deletes (published rows past 7 / 365 days; never db.migrate rows) or
redacts; event_feed_state only moves forward; forge_app cannot write the
reserved audit actions (db.migrate, audit.*, feed.*, events.*). `bootstrap::check` also verifies their triggers, the
exact privileges of every migrated table and unacknowledged unexpected DDL.
Schema tests (`src/schema_tests.rs`) migrate a fresh database each.

**Sequencer** (ADR-0016 §7, ADR-0018 §3, step C4): `forge.assign_seq(max_batch,
include_audit)` (migration `20261027110000`, owned by forge_seq, SECURITY
DEFINER, EXECUTE for forge_app only) numbers committed events gap-free in id
order — and audit rows only when `include_audit` AND
`audit_sequencing_enabled` — under an advisory lock, creating the
`event_feed_state` row on its first call. `sequencer::run_once(&db,
BatchSize, AuditSequencing)` calls it on the write pool and, in the same
transaction, `NOTIFY forge_events_sequenced` (payload: the last event seq)
when it numbered events. `sequencer::backlog` / `Backlog::check_lag` feed
`forge doctor` (`outbox.unsequenced_count`, `outbox.sequencer_lag`,
FG-DB-0005 when rows wait and the heartbeat is older than 30 s).
`bootstrap::check` verifies the function exactly (`bootstrap/definer.rs`).

**Audit seal** (ADR-0018 §4, step C8): `forge.audit_seal` (migration
`20261027130000`; its guard lets only `shipped_at` go NULL → value once,
forces `sealed_at`, requires each insert to continue the chain and end at or
below `audit_high_seq`; no DELETE / TRUNCATE) holds one row per sealed range.
`seal::v1` is the single definition of hash version 1 (columns, SELECT,
RFC 8785 row bytes; golden vector `tests/fixtures/audit_seal_v1_golden.py`).
`seal::seal_pending(&db, &SealKeys)` is the `audit.seal` job body (try-lock
`(1718579815, 8)`, ≤ 5000 rows per seal, `more` when rows remain, gap →
FG-DB-0011). `seal::verify_range` recomputes seals in READ ONLY
transactions on the write pool (`forge doctor audit.seal_verify`, 24 h);
`seal::backlog` feeds `audit.seal_backlog` (FG-DB-0015 past 5 minutes). The
HMAC keys come from `audit.seal_keys_file` (`forge_core::config::SealKeys`).
