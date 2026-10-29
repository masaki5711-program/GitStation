//! Loading roles against a migrated database: inherited roles from ancestor
//! groups, direct roles, disabled members, the depth cap. Fixtures are
//! written through the group and user commands.

use forge_core::namespace_path::MAX_DEPTH;
use forge_core::RequestId;
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::groups::{self, CreatedGroup, NewGroup};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

/// An active human named `name`, with their personal namespace.
async fn user(t: &FreshDb, name: &str) -> users::Created {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
}

/// A group `segment` below `parent` (top level when `None`), owned by `owner`.
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
        .unwrap_or_else(|e| panic!("{segment}: {e}"))
}

async fn add(t: &FreshDb, g: &CreatedGroup, user: UserId, role: Role) {
    groups::add_member(
        &t.db,
        &ctx(&format!("req-add-{}-{}", g.group_id.get(), user.get())),
        g.group_id,
        user,
        role,
    )
    .await
    .expect("add member");
}

/// Both loaders agree on a group; returns their answer.
async fn roles(t: &FreshDb, g: &CreatedGroup, user: UserId) -> MemberRoles {
    let by_group = load_group_roles(&t.db, g.group_id, user)
        .await
        .expect("load_group_roles");
    let by_namespace = load_namespace_roles(&t.db, g.namespace_id, user)
        .await
        .expect("load_namespace_roles");
    assert_eq!(by_group, by_namespace, "the two loaders disagree");
    by_group
}

fn member(native: Option<Role>, direct: Option<Role>) -> MemberRoles {
    MemberRoles { native, direct }
}

#[tokio::test]
async fn native_role_is_max_over_ancestor_groups() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let bob = user(&t, "bob").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    let core = group(&t, "core", Some(&api), alice).await;
    add(&t, &acme, bob, Role::Reporter).await;
    add(&t, &api, bob, Role::Maintainer).await;
    add(&t, &core, bob, Role::Guest).await;

    // The highest of Reporter (acme), Maintainer (api), Guest (core).
    assert_eq!(
        roles(&t, &core, bob).await,
        member(Some(Role::Maintainer), Some(Role::Guest))
    );
    assert_eq!(
        roles(&t, &api, bob).await,
        member(Some(Role::Maintainer), Some(Role::Maintainer))
    );
    // A descendant's role never flows up.
    assert_eq!(
        roles(&t, &acme, bob).await,
        member(Some(Role::Reporter), Some(Role::Reporter))
    );
    t.drop_now().await;
}

#[tokio::test]
async fn direct_role_ignores_ancestors() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let carol = user(&t, "carol").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    add(&t, &acme, carol, Role::Owner).await;

    assert_eq!(
        roles(&t, &api, carol).await,
        member(Some(Role::Owner), None)
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_non_member_has_no_role() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let dave = user(&t, "dave").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    // Another group's membership does not count.
    let other = group(&t, "other", None, dave).await;

    assert_eq!(roles(&t, &api, dave).await, MemberRoles::default());
    assert_eq!(
        roles(&t, &other, dave).await,
        member(Some(Role::Owner), Some(Role::Owner))
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_member_contributes_no_role() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let bob = user(&t, "bob").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    add(&t, &acme, bob, Role::Maintainer).await;
    add(&t, &api, bob, Role::Developer).await;
    let outcome = users::disable(&t.db, &ctx("req-disable-bob"), bob)
        .await
        .expect("disable");
    assert_eq!(outcome, users::DisableOutcome::Disabled);

    assert_eq!(roles(&t, &api, bob).await, MemberRoles::default());
    assert_eq!(roles(&t, &acme, bob).await, MemberRoles::default());
    // The rows stay: a disabled member is still a member, without a role.
    let mut su = t.superuser().await;
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM forge.group_members WHERE user_id = $1")
            .bind(bob.get())
            .fetch_one(&mut su)
            .await
            .expect("count");
    assert_eq!(rows, 2);
    t.drop_now().await;
}

#[tokio::test]
async fn depth_20_chain_resolves() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let bob = user(&t, "bob").await.user_id;
    let top = group(&t, "g1", None, alice).await;
    add(&t, &top, bob, Role::Reporter).await;
    let mut leaf = top;
    for depth in 2..=MAX_DEPTH {
        leaf = group(&t, &format!("g{depth}"), Some(&leaf), alice).await;
    }

    assert_eq!(
        roles(&t, &leaf, bob).await,
        member(Some(Role::Reporter), None)
    );
    assert_eq!(
        roles(&t, &leaf, alice).await,
        member(Some(Role::Owner), Some(Role::Owner))
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_ancestor_role_above_the_direct_one_wins() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let frank = user(&t, "frank").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    let core = group(&t, "core", Some(&api), alice).await;
    add(&t, &acme, frank, Role::Owner).await;
    add(&t, &api, frank, Role::Developer).await;
    add(&t, &core, frank, Role::Guest).await;

    // The farthest ancestor wins, not the nearest membership.
    assert_eq!(
        roles(&t, &core, frank).await,
        member(Some(Role::Owner), Some(Role::Guest))
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_namespace_chain_beyond_the_cap_is_membership_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    let core = group(&t, "core", Some(&api), alice).await;
    // A corrupt parent chain (api <-> core) that no command can write; the
    // audit triggers are off for the tamper only (this database is dropped).
    t.exec_superuser(&format!(
        "ALTER TABLE forge.namespaces DISABLE TRIGGER USER; \
         UPDATE forge.namespaces SET parent_id = {} WHERE id = {}; \
         ALTER TABLE forge.namespaces ENABLE TRIGGER USER",
        core.namespace_id.get(),
        api.namespace_id.get()
    ))
    .await;

    let err = load_group_roles(&t.db, core.group_id, alice)
        .await
        .expect_err("a cycle never resolves");
    assert_reason(
        &err,
        ErrorCode::MembershipDataInvalid,
        "namespace_chain_too_deep",
    );
    let shown = err.to_string();
    assert!(shown.contains("resource=group"), "{shown}");
    assert!(shown.contains(&format!("user={}", alice.get())), "{shown}");
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_stored_role_is_membership_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    // A value the CHECK refuses; the constraint and the audit triggers are
    // dropped / off for the tamper only (this database is dropped).
    t.exec_superuser(&format!(
        "ALTER TABLE forge.group_members DROP CONSTRAINT group_members_role; \
         ALTER TABLE forge.group_members DISABLE TRIGGER USER; \
         UPDATE forge.group_members SET role = 'superuser' WHERE group_id = {}; \
         ALTER TABLE forge.group_members ENABLE TRIGGER USER",
        acme.group_id.get()
    ))
    .await;

    // Found on an ancestor row: the whole load fails, no partial role.
    let err = load_namespace_roles(&t.db, api.namespace_id, alice)
        .await
        .expect_err("an unknown role is never ignored");
    assert_reason(&err, ErrorCode::MembershipDataInvalid, "unknown_role");
    assert!(err.to_string().contains("raw=superuser"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_personal_namespace_carries_no_group_role() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    // alice's user id may equal a group id: a personal namespace's owner_id
    // is a user id and must never be read as a group.
    let acme = group(&t, "acme", None, alice.user_id).await;
    assert_eq!(acme.group_id.get(), alice.user_id.get());

    let roles = load_namespace_roles(&t.db, alice.namespace_id, alice.user_id)
        .await
        .expect("load");
    assert_eq!(roles, MemberRoles::default());
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_group_or_namespace_is_not_found() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;

    let err = load_group_roles(&t.db, GroupId::new(424_242), alice)
        .await
        .expect_err("no such group");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    let err = load_namespace_roles(&t.db, NamespaceId::new(424_242), alice)
        .await
        .expect_err("no such namespace");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    t.drop_now().await;
}
