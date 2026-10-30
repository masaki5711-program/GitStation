#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Protected ref rules from the database reach `authorize()` (step D4 MR5,
//! ADR-0008): the permission-matrix test builds `RefProtection` by hand and
//! cannot see a lookup that drops a matching rule, so these tests load the
//! levels with `protected_refs::levels_for` the way the push path does,
//! build the protection with `RefProtection::effective` and decide.
//! Needs the development database (ADR-0011).

use forge_auth::{
    authorize, Action, Credential, Principal, ProjectView, RefProtection, RefUpdate, Resource,
    Role, Visibility,
};
use forge_core::{ProjectId, RefName, RefPattern, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::protected_refs;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

/// `alice/web` and its owner alice.
async fn web(t: &FreshDb) -> (ProjectId, UserId) {
    let alice = users::create(
        &t.db,
        &ctx("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &ctx("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    (web.project_id, alice.user_id)
}

async fn rule(t: &FreshDb, project: ProjectId, request: &str, pattern: &str, role: Role) {
    protected_refs::create(
        &t.db,
        &ctx(request),
        project,
        &RefPattern::parse(pattern).expect("pattern"),
        role,
    )
    .await
    .unwrap_or_else(|e| panic!("{pattern}: {e}"));
}

/// The push decision for one fast-forward of `name` by a caller with
/// `role`: the protection comes from the database, never from the request.
async fn push(
    t: &FreshDb,
    project: ProjectId,
    caller: UserId,
    role: Role,
    name: &str,
) -> (Option<RefProtection>, Result<(), ForgeError>) {
    let name = RefName::parse(name).expect("ref");
    let levels = protected_refs::levels_for(&t.db, project, &name)
        .await
        .expect("levels");
    let protection = RefProtection::effective(levels);
    let principal = Principal::authenticated(caller, false, true, Credential::Session);
    let resource = Resource::Ref {
        project: ProjectView {
            visibility: Visibility::Private,
            role: Some(role),
            pending_deletion: false,
        },
        name,
        update: RefUpdate::FastForward,
        protection,
    };
    (
        protection,
        authorize(&principal, Action::RepoPush, &resource),
    )
}

#[tokio::test]
async fn adding_a_rule_never_loosens_protection() {
    let t = FreshDb::migrated().await;
    let (web, alice) = web(&t).await;
    // Every branch needs a Maintainer.
    rule(&t, web, "req-rule-all", "refs/heads/*", Role::Maintainer).await;
    let (protection, decided) = push(&t, web, alice, Role::Developer, "refs/heads/main").await;
    assert_eq!(
        protection.map(|p| p.push_min_role()),
        Some(Role::Maintainer)
    );
    let err = decided.expect_err("a developer may not push a protected branch");
    assert_eq!(err.code(), ErrorCode::ProtectedRefPushNotAllowed);

    // A looser rule on main (Developer) does not lift the wildcard's
    // Maintainer level: the strictest matching rule wins.
    rule(&t, web, "req-rule-main", "refs/heads/main", Role::Developer).await;
    let (protection, decided) = push(&t, web, alice, Role::Developer, "refs/heads/MAIN").await;
    assert_eq!(
        protection.map(|p| p.push_min_role()),
        Some(Role::Maintainer)
    );
    let err = decided.expect_err("still Maintainer");
    assert_eq!(err.code(), ErrorCode::ProtectedRefPushNotAllowed);

    let (_, decided) = push(&t, web, alice, Role::Maintainer, "refs/heads/main").await;
    decided.expect("a Maintainer may push");

    // A stricter rule tightens it: only an Owner may push main now, and the
    // looser rules still in place do not soften it.
    rule(&t, web, "req-rule-owner", "refs/heads/MA*", Role::Owner).await;
    let (protection, decided) = push(&t, web, alice, Role::Maintainer, "refs/heads/main").await;
    assert_eq!(protection.map(|p| p.push_min_role()), Some(Role::Owner));
    let err = decided.expect_err("an Owner rule matches");
    assert_eq!(err.code(), ErrorCode::ProtectedRefPushNotAllowed);
    let (_, decided) = push(&t, web, alice, Role::Owner, "refs/heads/main").await;
    decided.expect("an Owner may push");
    // A ref no rule matches stays unprotected.
    let (protection, decided) = push(&t, web, alice, Role::Developer, "refs/tags/v1").await;
    assert_eq!(protection, None);
    decided.expect("an unprotected tag");
    t.drop_now().await;
}
