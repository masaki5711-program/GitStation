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
//
// The pinned tests below fix one interleaving each with real commands on
// separate connections: a blocker transaction (superuser) holds a row lock
// that one command needs late in its change, so that command stays open
// holding its own locks while the other runs; the helper waits until the
// *specific* statement of the other command waits on a lock.

/// Wait (bounded) until a backend of this test's database runs a statement
/// containing `statement` and waits on a lock (heavyweight: advisory,
/// tuple, transactionid, …). Other tests use other databases.
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

/// The statements the tests wait for (fragments of the commands' SQL).
const ADMIN_SET_WAIT: &str = "pg_advisory_xact_lock(hashtextextended($1, 0))";
const DISABLE_ROW_WAIT: &str = "is_admin FROM forge.users WHERE id = $1 FOR UPDATE";
const DISABLE_TOKENS_WAIT: &str = "UPDATE forge.tokens SET revoked_at";
const OWNER_SHARE_WAIT: &str = "FOR SHARE OF u";
const MEMBER_DELETE_WAIT: &str = "DELETE FROM forge.group_members";

/// A superuser transaction holding `lock_sql` (a `SELECT … FOR UPDATE`) until
/// it is rolled back.
async fn blocker(t: &FreshDb, lock_sql: &str) -> PgConnection {
    let mut su = t.superuser().await;
    sqlx::raw_sql(&format!("BEGIN; {lock_sql}"))
        .execute(&mut su)
        .await
        .unwrap_or_else(|e| panic!("{lock_sql}: {e}"));
    su
}

async fn release(mut blocker: PgConnection) {
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut blocker)
        .await
        .expect("rollback the blocker");
}

async fn active(su: &mut PgConnection, user: UserId) -> bool {
    let state: String = sqlx::query_scalar("SELECT state FROM forge.users WHERE id = $1")
        .bind(user.get())
        .fetch_one(su)
        .await
        .expect("state");
    state == "active"
}

/// `acme` (top-level) with alice and bob as its direct Owners; bob holds a
/// token (its row is what the blockers hold to keep a disable open).
async fn two_owners(t: &FreshDb) -> (crate::groups::CreatedGroup, UserId, UserId) {
    let alice = account(t, "alice", UserKind::Human, false).await;
    let bob = account(t, "bob", UserKind::Human, false).await;
    let acme = super::guard::top_level(t, "acme", alice).await;
    crate::groups::add_member(
        &t.db,
        &ctx("req-add-bob"),
        acme.group_id,
        bob,
        forge_core::Role::Owner,
    )
    .await
    .expect("bob owner");
    (acme, alice, bob)
}

/// Smoke test only: two administrators disable each other through
/// `tokio::join!`; whether the two changes actually overlap is up to the
/// scheduler (it passes even without the advisory lock). The pinned test
/// below is the one that proves the lock.
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

/// Pinned: a disable waits for an uncommitted change that holds
/// `users.admin_set` (here: disabling the other admin) and then counts it.
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
        until_waiting(&t, ADMIN_SET_WAIT).await;
        writer.commit().await.expect("bob is disabled");
    };
    let (disabled, ()) = tokio::join!(disabling, committing);
    let err = disabled.expect_err("bob no longer counts");
    assert_reason(&err, ErrorCode::AdministrationConflict, "last_admin");
    let mut su = t.superuser().await;
    assert!(active(&mut su, alice).await);
    t.drop_now().await;
}

/// Pinned: a bootstrap waits for an uncommitted change that holds
/// `users.admin_set` (here: making bob an administrator) and then refuses
/// with `admin_exists`.
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
        until_waiting(&t, ADMIN_SET_WAIT).await;
        writer.commit().await.expect("bob is an admin");
    };
    let (done, ()) = tokio::join!(bootstrapping, committing);
    let err = done.expect_err("bob is an admin now");
    assert_reason(&err, ErrorCode::AdministrationConflict, "admin_exists");
    t.drop_now().await;
}

/// Pinned, removal first: the real `groups::remove_member` of bob holds the
/// group row and alice's user row `FOR SHARE` (`keep_an_owner`) and is kept
/// open before its DELETE by a blocker on bob's membership row. The disable
/// of alice must wait for alice's row, then see bob gone and refuse
/// (`last_owner`). Without `FOR SHARE OF u` the disable would not wait,
/// would still count bob, and both would commit, leaving acme ownerless.
#[tokio::test]
async fn a_disable_waits_for_a_real_owner_removal_and_then_refuses() {
    let t = FreshDb::migrated().await;
    let (acme, alice, bob) = two_owners(&t).await;
    let g = acme.group_id;
    let hold = blocker(
        &t,
        &format!(
            "SELECT 1 FROM forge.group_members WHERE group_id = {} AND user_id = {} FOR UPDATE",
            g.get(),
            bob.get()
        ),
    )
    .await;
    let (rm_ctx, dis_ctx) = (ctx("req-rm-bob"), ctx("req-disable-alice"));
    let removing = crate::groups::remove_member(&t.db, &rm_ctx, g, bob);
    let disabling = async {
        until_waiting(&t, MEMBER_DELETE_WAIT).await;
        disable(&t.db, &dis_ctx, alice).await
    };
    let releasing = async {
        until_waiting(&t, MEMBER_DELETE_WAIT).await;
        until_waiting(&t, DISABLE_ROW_WAIT).await;
        release(hold).await;
    };
    let (removed, disabled, ()) = tokio::join!(removing, disabling, releasing);
    removed.expect("alice is an active owner while bob leaves");
    let err = disabled.expect_err("bob is gone: alice is the last owner");
    assert_reason(&err, ErrorCode::AdministrationConflict, "last_owner");
    let mut su = t.superuser().await;
    assert!(active(&mut su, alice).await, "acme keeps an owner");
    t.drop_now().await;
}

/// Pinned, disable first: the disable of bob has locked and updated bob's
/// row and is kept open before its token revoke by a blocker on bob's token.
/// The real `groups::remove_member` of alice must wait for bob's row
/// (`keep_an_owner` locks the other owners `FOR SHARE`), then re-check the
/// committed row, no longer count bob (`state = 'active'`) and refuse
/// (FG-REPO-0010 `last_owner`). Without `FOR SHARE OF u`, or without the
/// active filter, the removal would commit and leave acme ownerless.
#[tokio::test]
async fn a_real_owner_removal_waits_for_a_disable_and_then_refuses() {
    let t = FreshDb::migrated().await;
    let (acme, alice, bob) = two_owners(&t).await;
    super::guard::token_for(&t, bob, "req-token-bob").await;
    let g = acme.group_id;
    let hold = blocker(
        &t,
        &format!(
            "SELECT 1 FROM forge.tokens WHERE user_id = {} FOR UPDATE",
            bob.get()
        ),
    )
    .await;
    let (dis_ctx, rm_ctx) = (ctx("req-disable-bob"), ctx("req-rm-alice"));
    let disabling = disable(&t.db, &dis_ctx, bob);
    let removing = async {
        until_waiting(&t, DISABLE_TOKENS_WAIT).await;
        crate::groups::remove_member(&t.db, &rm_ctx, g, alice).await
    };
    let releasing = async {
        until_waiting(&t, DISABLE_TOKENS_WAIT).await;
        until_waiting(&t, OWNER_SHARE_WAIT).await;
        release(hold).await;
    };
    let (disabled, removed, ()) = tokio::join!(disabling, removing, releasing);
    assert_eq!(
        disabled.expect("alice still owns acme"),
        DisableOutcome::Disabled
    );
    let err = removed.expect_err("bob is disabled: alice is the last owner");
    assert_reason(&err, ErrorCode::MembershipConflict, "last_owner");
    let mut su = t.superuser().await;
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT role FROM forge.group_members WHERE group_id = $1 AND user_id = $2",
    )
    .bind(g.get())
    .bind(alice.get())
    .fetch_optional(&mut su)
    .await
    .expect("role");
    assert_eq!(owner.as_deref(), Some("owner"), "acme keeps alice");
    t.drop_now().await;
}

/// Pinned, the two Owners disable each other: bob's disable holds
/// `users.admin_set` and is kept open before its token revoke; alice's
/// disable waits on the advisory lock, then sees bob disabled and refuses
/// (`last_owner`). The user row locks alone would not serialize these two
/// (each locks only its own row).
#[tokio::test]
async fn two_owners_disabling_each_other_are_serialized_by_the_admin_set_lock() {
    let t = FreshDb::migrated().await;
    let (_acme, alice, bob) = two_owners(&t).await;
    super::guard::token_for(&t, bob, "req-token-bob").await;
    let hold = blocker(
        &t,
        &format!(
            "SELECT 1 FROM forge.tokens WHERE user_id = {} FOR UPDATE",
            bob.get()
        ),
    )
    .await;
    let (ctx_b, ctx_a) = (ctx("req-disable-bob"), ctx("req-disable-alice"));
    let disabling_bob = disable(&t.db, &ctx_b, bob);
    let disabling_alice = async {
        until_waiting(&t, DISABLE_TOKENS_WAIT).await;
        disable(&t.db, &ctx_a, alice).await
    };
    let releasing = async {
        until_waiting(&t, DISABLE_TOKENS_WAIT).await;
        until_waiting(&t, ADMIN_SET_WAIT).await;
        release(hold).await;
    };
    let (b, a, ()) = tokio::join!(disabling_bob, disabling_alice, releasing);
    assert_eq!(b.expect("alice still owns acme"), DisableOutcome::Disabled);
    let err = a.expect_err("bob is disabled: alice is the last owner");
    assert_reason(&err, ErrorCode::AdministrationConflict, "last_owner");
    t.drop_now().await;
}

/// Smoke test only (scheduler-dependent, like the admin one): the two
/// Owners disable each other through `tokio::join!`; exactly one is refused.
/// The pinned test above proves the serialization.
#[tokio::test]
async fn two_owners_disabling_each_other_at_once_leave_one_owner() {
    let t = FreshDb::migrated().await;
    let (_acme, alice, bob) = two_owners(&t).await;
    let (ctx_a, ctx_b) = (ctx("req-disable-alice"), ctx("req-disable-bob"));
    let (a, b) = tokio::join!(disable(&t.db, &ctx_a, alice), disable(&t.db, &ctx_b, bob),);
    let refused: Vec<_> = [a, b].into_iter().filter_map(Result::err).collect();
    assert_eq!(refused.len(), 1, "exactly one is refused: {refused:?}");
    assert_reason(&refused[0], ErrorCode::AdministrationConflict, "last_owner");
    t.drop_now().await;
}
