# forge-db

**Responsibility**: the PostgreSQL layer (ADR-0002, ADR-0018) — connecting with TLS and
every timeout applied, the write / read pool split and the migration
connection, mapping driver errors to registered codes, migrations
(ADR-0014) and the bootstrap checks. The Outbox sequencer and the job queue.

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
- `users::create(&Db, &ChangeContext, &NewUser) -> Result<Created, ForgeError>`
  and `users::disable(&Db, &ChangeContext, UserId) -> Result<DisableOutcome, ForgeError>`
  (step D1; `NewUser::human`, `AuthSource`): one change each, with its `user.*`
  audit row and no event. `create` also writes the personal namespace
  (`kind = user`, `owner_id` = the user), under the path's advisory lock
  (`namespaces::lock_path`, crate-private; D5 rename / move / delete / release take
  it too). Disabling a disabled user writes nothing.
- `groups::create(&Db, &ChangeContext, &NewGroup) -> Result<CreatedGroup, ForgeError>`
  (step D2): the group, its namespace (`kind = group`, only at the top level or
  under a group) and its first Owner, under the path lock, with one
  `group.created` audit row. `groups::{add_member, change_member, remove_member}`:
  one `member.*` audit row each on the group (audit-only); every one locks the
  group row first; a top-level group keeps at least one direct active Owner;
  bots are never members. `change_member` to the same role is
  `MemberChange::Unchanged` and writes nothing.
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
OutboxSequencerLag (`unsequenced_count`, `heartbeat_age_ms`, `lock_holder_pid`), FG-DB-0006
JobDeadLettered (`job_id`, `kind`, `attempts`, `last_error_code`, `origin_request_id`;
`reason`), FG-DB-0007 JobLeaseExpired (`job_id`, `kind`, `worker_id`, `attempt`; `reason`), FG-DB-0017 JobQueueBacklog (`kind`, `ready`,
`oldest_wait_ms`; `reason`), FG-DB-0011 AuditSealMismatch (`reason` recompute / chain /
hmac / gap, `detail`, `seal_id`), FG-DB-0015 AuditSealBacklog (`oldest_age_s`,
`audit_sequencing`), FG-DB-0008 SubscriberLag (`reason` stuck / behind / quarantined /
fanout_limit, `subscriber`, `seq`, `event_type`, `lag_events`), FG-DB-0013 AuditMissing
(`reason` no_audit_row / derived_lineage / derived_depth / derived_fanout /
derived_progress / audited_change with `table`).
Raised by `users::create` / `users::disable`: FG-REPO-0007 / 0008 (`PathSegment`),
FG-REPO-0009 (`reason=sibling` or `tombstoned`), FG-AUTH-0005 (`reason=email_taken`,
ignoring case), FG-AUTH-0006 (field rules, `reason` / `len`), FG-AUTH-0007
(`reason=employee_number_taken` / `external_id_taken`), FG-REPO-0005 (`resource=user`,
`reason=absent` on disable), FG-SYS-0001
(`reason=unexpected_dedup`, `unknown_user_kind`).
Raised by `groups`: FG-REPO-0007 (also `too_deep`, `path_too_long`) / 0008 / 0009,
FG-REPO-0010 MembershipConflict (`already_member`, `not_member`, `last_owner`,
`bot_member`), FG-REPO-0011 GroupRequestInvalid (`display_name_length`,
`description_length`, `nul_character`, `parent_not_group`, `owner_inactive`),
FG-REPO-0005 (`resource=group` / `namespace` / `user`, `reason=absent`), FG-SYS-0001
(`unknown_role`, `unexpected_dedup`).

**Runbooks**: `ops/runbooks/db-unavailable.md`, `db-bootstrap.md`,
`append-only-violation.md`, `outbox-sequencer-lag.md`, `job-dead-lettered.md`,
`job-queue-backlog.md`, `audit-seal-mismatch.md`, `audit-seal-backlog.md`,
`subscriber-lag.md`.

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
a plain event committed without an audit row in its transaction is
FG-DB-0013 (deferred trigger `events_audit_present`, `WHEN caused_by_seq IS NULL`).
The typed commands (step D) are its only callers; their state writes go through
`ChangeTx::step` (one statement at a time, each inside the deadline).

**State tables** (step D1, ADR-0016 §1): a `tables.toml` entry with
`kind = "audited"` declares `audit_resource` (literal or `=column`),
`audit_id_column`, `unaudited_columns` and `txn_audited_columns`. The table
carries the deferred constraint trigger `forge.audited_change_present` with
those four arguments and `UPDATE OF` every other column, `ENABLE ALWAYS`: a
change commits only with a succeeded audit row for the same resource in its
transaction (txn-audited columns alone: a succeeded audit row of the same
resource type; a changed
id or `=column` value: audit rows for the old and the new resource), else
FG-DB-0013 `reason=audited_change` (`audited_change_config`: the trigger
does not match the ledger). The id column is an integer column.
`bootstrap::check` compares each table and trigger with the ledger
(`bootstrap/audited.rs`; FG-DB-0009 `guard=audited_trigger`, `table`,
`detail`). The root gate validate-migrations requires the trigger at the end
of the migration sequence. TRUNCATE bypasses row triggers: an audited table
must grant it to no role — the table goes into `LATER_TABLES` and its exact
ACL into `expectations()` in `src/bootstrap/schema.rs` in the migration's
commit; doctor reports an audited table missing there (FG-DB-0009
`guard=privileges`, `detail=audited_table_unlisted`).
The ledger refuses resource columns that are unaudited or txn-audited, and a
column that is both.

**Schema** (ADR-0016 §1, step C2): `forge.events` (outbox), `forge.audit_log`,
`forge.event_feed_state`, `forge.event_subscribers`,
`forge.subscriber_dead_letters`, `forge.jobs`, and (step D1, migration `20261028100100`)
`forge.users`, `forge.namespaces`, `forge.namespace_tombstones` (all `kind = "audited"`;
`forge_ro` cannot read email, employee_number or external_id; tombstone targets are
`ON DELETE SET NULL` foreign keys and unaudited); the DDL-status decision is
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
RFC 8785 row bytes; details numbers other than safe integers as
`{"$n":"<text>"}`, keys starting with `$` get one more `$` — injective;
golden vector `tests/fixtures/audit_seal_v1_golden.py`, run with
`py -X utf8 -I`). `seal_sha256` covers `kind` and `key_id` (`seal::chain`).
`seal::seal_due(&db, &SealKeys, SealPolicy::WORKER)` is the body of the
`audit_seal` task of `forge worker` (a minute after the last seal, or when
more than 1000 rows wait; `seal_pending` seals without the policy — the
drain after `more`): try-lock `(1718579815, 8)` (`Busy` for the loser),
≤ 5000 rows per seal, hashing on the blocking pool, cancel-safe and
idempotent (a commit timeout's outcome is unknown; calling again is safe).
Errors: gap → FG-DB-0011 reason=gap; the feed-state row gone while rows
are numbered, or `audit_high_seq` below the last seal → reason=chain
(`feed_state_missing` / `audit_high_below_seal`); a row v1 cannot read
(details nested past 128) → FG-SYS-0001 `operation=seal.v1`.
`seal::verify_range` recomputes seals on the write pool (the sealed act-as
columns are not readable by forge_ro), each in its own SELECT-only
transaction that always rolls back under `search_path = pg_catalog,
pg_temp` — **not** READ ONLY / REPEATABLE READ: the root gate
db-write-boundary keeps `SET TRANSACTION` inside ChangeTx (limitation; C9's
full reverify may need the gate to allow it). Lists are capped at
`MAX_VERIFY_SEALS` (`Verified::truncated`). `forge doctor
audit.seal_verify` checks the last 24 h by `sealed_at`, which is not
authenticated (the full walk is C9's reverify); `warn` when seals exist
but no keys are configured. `seal::backlog` feeds `audit.seal_backlog`
(FG-DB-0015 past 5 minutes). The HMAC keys come from
`audit.seal_keys_file` (`forge_core::config::SealKeys`; not zeroized on
drop — deferred, new crate).

**Job queue** (ADR-0016 §7, step C6; `src/jobs/`): `forge.jobs` with fenced
leases, for internal work only — never one job per edge device (plan 01
§3.10: fleet-wide work is one job that walks its targets). `jobs::enqueue`
(write pool) or `ChangeTx::enqueue_job` (inside a change, rolled back with
it); a `dedup_key` is held per kind by queued, running and succeeded jobs —
a dead job frees it, so its work can be enqueued again (partial index
`jobs_dedup`, migration `20261027120000`). `jobs::claim` is ONE autocommit
`FOR UPDATE SKIP LOCKED` statement taking the oldest ready job (`running`,
`attempts + 1`, `worker_id`, `lease_until = now() + timeout + 30 s`) of the
given kinds only. `complete` / `fail` / `release` are fenced by `(id,
running, worker_id, attempts)` — a lost lease returns `Fenced(FG-DB-0007
reason=fenced)` and changes nothing. `fail` requeues after 10 s ×
2^(attempt − 1) (≤ 1 h) or deads (attempts used up, `Retry::Never`);
`release` hands a job back at shutdown (ready now, attempt not counted).
`jobs::reap` is ONE statement for up to 1000 expired leases (dead with
`last_error_code = FG-DB-0007` when the attempts are used up, else requeued
with the backoff). `JobRunner` (`JobHandler` per kind, one job at a time —
a head-of-line limit to revisit before C8 / retention jobs; handler in its
own task under `timeout_ms`: `execution_timeout` → FG-DB-0007 and retry,
a panic → FG-SYS-0001 and retry; after an abort it waits up to 5 s for the
handler to end before settling, else `abort_pending`; `run_once_until`
gives a stopped job a grace, then releases it — `released_on_shutdown`; a
dropped run aborts the task, the lease expires, reap requeues it) logs the
handler's failure, dead jobs (FG-DB-0006) and lost leases (FG-DB-0007) with
the job's origin request id. Delivery is at-least-once: handler side
effects must be idempotent or fenced by `lease.attempt`. `jobs::health`
feeds `forge doctor` (`jobs.dead` → FG-DB-0006; `jobs.expired_leases`,
`jobs.queue_depth` → FG-DB-0017 JobQueueBacklog, counted per kind via
`jobs_ready_kind`). `forge worker` runs the runner (1 s idle poll, 10 s
shutdown grace) and the reaper (30 s, also at startup).
**Subscribers** (ADR-0016 §6, §7, step C5): `subscribers::Subscriber` (name,
`Ordering::Ordered` / `Independent`, `handle(&SourceEvent, &mut DerivedTx)`),
`Registry`, `register(&db, sub, Start::Head)`, `run_once(&db, sub)` — one event
(`last_seq + 1`) per write-pool transaction: `SELECT … FOR UPDATE NOWAIT` (55P03 =
`Progress::Busy`, debug log only), the handler inside a savepoint (20 s), then
`last_seq` / `last_txid` advance monotonically. Five failures in a row stop an
ordered subscriber (FG-DB-0008 `stuck`, retried, never skipped) or quarantine
the event of an independent one (`subscriber_dead_letters`, `quarantined`);
the derivation limits (depth 8, 100 per root) are `fanout_limit` at once.
Counted as failures too: an undecodable event (FG-SYS-0001 `decode`), a
failed savepoint RELEASE, and derived writes refused at COMMIT (FG-DB-0013
`derived_*`, counted in a fresh transaction); the stored code is the
handler's own (`src/subscribers/count.rs`). Not counted (step errors,
retried with backoff): DB failures, `subscriber_not_registered`,
`ordering_mismatch`, `event_missing`, `cursor_moved` (`cursor.rs`).
`DerivedTx` writes derived tables (`conn()` — a trust boundary, see the
`Subscriber` docs) and derived events (`emit`: actor
`system`, the source request id, lineage set; no audit row). Migration
`20261027140000` checks derived events (`events_lineage` BEFORE INSERT,
`events_derived_progress` deferred; XF002 → FG-DB-0013; an unknown
CONSTRAINT name is `xf002_unknown`). `Dispatcher` runs
every registered subscriber for `forge worker` (a round starts no step
after `ROUND_BUDGET` = 30 s, then `more`) and waits on the crate-private
`Wake` (LISTEN
`forge_events_sequenced` on the dedicated connection `Db::listen_options` — no
statement / idle timeout, server-side TCP keepalive, a bounded `SELECT 1` at
every 5 s poll). `states` / `check_lag` / `dead_letters` feed `forge doctor`
(`subscribers.lag`, `subscribers.dead_letters`).
