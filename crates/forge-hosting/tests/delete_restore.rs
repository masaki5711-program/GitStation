#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! D5 MR3: `Hosting::delete_project` / `restore_project` — who may, the
//! configured grace period, the audit rows and events, and the answers for
//! a project that is gone or being purged.

mod common;

use std::time::Duration;

use common::*;
use forge_auth::Action;
use forge_core::Role;
use forge_db::test_support::FreshDb;
use forge_errors::ErrorCode;
use forge_hosting::Hosting;

#[tokio::test]
async fn an_owner_deletes_and_restores_with_the_configured_grace() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone()).with_deletion_grace(Duration::from_secs(2 * 86_400));
    let alice = user(&t, "alice", false).await;
    let acme = group(&t, "acme", alice.user_id).await;
    let web = project(&t, "web", acme, alice.user_id).await;
    let as_alice = session(alice.user_id, false);
    let delete = lifecycle_proof(&hosting, &as_alice, Action::ProjectDelete, web.project_id)
        .await
        .expect("an Owner may delete");
    let deletion = hosting
        .delete_project(&request("req-delete"), &delete)
        .await
        .expect("delete");
    assert_eq!(deletion.deleted_by, Some(alice.user_id));
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.projects WHERE id = 1 AND deleted_by = 1 \
             AND purge_after - deleted_at = interval '2 days'"
        )
        .await,
        1
    );
    // The project is now seen as pending deletion; everything but restore
    // is FG-REPO-0006 (410) for the Owner.
    let err = lifecycle_proof(&hosting, &as_alice, Action::ProjectDelete, web.project_id)
        .await
        .expect_err("already pending");
    assert_eq!(err.code(), ErrorCode::PendingDeletion);
    let restore = lifecycle_proof(&hosting, &as_alice, Action::ProjectRestore, web.project_id)
        .await
        .expect("restore is allowed on a pending project");
    let cleared = hosting
        .restore_project(&request("req-restore"), &restore)
        .await
        .expect("restore");
    assert_eq!(cleared, deletion);
    for (action, n) in [("project.deleted", 1), ("project.restored", 1)] {
        assert_eq!(
            count(
                &t,
                &format!("SELECT count(*) AS n FROM forge.events WHERE event_type = '{action}'")
            )
            .await,
            n,
            "{action}"
        );
    }
    // Restored twice: the proof is decided on a live project now; the
    // command answers FG-REPO-0019 not_deleted.
    let again = lifecycle_proof(&hosting, &as_alice, Action::ProjectRestore, web.project_id)
        .await
        .expect("an Owner may try");
    let err = hosting
        .restore_project(&request("req-restore2"), &again)
        .await
        .expect_err("nothing to restore");
    assert_eq!(err.code(), ErrorCode::RestoreUnavailable);
    assert!(err.to_string().contains("not_deleted"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn only_an_owner_or_an_administrator_may_delete_and_a_stranger_sees_nothing() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let carol = user(&t, "carol", false).await;
    let root = user(&t, "root", true).await;
    let acme = group(&t, "acme", alice.user_id).await;
    let web = project(&t, "web", acme, alice.user_id).await;
    forge_db::projects::add_member(
        &t.db,
        &system("req-bob"),
        web.project_id,
        bob.user_id,
        Role::Maintainer,
    )
    .await
    .expect("bob maintains web");
    let err = lifecycle_proof(
        &hosting,
        &session(bob.user_id, false),
        Action::ProjectDelete,
        web.project_id,
    )
    .await
    .expect_err("a Maintainer may not");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let err = lifecycle_proof(
        &hosting,
        &session(carol.user_id, false),
        Action::ProjectDelete,
        web.project_id,
    )
    .await
    .expect_err("a stranger sees no project");
    assert_eq!(err.code(), ErrorCode::NotFound);
    // An administrator may; deleted_by is the administrator.
    let as_root = session(root.user_id, true);
    let proof = lifecycle_proof(&hosting, &as_root, Action::ProjectDelete, web.project_id)
        .await
        .expect("an administrator may");
    let deletion = hosting
        .delete_project(&request("req-root"), &proof)
        .await
        .expect("delete");
    assert_eq!(deletion.deleted_by, Some(root.user_id));
    // A pending project is 410 to a member who can see it, 404 to a stranger.
    let err = lifecycle_proof(
        &hosting,
        &session(bob.user_id, false),
        Action::ProjectRead,
        web.project_id,
    )
    .await
    .expect_err("pending");
    assert_eq!(err.code(), ErrorCode::PendingDeletion);
    let err = lifecycle_proof(
        &hosting,
        &session(carol.user_id, false),
        Action::ProjectRead,
        web.project_id,
    )
    .await
    .expect_err("hidden");
    assert_eq!(err.code(), ErrorCode::NotFound);
    t.drop_now().await;
}

#[tokio::test]
async fn a_proof_for_another_action_is_never_a_deletion() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let acme = group(&t, "acme", alice.user_id).await;
    let web = project(&t, "web", acme, alice.user_id).await;
    let as_alice = session(alice.user_id, false);
    let update = project_proof(&hosting, &as_alice, web.project_id).await;
    let err = hosting
        .delete_project(&request("req-mismatch"), &update)
        .await
        .expect_err("ProjectUpdate proof");
    assert_eq!(err.code(), ErrorCode::Internal);
    assert!(err.to_string().contains("proof_action_mismatch"), "{err}");
    let err = hosting
        .restore_project(&request("req-mismatch2"), &update)
        .await
        .expect_err("ProjectUpdate proof");
    assert_eq!(err.code(), ErrorCode::Internal);
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.projects WHERE deleted_at IS NOT NULL"
        )
        .await,
        0
    );
    t.drop_now().await;
}
