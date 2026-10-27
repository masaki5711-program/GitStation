//! Migration tests: each runs in its own fresh database (ADR-0011) with the
//! fixture migrators under `tests/fixtures/migrator/` (not `migrations/`,
//! so the immutability hook does not apply to them).

use std::path::Path;
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use sqlx::migrate::{MigrateError, Migrator};

use super::*;
use crate::test_support::*;

const LONG: Duration = Duration::from_secs(60);

async fn fixture(name: &str) -> Migrator {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/migrator")
        .join(name);
    Migrator::new(dir).await.expect("fixture migrator")
}

/// The migration connection of a fresh database (forge_migrator → forge_owner).
async fn migrator(t: &FreshDb) -> MigratorConn {
    Db::connect_migrator(&t.migrate_config(&[]).db)
        .await
        .expect("migrator connection")
}

/// Open, migrate, close.
async fn run(t: &FreshDb, m: &Migrator, timeout: Duration) -> Result<Vec<i64>, ForgeError> {
    let mut mc = migrator(t).await;
    let result = migrate(&mut mc, m, timeout).await;
    mc.close().await;
    result
}

async fn table_exists(db: &Db, table: &str) -> bool {
    sqlx::query_scalar::<_, Option<String>>("SELECT to_regclass($1)::text")
        .bind(table)
        .fetch_one(db.pool().executor())
        .await
        .expect("to_regclass")
        .is_some()
}

#[tokio::test]
async fn migrate_applies_pending_in_order_and_is_idempotent() {
    let t = FreshDb::new().await;
    let v1 = fixture("v1").await;
    let applied = run(&t, &v1, LONG).await.expect("migrate");
    assert_eq!(applied, vec![20261026000001, 20261026000002]);
    assert!(table_exists(&t.db, "fixture_b").await);
    let again = run(&t, &v1, LONG).await.expect("second run");
    assert!(again.is_empty(), "nothing left to apply: {again:?}");
    let st = check_schema(&t.db, &v1).await.expect("compatible");
    assert_eq!(st.applied, vec![20261026000001, 20261026000002]);
    t.drop_now().await;
}

#[tokio::test]
async fn status_is_read_only() {
    let t = FreshDb::new().await;
    let st = status(&t.db, &fixture("v1").await).await.expect("status");
    assert_eq!(st.pending, vec![20261026000001, 20261026000002]);
    assert!(
        !table_exists(&t.db, "_sqlx_migrations").await,
        "status must not create the bookkeeping table"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn pending_migrations_are_schema_incompatible() {
    let t = FreshDb::new().await;
    let err = check_schema(&t.db, &fixture("v1").await)
        .await
        .expect_err("pending");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "pending");
    assert!(
        err.to_string()
            .contains("versions=20261026000001,20261026000002"),
        "{err}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_database_newer_than_the_binary_is_db_ahead_and_migrate_refuses() {
    let t = FreshDb::new().await;
    run(&t, &fixture("v2").await, LONG).await.expect("v2");
    let v1 = fixture("v1").await;
    let err = check_schema(&t.db, &v1).await.expect_err("db ahead");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "db_ahead");
    assert!(err.to_string().contains("versions=20261026000003"), "{err}");
    let err = run(&t, &v1, LONG).await.expect_err("refuses");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "db_ahead");
    t.drop_now().await;
}

#[tokio::test]
async fn an_edited_applied_migration_is_checksum_mismatch() {
    let t = FreshDb::new().await;
    run(&t, &fixture("v1").await, LONG).await.expect("v1");
    let edited = fixture("v1_edited").await;
    let err = check_schema(&t.db, &edited).await.expect_err("edited");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "checksum_mismatch");
    assert!(err.to_string().contains("versions=20261026000002"), "{err}");
    let err = run(&t, &edited, LONG).await.expect_err("refuses");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "checksum_mismatch");
    t.drop_now().await;
}

#[tokio::test]
async fn a_failing_migration_is_migration_failed_and_leaves_no_partial_version() {
    let t = FreshDb::new().await;
    let err = run(&t, &fixture("failing").await, LONG)
        .await
        .expect_err("division by zero");
    assert_reason(&err, ErrorCode::MigrationFailed, "sql_failed");
    let text = chain(&err);
    assert!(text.contains("version=20261026000002"), "{text}");
    assert!(text.contains("sqlstate=22012"), "{text}");
    assert!(
        !table_exists(&t.db, "fixture_b").await,
        "the failing migration's transaction was rolled back"
    );
    let st = status(&t.db, &fixture("failing").await)
        .await
        .expect("status");
    assert_eq!(st.applied, vec![20261026000001]);
    assert_eq!(st.pending, vec![20261026000002]);
    assert!(st.dirty.is_empty(), "{st:?}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_dirty_record_is_schema_incompatible() {
    let t = FreshDb::new().await;
    let v1 = fixture("v1").await;
    run(&t, &v1, LONG).await.expect("v1");
    sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = 20261026000002")
        .execute(&mut t.superuser().await)
        .await
        .expect("mark dirty");
    let err = check_schema(&t.db, &v1).await.expect_err("dirty");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "dirty");
    let err = run(&t, &v1, LONG).await.expect_err("refuses");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "dirty");
    t.drop_now().await;
}

#[tokio::test]
async fn concurrent_migrate_runs_are_serialized() {
    let t = FreshDb::new().await;
    let v2 = fixture("v2").await;
    let (a, b) = tokio::join!(run(&t, &v2, LONG), run(&t, &v2, LONG));
    let (a, b) = (a.expect("run a"), b.expect("run b"));
    let mut all: Vec<i64> = a.iter().chain(b.iter()).copied().collect();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all, vec![20261026000001, 20261026000002, 20261026000003]);
    let st = check_schema(&t.db, &v2).await.expect("compatible");
    assert_eq!(st.applied.len(), 3);
    t.drop_now().await;
}

#[tokio::test]
async fn the_binary_migrator_matches_an_empty_database() {
    // No product migrations yet: a fresh database is compatible.
    let t = FreshDb::new().await;
    check_schema(&t.db, &MIGRATOR).await.expect("compatible");
    t.drop_now().await;
}

#[tokio::test]
async fn migrations_run_with_their_own_statement_timeout_on_a_dedicated_connection() {
    let t = FreshDb::new().await;
    let err = run(&t, &fixture("slow").await, Duration::from_millis(500))
        .await
        .expect_err("pg_sleep(3) exceeds the migration timeout");
    assert_reason(&err, ErrorCode::MigrationFailed, "sql_failed");
    let text = chain(&err);
    assert!(text.contains("version=20261026000002"), "{text}");
    assert!(text.contains("sqlstate=57014"), "{text}");
    // The pool's sessions keep the normal statement timeout.
    let mut conn = t.db.acquire().await.expect("pooled connection");
    let value: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&mut *conn)
        .await
        .expect("show");
    assert_eq!(value, "15s");
    drop(conn);
    t.drop_now().await;
}

#[tokio::test]
async fn a_migration_run_blocked_by_another_holder_of_the_lock_is_locked() {
    let t = FreshDb::new().await;
    let v1 = fixture("v1").await;
    // sqlx's migration lock: pg_advisory_lock(<id derived from the db name>);
    // holding every advisory lock is impractical, so take the one sqlx uses.
    let mut holder = t.db.acquire().await.expect("holder");
    let dbname: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *holder)
        .await
        .expect("db name");
    let lock_id = sqlx_lock_id(&dbname);
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(lock_id)
        .execute(&mut *holder)
        .await
        .expect("hold the migration lock");
    // A short lock wait for this run only.
    let cfg = t.migrate_config(&[("FORGE_DATABASE_LOCK_TIMEOUT_MS", "300")]);
    let mut short = Db::connect_migrator(&cfg.db).await.expect("connect");
    let err = migrate(&mut short, &v1, LONG).await.expect_err("lock held");
    assert_reason(&err, ErrorCode::MigrationFailed, "locked");
    drop(holder);
    short.close().await;
    t.drop_now().await;
}

/// The advisory lock id sqlx-postgres 0.8 derives from the database name
/// (`generate_lock_id`: 0x3d32ad9e * crc32(name)).
fn sqlx_lock_id(database_name: &str) -> i64 {
    const CRC_IEEE: crc32::Table = crc32::Table::new();
    0x3d32ad9e * i64::from(CRC_IEEE.checksum(database_name.as_bytes()))
}

/// Minimal CRC-32 (IEEE), enough to reproduce sqlx's lock id in a test.
mod crc32 {
    pub struct Table([u32; 256]);
    impl Table {
        pub const fn new() -> Self {
            let mut t = [0u32; 256];
            let mut i = 0;
            while i < 256 {
                let mut c = i as u32;
                let mut k = 0;
                while k < 8 {
                    c = if c & 1 == 1 {
                        0xEDB8_8320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                    k += 1;
                }
                t[i] = c;
                i += 1;
            }
            Self(t)
        }
        pub fn checksum(&self, bytes: &[u8]) -> u32 {
            let mut c = !0u32;
            for b in bytes {
                c = self.0[((c ^ u32::from(*b)) & 0xFF) as usize] ^ (c >> 8);
            }
            !c
        }
    }
}

#[test]
fn every_migrate_error_maps_to_a_registered_code_and_reason() {
    for (e, code, reason) in [
        (
            MigrateError::Dirty(7),
            ErrorCode::SchemaIncompatible,
            "dirty",
        ),
        (
            MigrateError::VersionMismatch(7),
            ErrorCode::SchemaIncompatible,
            "checksum_mismatch",
        ),
        (
            MigrateError::VersionMissing(7),
            ErrorCode::SchemaIncompatible,
            "db_ahead",
        ),
        (
            MigrateError::Source("bad dir".into()),
            ErrorCode::MigrationFailed,
            "source",
        ),
        (
            MigrateError::VersionTooOld(1, 2),
            ErrorCode::MigrationFailed,
            "driver",
        ),
    ] {
        let err = migrate_error(e);
        assert_reason(&err, code, reason);
    }
}

#[tokio::test]
async fn migrator_connection_runs_as_forge_owner_and_owns_what_it_creates() {
    let t = FreshDb::new().await;
    let mut mc = migrator(&t).await;
    let (current, session, app): (String, String, String) = sqlx::query_as(
        "SELECT current_user::text, session_user::text, current_setting('application_name')",
    )
    .fetch_one(&mut mc.conn)
    .await
    .expect("whoami");
    assert_eq!(
        (current.as_str(), session.as_str(), app.as_str()),
        ("forge_owner", "forge_migrator", "forge-migrate")
    );
    migrate(&mut mc, &fixture("v1").await, LONG)
        .await
        .expect("v1");
    mc.close().await;
    let owners: Vec<String> = sqlx::query_scalar(
        "SELECT tableowner::text FROM pg_tables \
         WHERE tablename IN ('fixture_a', 'fixture_b', '_sqlx_migrations')",
    )
    .fetch_all(t.db.pool().executor())
    .await
    .expect("owners");
    assert_eq!(owners, ["forge_owner"; 3]);
    // The read pool sees what is applied (default privileges of the bootstrap).
    let st = check_schema(&t.db, &fixture("v1").await)
        .await
        .expect("compatible");
    assert_eq!(st.applied.len(), 2);
    t.drop_now().await;
}

#[tokio::test]
async fn a_migrate_login_that_cannot_become_owner_is_config_invalid_wrong_role() {
    let t = FreshDb::new().await;
    // forge_app is not a member of forge_owner.
    let pw = PasswordFile::new(APP_PASSWORD);
    let (url, path) = (url_as("forge_app", &t.name), pw.path());
    let cfg = t.migrate_config(&[
        ("FORGE_DATABASE_MIGRATE_URL", url.as_str()),
        ("FORGE_DATABASE_MIGRATE_PASSWORD_FILE", path.as_str()),
    ]);
    let err = Db::connect_migrator(&cfg.db)
        .await
        .expect_err("not a member");
    assert_reason(&err, ErrorCode::ConfigInvalid, "wrong_role");
    let text = err.to_string();
    assert!(text.contains("config_key=database.migrate_url"), "{text}");
    assert!(text.contains("role_check=not_member"), "{text}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_superuser_migrate_login_is_refused() {
    let t = FreshDb::new().await;
    let pw = PasswordFile::new("forge_dev");
    let (url, path) = (url_with_db(&t.name), pw.path());
    let cfg = t.migrate_config(&[
        ("FORGE_DATABASE_MIGRATE_URL", url.as_str()),
        ("FORGE_DATABASE_MIGRATE_PASSWORD_FILE", path.as_str()),
    ]);
    let err = Db::connect_migrator(&cfg.db).await.expect_err("superuser");
    assert_reason(&err, ErrorCode::ConfigInvalid, "wrong_role");
    assert!(err.to_string().contains("role_check=superuser"), "{err}");
    t.drop_now().await;
}

#[test]
fn a_missing_migrate_url_is_config_invalid() {
    let cfg = test_config("postgres://forge_app@127.0.0.1:1/forge", "x", &[]);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let err = rt
        .block_on(Db::connect_migrator(&cfg.db))
        .expect_err("no migrate url");
    assert_reason(&err, ErrorCode::ConfigInvalid, "missing");
    assert!(err.to_string().contains("config_key=database.migrate_url"));
}
