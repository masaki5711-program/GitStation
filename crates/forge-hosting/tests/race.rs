#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! A role removed **concurrently** with a write (M1 phase 8b MR7a; the
//! member commands since MR8): the
//! removal is held open (uncommitted, its row locked) while the command
//! runs; the command must wait on the membership row it locks `FOR SHARE`,
//! then see the committed removal and refuse. Each test turns red when the
//! lock is dropped: `until_waiting` panics when nobody waits, and without
//! the lock the command would read the old row and write.

mod common;

use common::*;
use forge_core::{Role, Visibility};
use forge_db::groups;
use forge_db::test_support::FreshDb;
use forge_errors::ErrorCode;
use forge_hosting::{Hosting, Patch};

// Out of scope (MR8): a protection deleted concurrently with a push
// decision. The push decides every ref on one rule snapshot read before
// git runs (ADR-0008, `protected_refs::rules_for`); a deletion committed
// after the snapshot is the documented window of the policy snapshot, not
// a write of this crate.

/// The statement of `roles::lock_roles` that waits on a group membership.
const GROUP_MEMBER_LOCK: &str = "ORDER BY m.group_id FOR SHARE OF m";
/// The one that waits on a project membership.
const PROJECT_MEMBER_LOCK: &str = "WHERE project_id = $1 AND user_id = $2 FOR SHARE";

#[tokio::test]
async fn a_group_role_removed_while_a_project_update_waits_denies_it() {
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
    let web = project(&t, "web", acme, alice.user_id).await;
    let as_bob = session(bob.user_id, false);
    let proof = project_proof(&hosting, &as_bob, web.project_id).await;
    let removal = t
        .hold_audited(
            ("group", acme.group_id.get()),
            &[format!(
                "DELETE FROM forge.group_members WHERE group_id = {} AND user_id = {}",
                acme.group_id.get(),
                bob.user_id.get()
            )],
        )
        .await;
    let patch = Patch {
        display_name: Some("Taken over".into()),
        description: None,
    };
    let req = request("req-race");
    let updating = hosting.update_project(&req, &proof, &patch);
    let committing = async {
        t.until_waiting(GROUP_MEMBER_LOCK).await;
        removal.commit().await;
    };
    let (updated, ()) = tokio::join!(updating, committing);
    // bob has no role left on the private project: it is invisible to him.
    let err = updated.expect_err("the role is gone");
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.audit_log WHERE action = 'project.updated'"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.projects WHERE display_name = 'Project web'"
        )
        .await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_owner_removed_while_a_subgroup_create_waits_denies_it() {
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
    let proof = group_proof(
        &hosting,
        &as_bob,
        forge_auth::Action::SubgroupCreate,
        acme.group_id,
    )
    .await;
    let removal = t
        .hold_audited(
            ("group", acme.group_id.get()),
            &[format!(
                "DELETE FROM forge.group_members WHERE group_id = {} AND user_id = {}",
                acme.group_id.get(),
                bob.user_id.get()
            )],
        )
        .await;
    let req = request("req-race");
    let fields = group_fields("web");
    let creating = hosting.create_subgroup(&req, &proof, &fields);
    let committing = async {
        t.until_waiting(GROUP_MEMBER_LOCK).await;
        removal.commit().await;
    };
    let (created, ()) = tokio::join!(creating, committing);
    let err = created.expect_err("bob is no Owner any more");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(count(&t, "SELECT count(*) AS n FROM forge.groups").await, 1);
    t.drop_now().await;
}

#[tokio::test]
async fn a_maintainer_removed_while_a_project_create_waits_denies_it() {
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
    let proof = parent_proof(&hosting, &as_bob, acme.namespace_id).await;
    let removal = t
        .hold_audited(
            ("group", acme.group_id.get()),
            &[format!(
                "DELETE FROM forge.group_members WHERE group_id = {} AND user_id = {}",
                acme.group_id.get(),
                bob.user_id.get()
            )],
        )
        .await;
    let req = request("req-race");
    let fields = project_fields("web");
    let creating = hosting.create_project(&req, &proof, &fields);
    let committing = async {
        t.until_waiting(GROUP_MEMBER_LOCK).await;
        removal.commit().await;
    };
    let (created, ()) = tokio::join!(creating, committing);
    let err = created.expect_err("bob holds no role in acme any more");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(
        count(&t, "SELECT count(*) AS n FROM forge.projects").await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_owner_demoted_while_a_visibility_change_waits_denies_it() {
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
    let proof = visibility_proof(&hosting, &as_bob, web.project_id, Visibility::Internal).await;
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
    let req = request("req-race");
    let changing = hosting.change_visibility(&req, &proof);
    let committing = async {
        t.until_waiting(PROJECT_MEMBER_LOCK).await;
        demotion.commit().await;
    };
    let (changed, ()) = tokio::join!(changing, committing);
    let err = changed.expect_err("bob is a Developer now");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.events WHERE event_type = 'project.visibility_changed'"
        )
        .await,
        0
    );
    t.drop_now().await;
}

/// The caller's account row, locked `FOR SHARE` by `access::lock_account`.
const ACCOUNT_LOCK: &str = "is_admin FROM forge.users WHERE id = $1 FOR SHARE";

#[tokio::test]
async fn a_disable_committed_while_a_project_update_waits_denies_it() {
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
    let web = project(&t, "web", acme, alice.user_id).await;
    let proof = project_proof(&hosting, &session(bob.user_id, false), web.project_id).await;
    let disable = t
        .hold_audited(
            ("user", bob.user_id.get()),
            &[format!(
                "UPDATE forge.users SET state = 'disabled' WHERE id = {}",
                bob.user_id.get()
            )],
        )
        .await;
    let patch = Patch {
        display_name: Some("Taken over".into()),
        description: None,
    };
    let req = request("req-race");
    let updating = hosting.update_project(&req, &proof, &patch);
    let committing = async {
        t.until_waiting(ACCOUNT_LOCK).await;
        disable.commit().await;
    };
    let (updated, ()) = tokio::join!(updating, committing);
    let err = updated.expect_err("bob is disabled now");
    assert_eq!(err.code(), ErrorCode::AccountDisabled);
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.audit_log WHERE action = 'project.updated'"
        )
        .await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_owner_removed_while_a_group_update_waits_denies_it() {
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
    let proof = group_proof(
        &hosting,
        &session(bob.user_id, false),
        forge_auth::Action::GroupUpdate,
        acme.group_id,
    )
    .await;
    let removal = t
        .hold_audited(
            ("group", acme.group_id.get()),
            &[format!(
                "DELETE FROM forge.group_members WHERE group_id = {} AND user_id = {}",
                acme.group_id.get(),
                bob.user_id.get()
            )],
        )
        .await;
    let patch = Patch {
        display_name: Some("Taken over".into()),
        description: None,
    };
    let req = request("req-race");
    let updating = hosting.update_group(&req, &proof, &patch);
    let committing = async {
        t.until_waiting(GROUP_MEMBER_LOCK).await;
        removal.commit().await;
    };
    let (updated, ()) = tokio::join!(updating, committing);
    let err = updated.expect_err("bob is no Owner any more");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.audit_log WHERE action = 'group.updated'"
        )
        .await,
        0
    );
    t.drop_now().await;
}

/// The decided member commands' read of the member's row (`FOR UPDATE`).
const MEMBER_ROW_LOCK: &str = "AND user_id = $2 FOR UPDATE";

#[tokio::test]
async fn a_group_role_removed_while_a_member_add_waits_denies_it() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let carol = user(&t, "carol", false).await;
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
    let web = project(&t, "web", acme, alice.user_id).await;
    let as_bob = session(bob.user_id, false);
    let proof = project_member_proof(&hosting, &as_bob, web.project_id, carol.user_id, |_| {
        forge_auth::MembershipChange::Add(Role::Developer)
    })
    .await
    .expect("allowed before the race");
    let removal = t
        .hold_audited(
            ("group", acme.group_id.get()),
            &[format!(
                "DELETE FROM forge.group_members WHERE group_id = {} AND user_id = {}",
                acme.group_id.get(),
                bob.user_id.get()
            )],
        )
        .await;
    let req = request("req-race");
    let adding = hosting.add_project_member(&req, &proof);
    let committing = async {
        t.until_waiting(GROUP_MEMBER_LOCK).await;
        removal.commit().await;
    };
    let (added, ()) = tokio::join!(adding, committing);
    // bob has no role left on the private project: it is invisible to him.
    let err = added.expect_err("the role is gone");
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert_eq!(
        count(
            &t,
            &format!(
                "SELECT count(*) AS n FROM forge.project_members WHERE user_id = {}",
                carol.user_id.get()
            )
        )
        .await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_member_promoted_while_their_removal_waits_is_decided_on_the_new_role() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let carol = user(&t, "carol", false).await;
    let acme = group(&t, "acme", alice.user_id).await;
    let web = project(&t, "web", acme, alice.user_id).await;
    for (who, role) in [
        (bob.user_id, Role::Maintainer),
        (carol.user_id, Role::Developer),
    ] {
        forge_db::projects::add_member(&t.db, &system("req-add"), web.project_id, who, role)
            .await
            .expect("member");
    }
    let as_bob = session(bob.user_id, false);
    // bob (Maintainer) may remove carol (Developer) — as loaded now.
    let proof = project_member_proof(&hosting, &as_bob, web.project_id, carol.user_id, removal)
        .await
        .expect("allowed before the race");
    let promotion = t
        .hold_audited(
            ("project", web.project_id.get()),
            &[format!(
                "UPDATE forge.project_members SET role = 'owner' \
                 WHERE project_id = {} AND user_id = {}",
                web.project_id.get(),
                carol.user_id.get()
            )],
        )
        .await;
    let req = request("req-race");
    let removing = hosting.remove_project_member(&req, &proof);
    let committing = async {
        t.until_waiting(MEMBER_ROW_LOCK).await;
        promotion.commit().await;
    };
    let (removed, ()) = tokio::join!(removing, committing);
    // carol is an Owner now: a Maintainer does not remove her.
    let err = removed.expect_err("carol is an Owner now");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(err.to_string().contains("reason=above_own_role"), "{err}");
    assert_eq!(
        count(
            &t,
            &format!(
                "SELECT count(*) AS n FROM forge.project_members WHERE user_id = {} \
                 AND role = 'owner'",
                carol.user_id.get()
            )
        )
        .await,
        1
    );
    t.drop_now().await;
}
