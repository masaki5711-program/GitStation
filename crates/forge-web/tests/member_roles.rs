#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Roles inherited from ancestor groups reach `authorize()` (step D2,
//! ADR-0010): the permission-matrix test feeds roles by hand and cannot see
//! an inheritance bug, so these tests load the caller's role from the
//! database the way a member-management route does and decide with it.
//! Needs the development database (ADR-0011).

use forge_auth::{
    authorize, Action, Credential, MembershipChange, MembershipScope, Principal, Resource, Role,
};
use forge_core::{RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, CreatedGroup, NewGroup};
use forge_db::roles::{load_group_roles, MemberRoles};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn user(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
    .user_id
}

async fn group(
    t: &FreshDb,
    segment: &str,
    parent: Option<&CreatedGroup>,
    owner: UserId,
) -> CreatedGroup {
    let new = NewGroup {
        segment: segment.to_owned(),
        parent: parent.map(|p| p.namespace_id),
        display_name: format!("Group {segment}"),
        description: String::new(),
        owner,
    };
    groups::create(&t.db, &ctx(&format!("req-group-{segment}")), &new)
        .await
        .expect("group")
}

/// `acme` (owned by alice) with the subgroup `acme/api`; `name` joins
/// `acme` only, with `role`. Returns (api, the new member).
async fn subgroup_with_ancestor_member(
    t: &FreshDb,
    name: &str,
    role: Role,
) -> (CreatedGroup, UserId) {
    let alice = user(t, "alice").await;
    let acme = group(t, "acme", None, alice).await;
    let api = group(t, "api", Some(&acme), alice).await;
    let member = user(t, name).await;
    groups::add_member(&t.db, &ctx("req-add-ancestor"), acme.group_id, member, role)
        .await
        .expect("add to acme");
    (api, member)
}

/// The decision of the member-management route: the caller's role on the
/// group is loaded, never taken from the request.
async fn manage_members(
    t: &FreshDb,
    caller: UserId,
    on: &CreatedGroup,
    change: MembershipChange,
) -> Result<MemberRoles, ForgeError> {
    let roles = load_group_roles(&t.db, on.group_id, caller)
        .await
        .expect("load roles");
    let principal = Principal::authenticated(caller, false, true, Credential::Session);
    let resource = Resource::Membership {
        scope: MembershipScope::Group { role: roles.native },
        change,
        removes_last_owner: false,
    };
    authorize(&principal, Action::MemberManage, &resource).map(|()| roles)
}

#[tokio::test]
async fn ancestor_owner_may_manage_subgroup_members() {
    let t = FreshDb::migrated().await;
    let (api, carol) = subgroup_with_ancestor_member(&t, "carol", Role::Owner).await;

    for change in [
        MembershipChange::Add(Role::Maintainer),
        MembershipChange::Change {
            from: Role::Developer,
            to: Role::Owner,
        },
        MembershipChange::Remove { from: Role::Owner },
    ] {
        let roles = manage_members(&t, carol, &api, change)
            .await
            .unwrap_or_else(|e| panic!("{change:?}: {e}"));
        // Inherited only: a decision on the direct role would deny.
        assert_eq!(roles.direct, None);
    }
    t.drop_now().await;
}

#[tokio::test]
async fn a_direct_role_below_the_inherited_one_does_not_cap_it() {
    let t = FreshDb::migrated().await;
    let (api, gina) = subgroup_with_ancestor_member(&t, "gina", Role::Owner).await;
    groups::add_member(
        &t.db,
        &ctx("req-add-direct"),
        api.group_id,
        gina,
        Role::Guest,
    )
    .await
    .expect("add to api");

    let roles = manage_members(&t, gina, &api, MembershipChange::Add(Role::Owner))
        .await
        .expect("the inherited Owner decides");
    assert_eq!(roles.direct, Some(Role::Guest));
    t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_ancestor_owner_may_not() {
    let t = FreshDb::migrated().await;
    let (api, hana) = subgroup_with_ancestor_member(&t, "hana", Role::Owner).await;
    let outcome = users::disable(&t.db, &ctx("req-disable-hana"), hana)
        .await
        .expect("disable");
    assert_eq!(outcome, users::DisableOutcome::Disabled);

    // Even a principal built as active (a stale session) gets no role.
    let err = manage_members(&t, hana, &api, MembershipChange::Add(Role::Guest))
        .await
        .expect_err("a disabled member holds no role");
    assert_eq!(err.code(), ErrorCode::Forbidden, "{err}");
    assert!(err.to_string().contains("actual_role=none"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_non_member_may_not() {
    let t = FreshDb::migrated().await;
    let (api, _) = subgroup_with_ancestor_member(&t, "ivan", Role::Owner).await;
    let jun = user(&t, "jun").await;

    let err = manage_members(&t, jun, &api, MembershipChange::Add(Role::Guest))
        .await
        .expect_err("no role, no management");
    assert_eq!(err.code(), ErrorCode::Forbidden, "{err}");
    assert!(err.to_string().contains("actual_role=none"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn ancestor_reporter_may_not() {
    let t = FreshDb::migrated().await;
    let (api, erin) = subgroup_with_ancestor_member(&t, "erin", Role::Reporter).await;

    let err = manage_members(&t, erin, &api, MembershipChange::Add(Role::Guest))
        .await
        .expect_err("a reporter cannot manage members");
    assert_eq!(err.code(), ErrorCode::Forbidden, "{err}");
    assert!(err.to_string().contains("actual_role=reporter"), "{err}");
    t.drop_now().await;
}
