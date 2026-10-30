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

// ── D3 MR2: the disable guard under concurrency (ADR-0019 §11) ──────────

/// Wait (bounded) until a backend of this test's database waits on any lock.
async fn until_someone_waits(t: &FreshDb) {
    let mut su = t.superuser().await;
    for _ in 0..500 {
        let waiting = count(
            &mut su,
            "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
             AND datname = current_database()",
        )
        .await;
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nobody waited on a lock within 10 s");
}

async fn active(su: &mut PgConnection, user: UserId) -> bool {
    let state: String = sqlx::query_scalar("SELECT state FROM forge.users WHERE id = $1")
        .bind(user.get())
        .fetch_one(su)
        .await
        .expect("state");
    state == "active"
}

/// Two administrators disable each other at the same time: the advisory
/// lock `users.admin_set` makes the second see the first, so one admin stays.
/// The interleaving is up to the scheduler, so this is the end-to-end smoke
/// test; the next test pins the interleaving that needs the lock (it fails
/// when `disable` does not take it).
#[tokio::test]
async fn two_admins_disabling_each_other_at_once_leave_one_admin() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, true).await;
    let bob = account(&t, "bob", UserKind::Human, true).await;
    let (ctx_a, ctx_b) = (ctx("req-disable-alice"), ctx("req-disable-bob"));
    let (a, b) = tokio::join!(disable(&t.db, &ctx_a, alice), disable(&t.db, &ctx_b, bob),);
    let refused: Vec<_> = [a, b].into_iter().filter_map(Result::err).collect();
    assert_eq!(refused.len(), 1, "exactly one is refused: {refused:?}");
    assert_reason(&refused[0], ErrorCode::AdministrationConflict, "last_admin");
    let mut su = t.superuser().await;
    let still = [active(&mut su, alice).await, active(&mut su, bob).await];
    assert_eq!(
        still.iter().filter(|a| **a).count(),
        1,
        "one admin stays: {still:?}"
    );
    t.drop_now().await;
}

/// A disable waits for an uncommitted change that holds `users.admin_set`
/// (here: disabling the other admin) and then counts it.
#[tokio::test]
async fn a_disable_waits_on_the_admin_set_lock_and_then_sees_the_other_disable() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, true).await;
    let bob = account(&t, "bob", UserKind::Human, true).await;
    let mut writer = t.db.write_pool().begin().await.expect("writer");
    lock_admin_set(&mut writer)
        .await
        .expect("the writer takes the admin-set lock");
    sqlx::raw_sql(&format!(
        "{}; UPDATE forge.users SET state = 'disabled' WHERE id = {}",
        audit("user", bob.get(), "succeeded"),
        bob.get()
    ))
    .execute(&mut *writer)
    .await
    .expect("disable bob, uncommitted");
    let race_ctx = ctx("req-disable-alice");
    let disabling = disable(&t.db, &race_ctx, alice);
    let committing = async {
        until_waiting_on_advisory_lock(&t).await;
        writer.commit().await.expect("bob is disabled");
    };
    let (disabled, ()) = tokio::join!(disabling, committing);
    let err = disabled.expect_err("bob no longer counts");
    assert_reason(&err, ErrorCode::AdministrationConflict, "last_admin");
    let mut su = t.superuser().await;
    assert!(active(&mut su, alice).await);
    t.drop_now().await;
}

/// A bootstrap waits for an uncommitted change that holds `users.admin_set`
/// (here: making bob an administrator) and then refuses with `admin_exists`.
#[tokio::test]
async fn a_bootstrap_waits_on_the_admin_set_lock_and_then_sees_the_new_admin() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, false).await;
    let bob = account(&t, "bob", UserKind::Human, false).await;
    let mut writer = t.db.write_pool().begin().await.expect("writer");
    lock_admin_set(&mut writer)
        .await
        .expect("the writer takes the admin-set lock");
    sqlx::raw_sql(&format!(
        "{}; UPDATE forge.users SET is_admin = true WHERE id = {}",
        audit("user", bob.get(), "succeeded"),
        bob.get()
    ))
    .execute(&mut *writer)
    .await
    .expect("promote bob, uncommitted");
    let race_ctx = ctx("req-boot-alice");
    let promote = Bootstrap::Promote(alice);
    let bootstrapping = bootstrap_admin(&t.db, &race_ctx, &promote);
    let committing = async {
        until_waiting_on_advisory_lock(&t).await;
        writer.commit().await.expect("bob is an admin");
    };
    let (done, ()) = tokio::join!(bootstrapping, committing);
    let err = done.expect_err("bob is an admin now");
    assert_reason(&err, ErrorCode::AdministrationConflict, "admin_exists");
    t.drop_now().await;
}

/// The other Owner is being removed (uncommitted, holding the group row and
/// this owner's user row FOR SHARE, as `groups::remove_member` does): the
/// disable waits for the user row and then no longer counts the removed
/// Owner, so it is refused. The disable never locks the group row.
#[tokio::test]
async fn an_owner_disable_waits_for_a_concurrent_owner_removal() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, false).await;
    let bob = account(&t, "bob", UserKind::Human, false).await;
    let acme = super::guard::top_level(&t, "acme", alice).await;
    crate::groups::add_member(
        &t.db,
        &ctx("req-add-bob"),
        acme.group_id,
        bob,
        forge_core::Role::Owner,
    )
    .await
    .expect("bob owner");
    let g = acme.group_id.get();
    let mut removing = t.db.write_pool().begin().await.expect("removing");
    sqlx::raw_sql(&format!(
        "SELECT 1 FROM forge.groups WHERE id = {g} FOR UPDATE; \
         SELECT 1 FROM forge.users WHERE id = {} FOR SHARE; \
         {}; DELETE FROM forge.group_members WHERE group_id = {g} AND user_id = {}",
        alice.get(),
        audit("group", g, "succeeded"),
        bob.get()
    ))
    .execute(&mut *removing)
    .await
    .expect("remove bob, uncommitted");
    let race_ctx = ctx("req-disable-alice");
    let disabling = disable(&t.db, &race_ctx, alice);
    let committing = async {
        until_someone_waits(&t).await;
        removing.commit().await.expect("bob is removed");
    };
    let (disabled, ()) = tokio::join!(disabling, committing);
    let err = disabled.expect_err("bob no longer owns acme");
    assert_reason(&err, ErrorCode::AdministrationConflict, "last_owner");
    let mut su = t.superuser().await;
    assert!(active(&mut su, alice).await);
    t.drop_now().await;
}

/// The two Owners of a top-level group disable each other at the same time:
/// exactly one is refused (end-to-end smoke test, like the admin one).
#[tokio::test]
async fn two_owners_disabling_each_other_at_once_leave_one_owner() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, false).await;
    let bob = account(&t, "bob", UserKind::Human, false).await;
    let acme = super::guard::top_level(&t, "acme", alice).await;
    crate::groups::add_member(
        &t.db,
        &ctx("req-add-bob"),
        acme.group_id,
        bob,
        forge_core::Role::Owner,
    )
    .await
    .expect("bob owner");
    let (ctx_a, ctx_b) = (ctx("req-disable-alice"), ctx("req-disable-bob"));
    let (a, b) = tokio::join!(disable(&t.db, &ctx_a, alice), disable(&t.db, &ctx_b, bob),);
    let refused: Vec<_> = [a, b].into_iter().filter_map(Result::err).collect();
    assert_eq!(refused.len(), 1, "exactly one is refused: {refused:?}");
    assert_reason(&refused[0], ErrorCode::AdministrationConflict, "last_owner");
    t.drop_now().await;
}
