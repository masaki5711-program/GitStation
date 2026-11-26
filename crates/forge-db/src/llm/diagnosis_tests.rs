//! The diagnosis's recent changes (plan 19 L5-7) on a migrated database.

use super::*;
use crate::test_support::FreshDb;

#[tokio::test]
async fn read_only_db_role() {
    let t = FreshDb::migrated().await;
    let changes = recent_changes(&t.db, 24 * 60 * 60)
        .await
        .expect("recent changes");
    assert_eq!(changes.role, "forge_ro");
    // The fresh database was just migrated: every migration is recent.
    assert!(!changes.migrations.is_empty());
    assert!(changes.migrations.len() <= MAX_ROWS as usize);
    assert!(changes.migrations.iter().all(|m| m.success));
    assert!(changes.deployments.is_empty());
}

#[tokio::test]
async fn an_empty_window_reads_nothing() {
    let t = FreshDb::migrated().await;
    let changes = recent_changes(&t.db, 0).await.expect("recent changes");
    assert!(changes.migrations.is_empty());
    assert!(changes.deployments.is_empty());
}
