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
const SUBTREE_LOCK_WAIT: &str = "WHERE id = ANY($1) ORDER BY id FOR UPDATE";

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
            // The second waits on `b` (the first holds it FOR SHARE as its target).
            t.until_waiting("FOR UPDATE").await;
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
