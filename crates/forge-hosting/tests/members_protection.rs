#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The member and protected-ref commands (M1 phase 8b MR8): each decides
//! again inside its transaction on the member's direct role and the
//! protection level read there (never the request's), the last-owner fact
//! computed there, and the caller's re-read role and account; agents are
//! refused both (ADR-0017 amendment MR8).

mod common;

use common::*;
use forge_auth::{Credential, MembershipChange, Principal};
use forge_core::{RefPattern, Role, Scope};
use forge_db::test_support::FreshDb;
use forge_db::{groups, projects};
use forge_errors::ErrorCode;
use forge_hosting::{Hosting, MemberChange, RuleChange};

fn agent(user: forge_core::UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Token(vec![Scope::Api])).as_agent(true)
}

fn main_pattern() -> RefPattern {
    RefPattern::parse("refs/heads/main").expect("pattern")
}

#[tokio::test]
async fn a_maintainer_manages_project_members_within_their_role() {
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
    let p = web.project_id;
    let as_bob = session(bob.user_id, false);

    let proof = project_member_proof(&hosting, &as_bob, p, carol.user_id, |_| {
        MembershipChange::Add(Role::Developer)
    })
    .await
    .expect("add allowed");
    hosting
        .add_project_member(&request("req-add"), &proof)
        .await
        .expect("added");
    // self-elevation: bob may not make himself (or carol) an Owner.
    let err = project_member_proof(&hosting, &as_bob, p, bob.user_id, |_| {
        MembershipChange::Add(Role::Owner)
    })
    .await
    .expect_err("above own role");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let err = project_member_proof(&hosting, &as_bob, p, carol.user_id, change_to(Role::Owner))
        .await
        .expect_err("above own role");
    assert_eq!(err.code(), ErrorCode::Forbidden);

    let proof = project_member_proof(
        &hosting,
        &as_bob,
        p,
        carol.user_id,
        change_to(Role::Maintainer),
    )
    .await
    .expect("change allowed");
    let changed = hosting
        .change_project_member(&request("req-change"), &proof)
        .await
        .expect("changed");
    assert_eq!(
        changed,
        MemberChange::Changed {
            from: Role::Developer
        }
    );
    // A proof for an add does not drive a change (a programming error).
    let add = project_member_proof(&hosting, &as_bob, p, carol.user_id, |_| {
        MembershipChange::Add(Role::Guest)
    })
    .await
    .expect("allowed");
    let err = hosting
        .change_project_member(&request("req-shape"), &add)
        .await
        .expect_err("shape");
    assert_eq!(err.code(), ErrorCode::Internal);

    let proof = project_member_proof(&hosting, &as_bob, p, carol.user_id, removal)
        .await
        .expect("remove allowed");
    hosting
        .remove_project_member(&request("req-remove"), &proof)
        .await
        .expect("removed");
    // carol is gone: a second removal is decided as Missing, then 409.
    let proof = project_member_proof(&hosting, &as_bob, p, carol.user_id, removal)
        .await
        .expect("missing allowed");
    let err = hosting
        .remove_project_member(&request("req-again"), &proof)
        .await
        .expect_err("not a member");
    assert_eq!(err.code(), ErrorCode::MembershipConflict);
    // bob may not remove alice (an Owner).
    let err = project_member_proof(&hosting, &as_bob, p, alice.user_id, removal)
        .await
        .expect_err("owner");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.events WHERE event_type LIKE 'member.%'"
        )
        .await,
        3
    );
    t.drop_now().await;
}

#[tokio::test]
async fn the_last_owner_is_refused_by_authorize_inside_the_transaction() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let acme = group(&t, "acme", alice.user_id).await;
    let as_alice = session(alice.user_id, false);
    // The loader cannot know: the registry's decision passes, the one in
    // the transaction sees removes_last_owner and denies (403, not 409).
    let proof = group_member_proof(&hosting, &as_alice, acme.group_id, alice.user_id, removal)
        .await
        .expect("allowed before the transaction");
    let err = hosting
        .remove_group_member(&request("req-rm"), &proof)
        .await
        .expect_err("last owner");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(err.to_string().contains("reason=last_owner"), "{err}");
    let proof = group_member_proof(
        &hosting,
        &as_alice,
        acme.group_id,
        alice.user_id,
        change_to(Role::Maintainer),
    )
    .await
    .expect("allowed before the transaction");
    let err = hosting
        .change_group_member(&request("req-demote"), &proof)
        .await
        .expect_err("last owner");
    assert!(err.to_string().contains("reason=last_owner"), "{err}");
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.group_members WHERE role = 'owner'"
        )
        .await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn agents_are_refused_member_and_protection_changes() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let acme = group(&t, "acme", alice.user_id).await;
    let web = project(&t, "web", acme, alice.user_id).await;
    let as_agent = agent(alice.user_id);
    let err = project_member_proof(&hosting, &as_agent, web.project_id, bob.user_id, |_| {
        MembershipChange::Add(Role::Guest)
    })
    .await
    .expect_err("agent");
    assert!(err.to_string().contains("reason=agent_token"), "{err}");
    let err = group_member_proof(&hosting, &as_agent, acme.group_id, bob.user_id, |_| {
        MembershipChange::Add(Role::Guest)
    })
    .await
    .expect_err("agent");
    assert!(err.to_string().contains("reason=agent_token"), "{err}");
    let err = protection_proof(&hosting, &as_agent, web.project_id, None, Some(Role::Owner))
        .await
        .expect_err("agent");
    assert!(err.to_string().contains("reason=agent_token"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn protections_are_created_changed_and_deleted_within_the_callers_role() {
    let t = FreshDb::migrated().await;
    let hosting = Hosting::new(t.db.clone());
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let acme = group(&t, "acme", alice.user_id).await;
    let web = project(&t, "web", acme, alice.user_id).await;
    let p = web.project_id;
    projects::add_member(&t.db, &system("req-bob"), p, bob.user_id, Role::Maintainer)
        .await
        .expect("bob maintains web");
    let as_bob = session(bob.user_id, false);
    let as_alice = session(alice.user_id, false);

    let proof = protection_proof(&hosting, &as_bob, p, None, Some(Role::Maintainer))
        .await
        .expect("maintainer protection");
    let rule = hosting
        .create_protected_ref(&request("req-create"), &proof, &main_pattern())
        .await
        .expect("created");
    // The same pattern again: 409 FG-REPO-0017.
    let err = hosting
        .create_protected_ref(&request("req-dup"), &proof, &main_pattern())
        .await
        .expect_err("taken");
    assert_eq!(err.code(), ErrorCode::ProtectedRefPatternTaken);
    // bob cannot raise it to Owner; alice can.
    let err = protection_proof(&hosting, &as_bob, p, Some(rule), Some(Role::Owner))
        .await
        .expect_err("above own role");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let proof = protection_proof(&hosting, &as_alice, p, Some(rule), Some(Role::Owner))
        .await
        .expect("owner");
    assert_eq!(
        hosting
            .change_protected_ref(&request("req-raise"), &proof)
            .await
            .expect("raised"),
        RuleChange::Changed {
            from: Role::Maintainer
        }
    );
    // Now Owner-level: bob may not lift it — the stored level decides.
    let err = protection_proof(&hosting, &as_bob, p, Some(rule), None)
        .await
        .expect_err("owner-level");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    // A proof taken while the rule was Maintainer-level does not lift it
    // after alice raised it: decided again on the level read under the lock.
    let stale = {
        let fresh = forge_auth::LoadedProtection {
            rule: Some(rule),
            from: Some(Role::Maintainer),
            to: None,
        };
        forge_auth::prove(
            &as_bob,
            forge_auth::Action::ProtectedRefManage,
            forge_auth::RefProtectionCtx::new(project_ctx(&hosting, &as_bob, p).await, fresh),
        )
        .expect("decided on the old level")
    };
    let err = hosting
        .delete_protected_ref(&request("req-stale"), &stale)
        .await
        .expect_err("owner-level now");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let proof = protection_proof(&hosting, &as_alice, p, Some(rule), None)
        .await
        .expect("owner");
    hosting
        .delete_protected_ref(&request("req-delete"), &proof)
        .await
        .expect("deleted");
    assert_eq!(
        count(
            &t,
            "SELECT count(*) AS n FROM forge.events WHERE event_type LIKE 'protected_ref.%'"
        )
        .await,
        3
    );
    t.drop_now().await;
}
