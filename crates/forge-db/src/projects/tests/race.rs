//! `projects::create` under concurrency: two connections, one interleaving
//! pinned per test (the style of `users/tests/race.rs`). Each test turns red
//! when the lock it proves is dropped: the creator's `FOR SHARE`
//! (`check_creator`) or the path lock (`namespaces::lock_path`) — the
//! waiting helper panics when nobody waits.

use std::time::Duration;

use super::*;

/// The statements the tests wait for (fragments of the commands' SQL).
const CREATOR_SHARE_WAIT: &str = "SELECT kind, state FROM forge.users WHERE id = $1 FOR SHARE";
const DISABLE_ROW_WAIT: &str = "is_admin FROM forge.users WHERE id = $1 FOR UPDATE";
const PATH_LOCK_WAIT: &str = "pg_advisory_xact_lock(hashtextextended('namespace:'";

/// Wait (bounded) until a backend of this test's database runs a statement
/// containing `statement` and waits on a lock. Other tests use other
/// databases.
async fn until_waiting(t: &FreshDb, statement: &str) {
    let mut su = t.superuser().await;
    for _ in 0..500 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
             AND datname = current_database() AND strpos(query, $1) > 0",
        )
        .bind(statement)
        .fetch_one(&mut su)
        .await
        .expect("pg_stat_activity");
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no statement containing {statement:?} waited on a lock within 10 s");
}

/// `acme` owned by alice; bob (no group role) creates in it, so only the
/// creator check locks bob's row.
async fn acme_and_bob(t: &FreshDb) -> (CreatedGroup, UserId) {
    let alice = user(t, "alice").await;
    let bob = user(t, "bob").await;
    let acme = group(t, "acme", None, alice.user_id).await;
    (acme, bob.user_id)
}

/// Pinned, disable first: an uncommitted disable of bob holds his row; the
/// create waits on the creator's `FOR SHARE`, then reads the committed row
/// and refuses (`creator_inactive`). Without `FOR SHARE` the create would
/// not wait, would read bob as active and commit a project whose only
/// direct Owner is disabled.
#[tokio::test]
async fn a_create_waits_for_a_disable_of_its_creator_and_then_refuses() {
    let t = FreshDb::migrated().await;
    let (acme, bob) = acme_and_bob(&t).await;
    let mut writer = t.db.write_pool().begin().await.expect("writer");
    sqlx::raw_sql(&format!(
        "{}; UPDATE forge.users SET state = 'disabled' WHERE id = {}",
        audit("user", bob.get(), "succeeded"),
        bob.get()
    ))
    .execute(&mut *writer)
    .await
    .expect("disable bob, uncommitted");
    let new = project("web", acme.namespace_id, bob);
    let race_ctx = ctx("req-race");
    let creating = create(&t.db, &race_ctx, &new);
    let committing = async {
        until_waiting(&t, CREATOR_SHARE_WAIT).await;
        writer.commit().await.expect("bob is disabled");
    };
    let (created, ()) = tokio::join!(creating, committing);
    let err = created.expect_err("bob is disabled now");
    assert_reason(&err, ErrorCode::ProjectRequestInvalid, "creator_inactive");
    let mut su = t.superuser().await;
    assert_eq!(written(&mut su).await, [0; 7]);
    t.drop_now().await;
}

/// Pinned, create first: the create holds bob's row `FOR SHARE` and is kept
/// open by a blocker on the project's path lock; the real `users::disable`
/// of bob waits for his row and runs only after the create committed. The
/// result is the documented order: a project, then its creator disabled
/// (the project last-owner policy is D4 MR4's). Without `FOR SHARE` the
/// disable would not wait (the helper panics).
#[tokio::test]
async fn a_disable_of_the_creator_waits_for_the_create_to_commit() {
    let t = FreshDb::migrated().await;
    let (acme, bob) = acme_and_bob(&t).await;
    let mut hold = t.superuser().await;
    sqlx::raw_sql(
        "BEGIN; SELECT pg_advisory_xact_lock(hashtextextended('namespace:' || 'acme/web', 0))",
    )
    .execute(&mut hold)
    .await
    .expect("blocker takes the path lock");
    let new = project("web", acme.namespace_id, bob);
    let (create_ctx, disable_ctx) = (ctx("req-create"), ctx("req-disable"));
    let creating = create(&t.db, &create_ctx, &new);
    let disabling = async {
        until_waiting(&t, PATH_LOCK_WAIT).await;
        users::disable(&t.db, &disable_ctx, bob).await
    };
    let releasing = async {
        until_waiting(&t, PATH_LOCK_WAIT).await;
        until_waiting(&t, DISABLE_ROW_WAIT).await;
        sqlx::raw_sql("ROLLBACK")
            .execute(&mut hold)
            .await
            .expect("release the blocker");
    };
    let (created, disabled, ()) = tokio::join!(creating, disabling, releasing);
    let created = created.expect("the create commits first");
    assert_eq!(
        disabled.expect("then the disable"),
        users::DisableOutcome::Disabled
    );
    let mut su = t.superuser().await;
    let order: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM forge.audit_log WHERE request_id IN ('req-create', 'req-disable') \
         ORDER BY id",
    )
    .fetch_all(&mut su)
    .await
    .expect("audit order");
    assert_eq!(order, ["project.created", "user.disabled"]);
    assert!(trio(&mut su, created.project_id).await.is_some());
    t.drop_now().await;
}

/// Pinned: a tombstone that commits while a create of the same path is in
/// flight blocks that create — the create waits on the path lock and then
/// sees it. Without `lock_path` the create would not wait (the helper
/// panics) and would miss the uncommitted tombstone.
#[tokio::test]
async fn a_tombstone_written_concurrently_blocks_the_project_create() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let mut writer = t.db.write_pool().begin().await.expect("writer");
    // The same lock every tombstone writer (D5) takes.
    crate::namespaces::lock_path(&mut writer, "alice/ghost")
        .await
        .expect("the writer takes the path lock");
    sqlx::raw_sql(&format!(
        "{}; INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, cause_id) \
         VALUES ('alice/ghost', 'deleted', 'project', 999)",
        audit("project", 999, "succeeded"),
    ))
    .execute(&mut *writer)
    .await
    .expect("tombstone, uncommitted");
    let new = project("ghost", alice.namespace_id, alice.user_id);
    let race_ctx = ctx("req-race");
    let creating = create(&t.db, &race_ctx, &new);
    let committing = async {
        until_waiting(&t, PATH_LOCK_WAIT).await;
        writer.commit().await.expect("the tombstone commits");
    };
    let (created, ()) = tokio::join!(creating, committing);
    let err = created.expect_err("the tombstone wins");
    assert_reason(&err, ErrorCode::NamespacePathTaken, "tombstoned");
    let mut su = t.superuser().await;
    assert_eq!(written(&mut su).await, [0; 7]);
    t.drop_now().await;
}
