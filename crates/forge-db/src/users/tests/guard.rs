//! The disable guard (D3 MR2, ADR-0019 §10): a disable that would leave no
//! active human administrator, or a top-level group without a direct active
//! Owner, is refused (`FG-AUTH-0009`) and changes nothing; `Guard::Force`
//! passes it and records what it left behind.

use forge_core::{Role, Scope};

use super::*;
use crate::groups::{self, CreatedGroup, NewGroup};
use crate::tokens::{self, NewToken};

/// Everything a refused disable must leave untouched: the user is active,
/// their tokens are not revoked, and the request wrote no audit row or event.
async fn assert_untouched(t: &FreshDb, user: UserId, request: &str) {
    let mut su = t.superuser().await;
    let state: String = sqlx::query_scalar("SELECT state FROM forge.users WHERE id = $1")
        .bind(user.get())
        .fetch_one(&mut su)
        .await
        .expect("state");
    assert_eq!(state, "active", "{request}");
    assert_eq!(
        count(
            &mut su,
            &format!(
                "SELECT count(*) FROM forge.tokens WHERE user_id = {} AND revoked_at IS NOT NULL",
                user.get()
            )
        )
        .await,
        0,
        "a refused disable revokes nothing ({request})"
    );
    assert!(audit_rows(&mut su, request).await.is_empty(), "{request}");
    assert_eq!(
        count(
            &mut su,
            &format!("SELECT count(*) FROM forge.events WHERE request_id = '{request}'")
        )
        .await,
        0,
        "{request}"
    );
}

pub(super) async fn top_level(t: &FreshDb, segment: &str, owner: UserId) -> CreatedGroup {
    group_under(t, segment, None, owner).await
}

pub(super) async fn group_under(
    t: &FreshDb,
    segment: &str,
    parent: Option<&CreatedGroup>,
    owner: UserId,
) -> CreatedGroup {
    groups::create(
        &t.db,
        &ctx(&format!("req-group-{segment}")),
        &NewGroup {
            segment: segment.to_owned(),
            parent: parent.map(|p| p.namespace_id),
            display_name: format!("Group {segment}"),
            description: String::new(),
            owner,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{segment}: {e}"))
}

pub(super) async fn token_for(t: &FreshDb, user: UserId, request: &str) {
    tokens::issue(
        &t.db,
        &ctx(request),
        &NewToken {
            user,
            name: "laptop".to_owned(),
            scopes: vec![Scope::ReadApi],
            expires_in: None,
            is_agent: false,
        },
    )
    .await
    .expect("issue");
}

#[tokio::test]
async fn disabling_the_last_active_human_admin_is_administration_conflict() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, true).await;
    // None of these count as "another administrator".
    let _bot = account(&t, "ci-bot", UserKind::Bot, true).await;
    let _service = account(&t, "backup", UserKind::ServiceAccount, true).await;
    let carol = account(&t, "carol", UserKind::Human, true).await;
    let _ = disable(&t.db, &ctx("req-disable-carol"), carol)
        .await
        .expect("carol is not the last admin while alice is active");
    let _bob = account(&t, "bob", UserKind::Human, false).await;
    token_for(&t, alice, "req-token-alice").await;

    let err = disable(&t.db, &ctx("req-disable-alice"), alice)
        .await
        .expect_err("alice is the last active human admin");
    assert_reason(&err, ErrorCode::AdministrationConflict, "last_admin");
    assert_eq!(err.code().http_status(), 409);
    assert!(
        err.to_string().contains(&format!("user={}", alice.get())),
        "{err}"
    );
    assert_untouched(&t, alice, "req-disable-alice").await;

    // With a second active human admin the disable goes through.
    let _dave = account(&t, "dave", UserKind::Human, true).await;
    let outcome = disable(&t.db, &ctx("req-disable-alice-2"), alice)
        .await
        .expect("dave remains");
    assert_eq!(outcome, DisableOutcome::Disabled);
    t.drop_now().await;
}

#[tokio::test]
async fn disabling_a_non_admin_is_not_refused_when_no_admin_exists() {
    let t = FreshDb::migrated().await;
    let bob = account(&t, "bob", UserKind::Human, false).await;
    let outcome = disable(&t.db, &ctx("req-disable-bob"), bob)
        .await
        .expect("only an administrator can be the last administrator");
    assert_eq!(outcome, DisableOutcome::Disabled);
    t.drop_now().await;
}

#[tokio::test]
async fn disabling_the_last_direct_owner_of_a_top_level_group_is_refused() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, false).await;
    let bob = account(&t, "bob", UserKind::Human, false).await;
    let carol = account(&t, "carol", UserKind::Human, false).await;
    let acme = top_level(&t, "acme", alice).await;
    // A second owner who is disabled does not count.
    groups::add_member(
        &t.db,
        &ctx("req-add-carol"),
        acme.group_id,
        carol,
        Role::Owner,
    )
    .await
    .expect("carol owner");
    let _ = disable(&t.db, &ctx("req-disable-carol"), carol)
        .await
        .expect("alice still owns acme");
    token_for(&t, alice, "req-token-alice").await;

    let err = disable(&t.db, &ctx("req-disable-alice"), alice)
        .await
        .expect_err("alice is acme's last active owner");
    assert_reason(&err, ErrorCode::AdministrationConflict, "last_owner");
    assert_eq!(err.code().http_status(), 409);
    let text = err.to_string();
    assert!(
        text.contains(&format!("group_id={}", acme.group_id.get())),
        "{text}"
    );
    assert!(text.contains("ownerless_groups=1"), "{text}");
    assert_untouched(&t, alice, "req-disable-alice").await;

    // A subgroup may inherit its owners: bob, its only direct owner, may go.
    let team = group_under(&t, "team", Some(&acme), alice).await;
    groups::add_member(&t.db, &ctx("req-add-bob"), team.group_id, bob, Role::Owner)
        .await
        .expect("bob owner of team");
    groups::remove_member(
        &t.db,
        &ctx("req-rm-alice"),
        team.group_id,
        alice,
        Role::Owner,
    )
    .await
    .expect("alice leaves team");
    let outcome = disable(&t.db, &ctx("req-disable-bob"), bob)
        .await
        .expect("a subgroup does not need a direct owner");
    assert_eq!(outcome, DisableOutcome::Disabled);

    // Another active owner of acme lets alice go.
    let dave = account(&t, "dave", UserKind::Human, false).await;
    groups::add_member(
        &t.db,
        &ctx("req-add-dave"),
        acme.group_id,
        dave,
        Role::Owner,
    )
    .await
    .expect("dave owner");
    let outcome = disable(&t.db, &ctx("req-disable-alice-2"), alice)
        .await
        .expect("dave owns acme");
    assert_eq!(outcome, DisableOutcome::Disabled);
    t.drop_now().await;
}

#[tokio::test]
async fn a_forced_disable_passes_the_guard_and_records_what_it_left() {
    let t = FreshDb::migrated().await;
    let alice = account(&t, "alice", UserKind::Human, true).await;
    let _acme = top_level(&t, "acme", alice).await;
    let _beta = top_level(&t, "beta", alice).await;
    token_for(&t, alice, "req-token-alice").await;

    let outcome = disable_with(&t.db, &ctx("req-force"), alice, Guard::Force)
        .await
        .expect("forced");
    assert_eq!(
        outcome,
        DisableOutcome::DisabledPastGuard {
            ownerless_groups: 2,
            ownerless_projects: 0,
            left_without_admin: true,
        }
    );
    let mut su = t.superuser().await;
    assert_eq!(
        audit_rows(&mut su, "req-force").await,
        vec![(
            "user.disabled".to_owned(),
            "succeeded".to_owned(),
            "user".to_owned(),
            alice.get()
        )]
    );
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = 'req-force'")
            .fetch_one(&mut su)
            .await
            .expect("details");
    assert_eq!(
        details,
        json!({
            "kind": "human",
            "revoked_tokens": 1,
            "revoked_sessions": 0,
            "forced": true,
            "ownerless_groups": 2,
            "ownerless_projects": 0,
            "left_without_admin": true,
        })
    );
    // The bulk revoke still happens (one token.revoked event).
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.events WHERE request_id = 'req-force' \
             AND event_type = 'token.revoked'"
        )
        .await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_forced_disable_the_guard_would_allow_is_a_plain_disable() {
    let t = FreshDb::migrated().await;
    let bob = account(&t, "bob", UserKind::Human, false).await;
    let outcome = disable_with(&t.db, &ctx("req-force-bob"), bob, Guard::Force)
        .await
        .expect("forced");
    assert_eq!(outcome, DisableOutcome::Disabled);
    let mut su = t.superuser().await;
    let details: serde_json::Value = sqlx::query_scalar(
        "SELECT details FROM forge.audit_log WHERE request_id = 'req-force-bob'",
    )
    .fetch_one(&mut su)
    .await
    .expect("details");
    // The operator's choice is recorded even when nothing was overridden.
    assert_eq!(
        details,
        json!({
            "kind": "human",
            "revoked_tokens": 0,
            "revoked_sessions": 0,
            "forced": true,
            "ownerless_groups": 0,
            "ownerless_projects": 0,
            "left_without_admin": false,
        })
    );
    t.drop_now().await;
}
