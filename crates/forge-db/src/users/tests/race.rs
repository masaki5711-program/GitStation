//! The user commands under concurrency and deadlines: two connections at
//! once, against a migrated database.

use std::time::Duration;

use super::*;

/// Wait (bounded) until a backend of this test's database waits on an
/// advisory lock (other tests run in other databases of the cluster).
async fn until_waiting_on_advisory_lock(t: &FreshDb) {
    let mut su = t.superuser().await;
    for _ in 0..500 {
        let waiting = count(
            &mut su,
            "SELECT count(*) FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND wait_event = 'advisory' \
             AND datname = current_database()",
        )
        .await;
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nobody waited on the path lock within 10 s");
}

/// A tombstone that commits while a create of the same path is in flight
/// blocks that create: the create waits on the path lock and then sees it.
#[tokio::test]
async fn a_tombstone_written_concurrently_blocks_the_create() {
    let t = FreshDb::migrated().await;
    let mut writer = t.db.write_pool().begin().await.expect("writer");
    // The same lock every tombstone writer (D5) takes.
    crate::namespaces::lock_path(&mut writer, "ghost")
        .await
        .expect("the writer takes the path lock");
    sqlx::raw_sql(&format!(
        "{}; {}",
        audit("user", 999, "succeeded"),
        tombstone("ghost", 999)
    ))
    .execute(&mut *writer)
    .await
    .expect("tombstone, uncommitted");

    let ghost = NewUser::human("ghost", "Ghost", "ghost@example.test");
    let race_ctx = ctx("req-race");
    let creating = create(&t.db, &race_ctx, &ghost);
    let committing = async {
        until_waiting_on_advisory_lock(&t).await;
        writer.commit().await.expect("the tombstone commits");
    };
    let (created, ()) = tokio::join!(creating, committing);
    let err = created.expect_err("the tombstone wins");
    assert_reason(&err, ErrorCode::NamespacePathTaken, "tombstoned");
    let mut su = t.superuser().await;
    assert_eq!(rows_of(&mut su, "users").await, 0);
    assert_eq!(rows_of(&mut su, "namespaces").await, 0);
    assert!(audit_rows(&mut su, "req-race").await.is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn two_concurrent_disables_write_one_audit_row() {
    let t = FreshDb::migrated().await;
    let created = create_alice(&t, "req-create").await;
    let (ctx_a, ctx_b) = (ctx("req-disable-a"), ctx("req-disable-b"));
    let (a, b) = tokio::join!(
        disable(&t.db, &ctx_a, created.user_id),
        disable(&t.db, &ctx_b, created.user_id),
    );
    let mut outcomes = [a.expect("a"), b.expect("b")];
    outcomes.sort_by_key(|o| *o == DisableOutcome::AlreadyDisabled);
    assert_eq!(
        outcomes,
        [DisableOutcome::Disabled, DisableOutcome::AlreadyDisabled]
    );
    let mut su = t.superuser().await;
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'user.disabled'"
        )
        .await,
        1
    );
    t.drop_now().await;
}

/// A step counts against the change's deadline: a slow statement rolls the
/// whole change back and frees the connection.
#[tokio::test]
async fn a_step_past_the_deadline_rolls_the_change_back() {
    let t = FreshDb::migrated().await;
    let mut tx = ChangeTx::begin_within(&t.db, &ctx("req-slow"), Duration::from_millis(2000))
        .await
        .expect("begin");
    let err = tx
        .step(|c| async move {
            sqlx::query("SELECT pg_sleep(30)")
                .execute(c)
                .await
                .map_err(|e| query_error("test.sleep", e))
        })
        .await
        .expect_err("deadline");
    assert_reason(&err, ErrorCode::DbQueryFailed, "change_timeout");
    assert!(err.to_string().contains("timeout_ms=2000"), "{err}");
    drop(tx);
    // The pool is usable and the next change commits.
    create_alice(&t, "req-after").await;
    let mut su = t.superuser().await;
    assert!(audit_rows(&mut su, "req-slow").await.is_empty());
    t.drop_now().await;
}
