#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! forge-hosting's commands against the development database (ADR-0011):
//! each write decides again inside its transaction, on the facts it holds.
//! These tests change a fact **between** the registry's proof and the
//! command (sequentially; `race.rs` pins the concurrent interleavings).

mod common;

use common::*;
use forge_auth::{prove, Action, InstanceCtx};
use forge_core::{Role, Visibility};
use forge_db::groups;
use forge_db::test_support::FreshDb;
use forge_errors::ErrorCode;
use forge_hosting::{Hosting, Patch, Updated, VisibilityOutcome};

#[tokio::test]
async fn an_admin_creates_a_top_level_group_and_a_revoked_admin_flag_is_seen() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let root = user(&t, "root", true).await;
    let alice = user(&t, "alice", false).await;
    let admin = session(root.user_id, true);
    let proof = prove(&admin, Action::GroupCreateTopLevel, InstanceCtx).expect("admin");
    let created = hosting
        .create_group(&request("req-acme"), &proof, &group_fields("acme"))
        .await
        .expect("created");
    let record = hosting
        .group(created.group_id, Some(root.user_id))
        .await
        .expect("load")
        .expect("exists");
    assert_eq!(
        record.role,
        Some(Role::Owner),
        "the creator is the first Owner"
    );
    assert_eq!(record.path.full_path(), "acme");
    // The principal claims administrator rights the account no longer has
    // (here: never had): the in-transaction decision reads the row.
    let stale = session(alice.user_id, true);
    let proof = prove(&stale, Action::GroupCreateTopLevel, InstanceCtx).expect("stale claim");
    let err = hosting
        .create_group(&request("req-stale"), &proof, &group_fields("other"))
        .await
        .expect_err("not an administrator in the database");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(
        count(&t, "SELECT count(*) AS n FROM forge.groups").await,
        1,
        "nothing was written"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_proof_for_another_action_is_refused_before_anything_runs() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let root = user(&t, "root", true).await;
    let admin = session(root.user_id, true);
    let proof = prove(&admin, Action::UserCreate, InstanceCtx).expect("admin");
    let err = hosting
        .create_group(&request("req-mismatch"), &proof, &group_fields("acme"))
        .await
        .expect_err("a UserCreate proof");
    assert_eq!(err.code(), ErrorCode::Internal);
    assert!(
        err.to_string().contains("reason=proof_action_mismatch"),
        "{err}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn subgroup_and_group_update_follow_the_role_held_in_the_transaction() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let acme = group(&t, "acme", alice.user_id).await;
    groups::add_member(
        &t.db,
        &system("req-bob"),
        acme.group_id,
        bob.user_id,
        Role::Owner,
    )
    .await
    .expect("bob owns acme");
    let as_bob = session(bob.user_id, false);
    let sub = group_proof(&hosting, &as_bob, Action::SubgroupCreate, acme.group_id).await;
    let update = group_proof(&hosting, &as_bob, Action::GroupUpdate, acme.group_id).await;
    let web = hosting
        .create_subgroup(&request("req-web"), &sub, &group_fields("web"))
        .await
        .expect("subgroup");
    let record = hosting
        .group(web.group_id, Some(bob.user_id))
        .await
        .expect("load")
        .expect("exists");
    assert_eq!(record.parent_id, Some(acme.group_id));
    assert_eq!(record.path.full_path(), "acme/web");
    // bob is demoted after the registry's check: the command refuses.
    let _changed = groups::change_member(
        &t.db,
        &system("req-demote"),
        acme.group_id,
        bob.user_id,
        Role::Owner,
        Role::Developer,
    )
    .await
    .expect("demote bob");
    let err = hosting
        .create_subgroup(&request("req-api"), &sub, &group_fields("api"))
        .await
        .expect_err("demoted");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let patch = Patch {
        display_name: Some("Acme Inc".into()),
        description: None,
    };
    let err = hosting
        .update_group(&request("req-update"), &update, &patch)
        .await
        .expect_err("demoted");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    // alice (Owner) updates it; the same values again change nothing.
    let as_alice = session(alice.user_id, false);
    let update = group_proof(&hosting, &as_alice, Action::GroupUpdate, acme.group_id).await;
    assert!(matches!(
        hosting
            .update_group(&request("req-u1"), &update, &patch)
            .await,
        Ok(Updated::Changed(_))
    ));
    assert_eq!(
        hosting
            .update_group(&request("req-u2"), &update, &patch)
            .await
            .expect("unchanged"),
        Updated::Unchanged
    );
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.audit_log WHERE action = 'group.updated'"
        )
        .await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn projects_are_created_where_the_caller_may_and_never_elsewhere() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let acme = group(&t, "acme", alice.user_id).await;
    groups::add_member(
        &t.db,
        &system("req-bob"),
        acme.group_id,
        bob.user_id,
        Role::Maintainer,
    )
    .await
    .expect("bob maintains acme");
    let as_bob = session(bob.user_id, false);
    // In a group bob maintains, and in his own namespace.
    let in_acme = parent_proof(&hosting, &as_bob, acme.namespace_id).await;
    let created = hosting
        .create_project(&request("req-web"), &in_acme, &project_fields("web"))
        .await
        .expect("in acme");
    let record = hosting
        .project(created.project_id, Some(bob.user_id))
        .await
        .expect("load")
        .expect("exists");
    assert_eq!(record.path.full_path(), "acme/web");
    assert_eq!(
        record.access.role,
        Some(Role::Owner),
        "the creator is a direct Owner"
    );
    let own = parent_proof(&hosting, &as_bob, bob.namespace_id).await;
    hosting
        .create_project(&request("req-own"), &own, &project_fields("notes"))
        .await
        .expect("own namespace");
    // Demoted to Developer after the check: refused, nothing written.
    let _changed = groups::change_member(
        &t.db,
        &system("req-demote"),
        acme.group_id,
        bob.user_id,
        Role::Maintainer,
        Role::Developer,
    )
    .await
    .expect("demote bob");
    let err = hosting
        .create_project(&request("req-api"), &in_acme, &project_fields("api"))
        .await
        .expect_err("demoted");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(
        count(&t, "SELECT count(*) AS n FROM forge.projects").await,
        2
    );
    t.drop_now().await;
}

#[tokio::test]
async fn project_updates_see_deletion_and_lost_ownership() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let acme = group(&t, "acme", alice.user_id).await;
    let web = project(&t, "web", acme, alice.user_id).await;
    forge_db::projects::add_member(
        &t.db,
        &system("req-bob"),
        web.project_id,
        bob.user_id,
        Role::Owner,
    )
    .await
    .expect("bob owns web");
    let as_bob = session(bob.user_id, false);
    let update = project_proof(&hosting, &as_bob, web.project_id).await;
    let internal = visibility_proof(&hosting, &as_bob, web.project_id, Visibility::Internal).await;
    let patch = Patch {
        display_name: None,
        description: Some("synthetic".into()),
    };
    assert!(matches!(
        hosting
            .update_project(&request("req-u1"), &update, &patch)
            .await,
        Ok(Updated::Changed(_))
    ));
    assert_eq!(
        hosting
            .change_visibility(&request("req-v1"), &internal)
            .await
            .expect("owner"),
        VisibilityOutcome::Changed {
            from: Visibility::Private
        }
    );
    // bob is demoted to Maintainer: an update still passes, the
    // visibility (Owner only) no longer does.
    let _changed = forge_db::projects::change_member(
        &t.db,
        &system("req-demote"),
        web.project_id,
        bob.user_id,
        Role::Owner,
        Role::Maintainer,
    )
    .await
    .expect("demote bob");
    let private = visibility_proof(
        &hosting,
        &session(alice.user_id, false),
        web.project_id,
        Visibility::Private,
    )
    .await;
    let err = hosting
        .change_visibility(&request("req-v2"), &internal)
        .await
        .expect_err("not an Owner any more");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    // The project is deleted after the check (D5 writes this; a fixture
    // here): every write refuses with FG-REPO-0006.
    t.commit_audited(
        ("project", web.project_id.get()),
        &[format!(
            "UPDATE forge.projects SET deleted_at = now(), purge_after = now() + interval '7 days' \
             WHERE id = {}",
            web.project_id.get()
        )],
    )
    .await;
    let err = hosting
        .update_project(&request("req-u2"), &update, &patch)
        .await
        .expect_err("pending deletion");
    assert_eq!(err.code(), ErrorCode::PendingDeletion);
    let err = hosting
        .change_visibility(&request("req-v3"), &private)
        .await
        .expect_err("pending deletion");
    assert_eq!(err.code(), ErrorCode::PendingDeletion);
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.events WHERE event_type = 'project.visibility_changed'"
        )
        .await,
        1
    );
    t.drop_now().await;
}
