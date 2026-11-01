#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! D5 MR5: a move decided twice. The registry's proof was taken while the
//! caller was the project's Owner; the demotion is held open (uncommitted,
//! its row locked) while the move runs. The move must wait on the
//! membership row it locks `FOR SHARE`, then see the committed demotion and
//! refuse — nothing moved, no audit row, no event, no tombstone. Red when
//! the lock is dropped (`until_waiting` panics when nobody waits).

mod common;

use common::*;
use forge_auth::{
    prove, Action, LoadedGroup, LoadedTransferTarget, LoadedUserNamespace, TransferCtx,
};
use forge_core::Role;
use forge_db::test_support::FreshDb;
use forge_db::{groups, projects};
use forge_errors::ErrorCode;
use forge_hosting::{Hosting, MoveRequest};

/// The statement of `roles::lock_roles` that waits on a project membership.
const PROJECT_MEMBER_LOCK: &str = "WHERE project_id = $1 AND user_id = $2 FOR SHARE";

#[tokio::test]
async fn an_owner_demoted_while_a_move_waits_is_refused_and_nothing_is_written() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let acme = group(&t, "acme", alice.user_id).await;
    let beta = group(&t, "beta", alice.user_id).await;
    groups::add_member(
        &t.db,
        &system("req-bob-beta"),
        beta.group_id,
        bob.user_id,
        Role::Maintainer,
    )
    .await
    .expect("bob maintains beta");
    let web = project(&t, "web", acme, alice.user_id).await;
    projects::add_member(
        &t.db,
        &system("req-bob-web"),
        web.project_id,
        bob.user_id,
        Role::Owner,
    )
    .await
    .expect("bob owns web");
    let as_bob = session(bob.user_id, false);
    let loaded = project_ctx(&hosting, &as_bob, web.project_id).await;
    assert_eq!(loaded.role, Some(Role::Owner));
    let target = LoadedTransferTarget::Group {
        namespace: beta.namespace_id,
        group: LoadedGroup {
            id: beta.group_id,
            role: Some(Role::Maintainer),
        },
    };
    let proof = prove(
        &as_bob,
        Action::ProjectTransfer,
        TransferCtx::new(loaded, target),
    )
    .expect("allowed before the race");
    let demotion = t
        .hold_audited(
            ("project", web.project_id.get()),
            &[format!(
                "UPDATE forge.project_members SET role = 'developer' \
                 WHERE project_id = {} AND user_id = {}",
                web.project_id.get(),
                bob.user_id.get()
            )],
        )
        .await;
    let req = request("req-race-move");
    let to = MoveRequest {
        segment: "web".into(),
        reclaim: false,
    };
    let moving = hosting.move_project(&req, &proof, &to);
    let committing = async {
        t.until_waiting(PROJECT_MEMBER_LOCK).await;
        demotion.commit().await;
    };
    let (moved, ()) = tokio::join!(moving, committing);
    // bob is a Developer of web now: he sees it, he may not move it.
    let err = moved.expect_err("the Owner role is gone");
    assert_eq!(err.code(), ErrorCode::Forbidden, "{err:?}");
    for (what, sql) in [
        (
            "audit",
            "SELECT count(*) AS n FROM forge.audit_log WHERE action = 'project.moved'",
        ),
        (
            "event",
            "SELECT count(*) AS n FROM forge.events WHERE event_type = 'project.moved'",
        ),
        (
            "tombstone",
            "SELECT count(*) AS n FROM forge.namespace_tombstones",
        ),
    ] {
        assert_eq!(count(&t, sql).await, 0, "{what}");
    }
    let rows = t
        .json_rows(&format!(
            "SELECT full_path FROM forge.namespaces WHERE id = {}",
            web.namespace_id.get()
        ))
        .await;
    assert_eq!(rows[0]["full_path"], "acme/web", "not moved");
    t.drop_now().await;
}

/// A proof for one destination never moves the project elsewhere, and a
/// proof for another action is refused before any transaction.
#[tokio::test]
async fn a_transfer_proof_is_bound_to_its_target_and_action() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let acme = group(&t, "acme", alice.user_id).await;
    let web = project(&t, "web", acme, alice.user_id).await;
    let as_alice = session(alice.user_id, false);
    let loaded = project_ctx(&hosting, &as_alice, web.project_id).await;
    // Decided for alice's personal namespace: the command moves it there.
    let proof = prove(
        &as_alice,
        Action::ProjectTransfer,
        TransferCtx::new(
            loaded,
            LoadedTransferTarget::User(LoadedUserNamespace {
                namespace: alice.namespace_id,
                owner: alice.user_id,
            }),
        ),
    )
    .expect("allowed");
    let moved = hosting
        .move_project(
            &request("req-move-own"),
            &proof,
            &MoveRequest {
                segment: "web".into(),
                reclaim: false,
            },
        )
        .await
        .expect("moved");
    assert_eq!(moved.new_path, "alice/web");
    // A project proof for ProjectUpdate is not a transfer proof.
    let update = project_proof(&hosting, &as_alice, web.project_id).await;
    let err = hosting
        .delete_project(&request("req-wrong"), &update)
        .await
        .expect_err("another action");
    assert_eq!(err.code(), ErrorCode::Internal);
    t.drop_now().await;
}
