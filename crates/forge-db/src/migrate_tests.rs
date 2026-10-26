//! Migration tests: each runs in its own fresh database (ADR-0011) with the
//! fixture migrators under `tests/fixtures/migrator/` (not `migrations/`,
//! so the immutability hook does not apply to them).

use std::path::Path;
use std::time::Duration;

use forge_errors::ErrorCode;
use sqlx::migrate::Migrator;

use super::*;
use crate::test_support::*;

const LONG: Duration = Duration::from_secs(60);

async fn fixture(name: &str) -> Migrator {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/migrator")
        .join(name);
    Migrator::new(dir).await.expect("fixture migrator")
}

async fn table_exists(db: &Db, table: &str) -> bool {
    sqlx::query_scalar::<_, Option<String>>("SELECT to_regclass($1)::text")
        .bind(table)
        .fetch_one(db.pool())
        .await
        .expect("to_regclass")
        .is_some()
}

#[tokio::test]
async fn migrate_applies_pending_in_order_and_is_idempotent() {
    let t = FreshDb::new().await;
    let v1 = fixture("v1").await;
    let applied = migrate(&t.db, &v1, LONG).await.expect("migrate");
    assert_eq!(applied, vec![20261026000001, 20261026000002]);
    assert!(table_exists(&t.db, "fixture_b").await);
    let again = migrate(&t.db, &v1, LONG).await.expect("second run");
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
    migrate(&t.db, &fixture("v2").await, LONG)
        .await
        .expect("v2");
    let v1 = fixture("v1").await;
    let err = check_schema(&t.db, &v1).await.expect_err("db ahead");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "db_ahead");
    assert!(err.to_string().contains("versions=20261026000003"), "{err}");
    let err = migrate(&t.db, &v1, LONG).await.expect_err("refuses");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "db_ahead");
    t.drop_now().await;
}

#[tokio::test]
async fn an_edited_applied_migration_is_checksum_mismatch() {
    let t = FreshDb::new().await;
    migrate(&t.db, &fixture("v1").await, LONG)
        .await
        .expect("v1");
    let edited = fixture("v1_edited").await;
    let err = check_schema(&t.db, &edited).await.expect_err("edited");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "checksum_mismatch");
    assert!(err.to_string().contains("versions=20261026000002"), "{err}");
    let err = migrate(&t.db, &edited, LONG).await.expect_err("refuses");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "checksum_mismatch");
    t.drop_now().await;
}

#[tokio::test]
async fn a_failing_migration_is_migration_failed_and_leaves_no_partial_version() {
    let t = FreshDb::new().await;
    let err = migrate(&t.db, &fixture("failing").await, LONG)
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
    migrate(&t.db, &v1, LONG).await.expect("v1");
    sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = 20261026000002")
        .execute(t.db.pool())
        .await
        .expect("mark dirty");
    let err = check_schema(&t.db, &v1).await.expect_err("dirty");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "dirty");
    let err = migrate(&t.db, &v1, LONG).await.expect_err("refuses");
    assert_reason(&err, ErrorCode::SchemaIncompatible, "dirty");
    t.drop_now().await;
}

#[tokio::test]
async fn concurrent_migrate_runs_are_serialized() {
    let t = FreshDb::new().await;
    let v2 = fixture("v2").await;
    let (a, b) = tokio::join!(migrate(&t.db, &v2, LONG), migrate(&t.db, &v2, LONG));
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
