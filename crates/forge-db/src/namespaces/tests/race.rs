//! Moves under concurrency, one interleaving pinned per test with a held
//! fixture transaction (an advisory path lock or a row lock) and
//! `until_waiting`: move vs create, move vs move, move vs release. Each
//! ends with the tree invariant (`assert_tree_consistent`).

use super::*;

/// The fixture statement holding the path lock of `path` (as every
/// writer of a path takes it).
fn hold_path(path: &str) -> String {
    format!("SELECT pg_advisory_xact_lock(hashtextextended('namespace:' || '{path}', 0))")
}

/// What the move waits on when a path lock is held.
const PATHS_WAIT: &str = "pg_advisory_xact_lock(hashtextextended('namespace:' || p, 0))";
/// What a create waits on below a parent the move locked.
const PARENT_SHARE_WAIT: &str = "FROM forge.namespaces WHERE id = $1 FOR SHARE";
/// What a create waits on when its path lock is held.
const CREATE_PATH_WAIT: &str =
    "SELECT pg_advisory_xact_lock(hashtextextended('namespace:' || $1, 0))";
/// What the move waits on when a node of its subtree is locked.
const SUBTREE_LOCK_WAIT: &str = "ORDER BY id FOR NO KEY UPDATE";

/// Pinned, move first: the move has locked `acme/g` and waits on the path
/// lock of its new path; a create of `p` below `g` waits on `g`'s row
/// (`FOR SHARE`), then reads `g`'s new path and creates `beta/g/p` — never
/// the old, unlocked `acme/g/p`.
#[tokio::test]
async fn a_create_under_a_moving_parent_never_uses_an_unlocked_path() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let beta = group(&t, "beta", None, alice.user_id).await;
    let g = group(&t, "g", Some(acme.namespace_id), alice.user_id).await;
    let held = t
        .hold_audited(("group", beta.group_id.get()), &[hold_path("beta/g")])
        .await;
    let moving = move_group(
        &t,
        g.group_id,
        alice.user_id,
        to(Some(beta.namespace_id), "g"),
    );
    let creating = async {
        t.until_waiting(PATHS_WAIT).await;
        let new = NewProject::new("p", g.namespace_id, "P", alice.user_id);
        let c = ctx("req-create");
        let created = projects::create(&t.db, &c, &new);
        let release = async {
            t.until_waiting(PARENT_SHARE_WAIT).await;
            held.commit().await;
        };
        let (created, ()) = tokio::join!(created, release);
        created
    };
    let (moved, created) = tokio::join!(moving, creating);
    moved.expect("move");
    let created = created.expect("create");
    let mut su = t.superuser().await;
    assert_eq!(path_of(&mut su, created.namespace_id).await, "beta/g/p");
    assert_tree_consistent(&mut su).await;
    t.drop_now().await;
}

/// Pinned, create first: a create below `g` holds `g` (`FOR SHARE`) and
/// waits on its path lock; the move waits on `g`'s row. The create commits
/// `acme/g/p`; the move re-reads its subtree, moves `p` too and leaves a
/// tombstone at `acme/g/p`.
#[tokio::test]
async fn a_child_created_before_the_move_locks_its_parent_moves_with_it() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let beta = group(&t, "beta", None, alice.user_id).await;
    let g = group(&t, "g", Some(acme.namespace_id), alice.user_id).await;
    let held = t
        .hold_audited(("group", acme.group_id.get()), &[hold_path("acme/g/p")])
        .await;
    let new = NewProject::new("p", g.namespace_id, "P", alice.user_id);
    let c = ctx("req-create");
    let creating = projects::create(&t.db, &c, &new);
    let moving = async {
        t.until_waiting(CREATE_PATH_WAIT).await;
        let moving = move_group(
            &t,
            g.group_id,
            alice.user_id,
            to(Some(beta.namespace_id), "g"),
        );
        let release = async {
            t.until_waiting(SUBTREE_LOCK_WAIT).await;
            held.commit().await;
        };
        let (moved, ()) = tokio::join!(moving, release);
        moved
    };
    let (created, moved) = tokio::join!(creating, moving);
    let created = created.expect("create");
    let (moved, _) = moved.expect("move");
    assert_eq!(moved.summary.nodes, 2, "the new child moved too");
    let mut su = t.superuser().await;
    assert_eq!(path_of(&mut su, created.namespace_id).await, "beta/g/p");
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.namespace_tombstones WHERE old_path = 'acme/g/p'"
        )
        .await,
        1
    );
    assert_tree_consistent(&mut su).await;
    t.drop_now().await;
}

/// Move vs move: `a` below `b` and `b` below `a`, the first holding its
/// locks until the second waits. Never both: the second sees the cycle (or
/// loses a deadlock), and the tree stays consistent.
#[tokio::test]
async fn two_crossing_moves_never_both_commit() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let a = group(&t, "a", None, alice.user_id).await;
    let b = group(&t, "b", None, alice.user_id).await;
    let held = t
        .hold_audited(("group", b.group_id.get()), &[hold_path("b/a")])
        .await;
    let first = move_group(&t, a.group_id, alice.user_id, to(Some(b.namespace_id), "a"));
    let second = async {
        t.until_waiting(PATHS_WAIT).await;
        let second = move_group(&t, b.group_id, alice.user_id, to(Some(a.namespace_id), "b"));
        let release = async {
            // The second waits on `a`, its target (the first holds it FOR NO KEY UPDATE).
            t.until_waiting("FOR SHARE").await;
            held.commit().await;
        };
        let (second, ()) = tokio::join!(second, release);
        second
    };
    let (first, second) = tokio::join!(first, second);
    first.expect("the first move");
    let err = second.expect_err("the crossing move");
    assert!(
        err.code() == ErrorCode::TransferInvalid || err.code() == ErrorCode::DbQueryFailed,
        "{err}"
    );
    if err.code() == ErrorCode::TransferInvalid {
        assert_reason(&err, ErrorCode::TransferInvalid, "cycle");
    }
    let mut su = t.superuser().await;
    assert_eq!(path_of(&mut su, a.namespace_id).await, "b/a");
    assert_eq!(path_of(&mut su, b.namespace_id).await, "b");
    assert_tree_consistent(&mut su).await;
    t.drop_now().await;
}

/// Move vs release: a project moving onto a tombstoned path while the
/// tombstone is released. A held path lock lines both up: the release waits
/// on it first, the move second; the release deletes the tombstone, then
/// the move finds the path free.
#[tokio::test]
async fn a_move_waiting_on_a_release_sees_the_path_free() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let web = project(&t, "web", acme.namespace_id, alice.user_id).await;
    let api = project(&t, "api", acme.namespace_id, alice.user_id).await;
    move_project(
        &t,
        web.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "site"),
        MoveOptions::default(),
    )
    .await
    .expect("web -> site");
    let mut su = t.superuser().await;
    let stone: i64 = sqlx::query_scalar("SELECT id FROM forge.namespace_tombstones")
        .fetch_one(&mut su)
        .await
        .expect("tombstone");
    let held = t
        .hold_audited(("group", acme.group_id.get()), &[hold_path("acme/web")])
        .await;
    let c = ctx("req-release");
    let releasing = tombstones::release_decided(
        &t.db,
        &c,
        TombstoneId::new(stone),
        alice.user_id,
        |_: &TombstoneReleaseFacts| Ok(()),
    );
    let moving = async {
        t.until_waiting(CREATE_PATH_WAIT).await;
        let moving = move_project(
            &t,
            api.project_id,
            alice.user_id,
            to(Some(acme.namespace_id), "web"),
            MoveOptions::default(),
        );
        let release = async {
            t.until_waiting(PATHS_WAIT).await;
            held.commit().await;
        };
        let (moved, ()) = tokio::join!(moving, release);
        moved
    };
    let (released, moved) = tokio::join!(releasing, moving);
    released.expect("release");
    moved.expect("the move after the release");
    assert_eq!(path_of(&mut su, api.namespace_id).await, "acme/web");
    assert_tree_consistent(&mut su).await;
    t.drop_now().await;
}

/// Release vs reclaim (D5 review HIGH-1): a reclaiming move holds the
/// tombstone's target row (`FOR NO KEY UPDATE`) and waits on the path lock; the
/// release takes its row locks first (the target `FOR SHARE`), so it waits
/// on the move instead of holding the path lock it needs — no deadlock. The
/// move reclaims the tombstone; the release then finds it gone.
#[tokio::test]
async fn a_release_racing_a_reclaim_waits_on_the_rows_and_finds_the_tombstone_gone() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let web = project(&t, "web", acme.namespace_id, alice.user_id).await;
    move_project(
        &t,
        web.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "site"),
        MoveOptions::default(),
    )
    .await
    .expect("web -> site");
    let mut su = t.superuser().await;
    let stone: i64 = sqlx::query_scalar("SELECT id FROM forge.namespace_tombstones")
        .fetch_one(&mut su)
        .await
        .expect("tombstone");
    let held = t
        .hold_audited(("group", acme.group_id.get()), &[hold_path("acme/web")])
        .await;
    let reclaim = MoveOptions {
        reclaim: true,
        rate_limit: RateLimit::Enforce,
    };
    let moving = move_project(
        &t,
        web.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "web"),
        reclaim,
    );
    let releasing = async {
        t.until_waiting(PATHS_WAIT).await;
        let c = ctx("req-release");
        let releasing = tombstones::release_decided(
            &t.db,
            &c,
            TombstoneId::new(stone),
            alice.user_id,
            |_: &TombstoneReleaseFacts| Ok(()),
        );
        let release = async {
            t.until_waiting("SELECT kind, owner_id FROM forge.namespaces WHERE id = $1 FOR SHARE")
                .await;
            held.commit().await;
        };
        let (released, ()) = tokio::join!(releasing, release);
        released
    };
    let (moved, released) = tokio::join!(moving, releasing);
    moved.expect("the reclaim");
    let err = released.expect_err("reclaimed meanwhile");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert_eq!(path_of(&mut su, web.namespace_id).await, "acme/web");
    assert_tree_consistent(&mut su).await;
    t.drop_now().await;
}

/// Release vs a rename of the caller: the rename holds the caller's user
/// row (`FOR UPDATE`) and waits on its path locks; the release waits on the
/// caller's account row (`FOR SHARE`) before it takes any path lock, then
/// goes through.
#[tokio::test]
async fn a_release_waits_for_a_rename_of_its_caller_without_deadlock() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let web = project(&t, "web", acme.namespace_id, alice.user_id).await;
    move_project(
        &t,
        web.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "site"),
        MoveOptions::default(),
    )
    .await
    .expect("web -> site");
    let mut su = t.superuser().await;
    let stone: i64 = sqlx::query_scalar("SELECT id FROM forge.namespace_tombstones")
        .fetch_one(&mut su)
        .await
        .expect("tombstone");
    let held = t
        .hold_audited(("user", alice.user_id.get()), &[hold_path("alicia")])
        .await;
    let c = ctx("req-rename");
    let renaming = users::rename_decided(
        &t.db,
        &c,
        alice.user_id,
        alice.user_id,
        "alicia",
        MoveOptions::default(),
        |_: &UserRenameFacts| Ok(()),
    );
    let releasing = async {
        t.until_waiting(PATHS_WAIT).await;
        let c = ctx("req-release");
        let releasing = tombstones::release_decided(
            &t.db,
            &c,
            TombstoneId::new(stone),
            alice.user_id,
            |_: &TombstoneReleaseFacts| Ok(()),
        );
        let release = async {
            t.until_waiting("is_admin FROM forge.users WHERE id = $1 FOR SHARE")
                .await;
            held.commit().await;
        };
        let (released, ()) = tokio::join!(releasing, release);
        released
    };
    let (renamed, released) = tokio::join!(renaming, releasing);
    renamed.expect("rename");
    released.expect("release after the rename");
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.namespace_tombstones WHERE old_path = 'acme/web'"
        )
        .await,
        0
    );
    assert_tree_consistent(&mut su).await;
    t.drop_now().await;
}

/// D5 review MEDIUM-5: a project moving into a descendant group of a group
/// that moves at the same time. The project's move locks its target first
/// and holds it (`FOR SHARE`) while it waits on a path lock; the group move
/// waits on that target row in its subtree without holding the project's
/// namespace, so both commit (locking the subtree first, they deadlocked).
#[tokio::test]
async fn a_project_moving_into_a_moving_group_does_not_deadlock() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let a = group(&t, "a", None, alice.user_id).await;
    let z = group(&t, "z", None, alice.user_id).await;
    let b = group(&t, "b", Some(a.namespace_id), alice.user_id).await;
    // c's namespace id is below p's: the group move locks c before p.
    let c = group(&t, "c", Some(b.namespace_id), alice.user_id).await;
    let p = project(&t, "p", a.namespace_id, alice.user_id).await;
    let held = t
        .hold_audited(("group", c.group_id.get()), &[hold_path("a/b/c/p")])
        .await;
    let project_move = move_project(
        &t,
        p.project_id,
        alice.user_id,
        to(Some(c.namespace_id), "p"),
        MoveOptions::default(),
    );
    let group_move = async {
        t.until_waiting(PATHS_WAIT).await;
        let moving = move_group(&t, a.group_id, alice.user_id, to(Some(z.namespace_id), "a"));
        let release = async {
            t.until_waiting(SUBTREE_LOCK_WAIT).await;
            held.commit().await;
        };
        let (moved, ()) = tokio::join!(moving, release);
        moved
    };
    let (project_moved, group_moved) = tokio::join!(project_move, group_move);
    project_moved.expect("the project move");
    group_moved.expect("the group move");
    let mut su = t.superuser().await;
    assert_eq!(path_of(&mut su, p.namespace_id).await, "z/a/b/c/p");
    assert_tree_consistent(&mut su).await;
    t.drop_now().await;
}

/// A deadlock (40P01) is FG-DB-0002 `reason=deadlock` with
/// `retryable=true`: the transaction was rolled back, the whole request may
/// be retried.
#[tokio::test]
async fn a_deadlock_is_reported_as_retryable() {
    let t = FreshDb::migrated().await;
    let mut one = t.db.write_pool().begin().await.expect("one");
    let mut two = t.db.write_pool().begin().await.expect("two");
    sqlx::query("SELECT pg_advisory_xact_lock(1)")
        .execute(&mut *one)
        .await
        .expect("1");
    sqlx::query("SELECT pg_advisory_xact_lock(2)")
        .execute(&mut *two)
        .await
        .expect("2");
    let (a, b) = tokio::join!(
        sqlx::query("SELECT pg_advisory_xact_lock(2)").execute(&mut *one),
        sqlx::query("SELECT pg_advisory_xact_lock(1)").execute(&mut *two),
    );
    let err = match (a, b) {
        (Err(e), _) | (_, Err(e)) => crate::query_error("test.deadlock", e),
        (Ok(_), Ok(_)) => panic!("no deadlock"),
    };
    assert_reason(&err, ErrorCode::DbQueryFailed, "deadlock");
    assert!(err.to_string().contains("retryable=true"), "{err}");
    drop((one, two));
    t.drop_now().await;
}
