//! Plan 15 MR 2: the chain access loaders — the roles of the role loaders
//! plus the effective (narrowest) visibility of the chain and whether the
//! user holds a role somewhere strictly below the anchor
//! (`has_visible_descendant`). Nothing decides with them yet (MR 3).
//! Fixtures are written through the user, group and project commands;
//! visibilities and deletions with their audit rows, as forge_app.

use forge_core::ids::ProjectId;
use forge_core::namespace_path::MAX_DEPTH;
use forge_core::{RequestId, Visibility};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::groups::{self, CreatedGroup, NewGroup};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn user(t: &FreshDb, name: &str) -> users::Created {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
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
        .unwrap_or_else(|e| panic!("{segment}: {e}"))
}

async fn project(
    t: &FreshDb,
    segment: &str,
    parent: NamespaceId,
    creator: UserId,
) -> CreatedProject {
    let new = NewProject::new(segment, parent, &format!("Project {segment}"), creator);
    projects::create(&t.db, &ctx(&format!("req-project-{segment}")), &new)
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

async fn set_group(t: &FreshDb, g: &CreatedGroup, v: Visibility) {
    t.commit_audited(
        ("group", g.group_id.get()),
        &[format!(
            "UPDATE forge.groups SET visibility = '{}' WHERE id = {}",
            v.as_str(),
            g.group_id.get()
        )],
    )
    .await;
}

async fn set_project(t: &FreshDb, p: ProjectId, v: Visibility) {
    t.commit_audited(
        ("project", p.get()),
        &[format!(
            "UPDATE forge.projects SET visibility = '{}' WHERE id = {}",
            v.as_str(),
            p.get()
        )],
    )
    .await;
}

/// The group and namespace access loaders agree on a group (and their
/// roles are the role loaders'); returns their answer.
async fn group_access(t: &FreshDb, g: &CreatedGroup, user: UserId) -> ChainAccess {
    let by_group = load_group_access(&t.db, g.group_id, user)
        .await
        .expect("load_group_access");
    let by_namespace = load_namespace_access(&t.db, g.namespace_id, user)
        .await
        .expect("load_namespace_access");
    assert_eq!(by_group, by_namespace, "the two loaders disagree");
    let roles = load_group_roles(&t.db, g.group_id, user)
        .await
        .expect("load_group_roles");
    assert_eq!(
        by_group.roles, roles,
        "the roles differ from the role loader"
    );
    by_group
}

/// As [`group_access`] for a project.
async fn project_access(t: &FreshDb, p: &CreatedProject, user: UserId) -> ChainAccess {
    let by_project = load_project_access(&t.db, p.project_id, user)
        .await
        .expect("load_project_access");
    let by_namespace = load_namespace_access(&t.db, p.namespace_id, user)
        .await
        .expect("load_namespace_access");
    assert_eq!(by_project, by_namespace, "the two loaders disagree");
    let roles = load_project_roles(&t.db, p.project_id, user)
        .await
        .expect("load_project_roles");
    assert_eq!(
        by_project.roles, roles,
        "the roles differ from the role loader"
    );
    by_project
}

#[tokio::test]
async fn groups_are_internal_unless_set_otherwise() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    // The migration's default: what every group created before it reads.
    let mut su = t.superuser().await;
    let default: String = sqlx::query_scalar(
        "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
         JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
         WHERE d.adrelid = 'forge.groups'::regclass AND a.attname = 'visibility'",
    )
    .fetch_one(&mut su)
    .await
    .expect("default");
    assert_eq!(default, "'internal'::text");
    drop(su);
    for g in [&acme, &api] {
        let a = group_access(&t, g, alice).await;
        assert_eq!(a.visibility, Some(Visibility::Internal));
    }
    t.drop_now().await;
}

#[tokio::test]
async fn the_effective_visibility_is_the_narrowest_on_the_chain() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let bob = user(&t, "bob").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    let core = group(&t, "core", Some(&api), alice).await;
    let web = project(&t, "web", core.namespace_id, alice).await;
    set_project(&t, web.project_id, Visibility::Internal).await;
    set_group(&t, &api, Visibility::Private).await;

    // The value is the chain's, whoever asks (a non-member included).
    for who in [alice, bob] {
        assert_eq!(
            group_access(&t, &acme, who).await.visibility,
            Some(Visibility::Internal)
        );
        assert_eq!(
            group_access(&t, &api, who).await.visibility,
            Some(Visibility::Private)
        );
        // core is stored internal; its parent narrows it.
        assert_eq!(
            group_access(&t, &core, who).await.visibility,
            Some(Visibility::Private)
        );
        // An internal project below a private group is private too.
        assert_eq!(
            project_access(&t, &web, who).await.visibility,
            Some(Visibility::Private)
        );
    }
    // Widening the group again widens everything below it; the project's
    // own value still counts.
    set_group(&t, &api, Visibility::Internal).await;
    assert_eq!(
        project_access(&t, &web, bob).await.visibility,
        Some(Visibility::Internal)
    );
    set_project(&t, web.project_id, Visibility::Private).await;
    assert_eq!(
        project_access(&t, &web, bob).await.visibility,
        Some(Visibility::Private)
    );
    assert_eq!(
        group_access(&t, &core, bob).await.visibility,
        Some(Visibility::Internal)
    );
    t.drop_now().await;
}

/// Every combination on a three-group chain with a project: the effective
/// value is never wider than any step, and is internal only when every
/// step is.
#[tokio::test]
async fn the_effective_visibility_is_never_wider_than_any_step() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let carol = user(&t, "carol").await.user_id;
    let top = group(&t, "top", None, alice).await;
    let mid = group(&t, "mid", Some(&top), alice).await;
    let low = group(&t, "low", Some(&mid), alice).await;
    let p = project(&t, "p", low.namespace_id, alice).await;
    let vis = |bit: bool| {
        if bit {
            Visibility::Internal
        } else {
            Visibility::Private
        }
    };
    for mask in 0u8..16 {
        let steps = [mask & 1 != 0, mask & 2 != 0, mask & 4 != 0, mask & 8 != 0];
        set_group(&t, &top, vis(steps[0])).await;
        set_group(&t, &mid, vis(steps[1])).await;
        set_group(&t, &low, vis(steps[2])).await;
        set_project(&t, p.project_id, vis(steps[3])).await;
        let all = |n: usize| vis(steps[..n].iter().all(|b| *b));
        for (n, got) in [
            (1, group_access(&t, &top, carol).await.visibility),
            (2, group_access(&t, &mid, carol).await.visibility),
            (3, group_access(&t, &low, carol).await.visibility),
            (4, project_access(&t, &p, carol).await.visibility),
        ] {
            assert_eq!(got, Some(all(n)), "mask {mask:04b}, depth {n}");
        }
    }
    t.drop_now().await;
}

#[tokio::test]
async fn a_role_strictly_below_is_a_visible_descendant() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let bob = user(&t, "bob").await.user_id;
    let carol = user(&t, "carol").await.user_id;
    let dave = user(&t, "dave").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    let core = group(&t, "core", Some(&api), alice).await;
    let other = group(&t, "other", None, alice).await;
    set_group(&t, &acme, Visibility::Private).await;
    // bob: a member of the deepest group only.
    add(&t, &core, bob, Role::Guest).await;
    // carol: a direct member of a project in api (creator = direct Owner).
    let web = project(&t, "web", api.namespace_id, carol).await;

    let below = |a: ChainAccess| a.has_visible_descendant;
    assert!(below(group_access(&t, &acme, bob).await));
    assert!(below(group_access(&t, &api, bob).await));
    // Strictly below: core itself is bob's own membership, not a descendant.
    assert!(!below(group_access(&t, &core, bob).await));
    assert!(!below(group_access(&t, &other, bob).await));
    // bob has no role on acme: only the pass-through flag says he may see it.
    assert_eq!(
        group_access(&t, &acme, bob).await.roles,
        MemberRoles::default()
    );

    assert!(below(group_access(&t, &acme, carol).await));
    assert!(below(group_access(&t, &api, carol).await));
    assert!(!below(group_access(&t, &core, carol).await));
    // A project has nothing below it.
    assert!(!below(project_access(&t, &web, carol).await));

    // dave holds nothing anywhere.
    for g in [&acme, &api, &core, &other] {
        assert!(!below(group_access(&t, g, dave).await));
    }
    // alice owns acme (an ancestor): she sees it by her role; the groups
    // she created below are her direct memberships, so acme has some.
    assert!(below(group_access(&t, &acme, alice).await));
    t.drop_now().await;
}

#[tokio::test]
async fn a_deleted_project_or_a_disabled_user_is_no_visible_descendant() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let bob = user(&t, "bob").await.user_id;
    let erin = user(&t, "erin").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let gone = project(&t, "gone", acme.namespace_id, bob).await;
    let sub = group(&t, "sub", Some(&acme), alice).await;
    add(&t, &sub, erin, Role::Developer).await;
    assert!(group_access(&t, &acme, bob).await.has_visible_descendant);
    assert!(group_access(&t, &acme, erin).await.has_visible_descendant);

    t.commit_audited(
        ("project", gone.project_id.get()),
        &[format!(
            "UPDATE forge.projects SET deleted_at = now(), \
             purge_after = now() + interval '30 days' WHERE id = {}",
            gone.project_id.get()
        )],
    )
    .await;
    assert!(!group_access(&t, &acme, bob).await.has_visible_descendant);

    let outcome = users::disable(&t.db, &ctx("req-disable-erin"), erin)
        .await
        .expect("disable");
    assert!(
        matches!(outcome, users::DisableOutcome::Disabled),
        "{outcome:?}"
    );
    assert!(!group_access(&t, &acme, erin).await.has_visible_descendant);
    t.drop_now().await;
}

#[tokio::test]
async fn depth_20_chain_carries_visibility_and_descendants() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let bob = user(&t, "bob").await.user_id;
    let top = group(&t, "g1", None, alice).await;
    let mut chain = vec![top];
    for depth in 2..=MAX_DEPTH {
        let parent = chain.last().expect("parent");
        let next = group(&t, &format!("g{depth}"), Some(parent), alice).await;
        chain.push(next);
    }
    let (top, leaf) = (&chain[0], &chain[MAX_DEPTH - 1]);
    add(&t, leaf, bob, Role::Reporter).await;

    // Private only at the top: the leaf, 20 steps down, is private.
    set_group(&t, top, Visibility::Private).await;
    let a = group_access(&t, leaf, bob).await;
    assert_eq!(a.visibility, Some(Visibility::Private));
    assert_eq!(a.roles.direct, Some(Role::Reporter));
    // bob's membership 19 steps below the top is found from the top.
    let a = group_access(&t, top, bob).await;
    assert!(a.has_visible_descendant);
    assert_eq!(a.visibility, Some(Visibility::Private));

    // Private only at the leaf: the top stays internal.
    set_group(&t, top, Visibility::Internal).await;
    set_group(&t, leaf, Visibility::Private).await;
    assert_eq!(
        group_access(&t, top, bob).await.visibility,
        Some(Visibility::Internal)
    );
    assert_eq!(
        group_access(&t, leaf, bob).await.visibility,
        Some(Visibility::Private)
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_personal_namespace_has_no_visibility_of_its_own() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let a = load_namespace_access(&t.db, alice.namespace_id, alice.user_id)
        .await
        .expect("personal");
    assert_eq!(a.visibility, None);
    assert_eq!(a.roles, MemberRoles::default());
    // A project directly in it: its own value only.
    let web = project(&t, "web", alice.namespace_id, alice.user_id).await;
    assert_eq!(
        project_access(&t, &web, alice.user_id).await.visibility,
        Some(Visibility::Private)
    );
    // The flag is a fact (alice holds the project's direct Owner role, one
    // step below); authorize() decides a personal namespace by ownership
    // (Resource::Namespace), never by passing through.
    assert!(
        load_namespace_access(&t.db, alice.namespace_id, alice.user_id)
            .await
            .expect("personal")
            .has_visible_descendant
    );
    let bob = user(&t, "bob").await;
    assert!(
        !load_namespace_access(&t.db, alice.namespace_id, bob.user_id)
            .await
            .expect("personal")
            .has_visible_descendant
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_stored_visibility_is_membership_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    // A value the CHECK refuses; the constraint and the audit triggers are
    // dropped / off for the tamper only (this database is dropped).
    t.exec_superuser(&format!(
        "ALTER TABLE forge.groups DROP CONSTRAINT groups_visibility; \
         ALTER TABLE forge.groups DISABLE TRIGGER USER; \
         UPDATE forge.groups SET visibility = 'public' WHERE id = {}; \
         ALTER TABLE forge.groups ENABLE TRIGGER USER",
        acme.group_id.get()
    ))
    .await;

    // Found on an ancestor row: the whole load fails, never a guess.
    let err = load_group_access(&t.db, api.group_id, alice)
        .await
        .expect_err("an unknown visibility is never ignored");
    assert_reason(&err, ErrorCode::MembershipDataInvalid, "unknown_visibility");
    let shown = err.to_string();
    assert!(shown.contains("raw=public"), "{shown}");
    assert!(shown.contains("resource=group"), "{shown}");
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_anchor_is_not_found() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let err = load_group_access(&t.db, GroupId::new(424_242), alice)
        .await
        .expect_err("no such group");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    let err = load_namespace_access(&t.db, NamespaceId::new(424_242), alice)
        .await
        .expect_err("no such namespace");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    let err = load_project_access(&t.db, ProjectId::new(424_242), alice)
        .await
        .expect_err("no such project");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    t.drop_now().await;
}

#[tokio::test]
async fn a_chain_beyond_the_cap_is_membership_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let acme = group(&t, "acme", None, alice).await;
    let api = group(&t, "api", Some(&acme), alice).await;
    let core = group(&t, "core", Some(&api), alice).await;
    t.exec_superuser(&format!(
        "ALTER TABLE forge.namespaces DISABLE TRIGGER USER; \
         UPDATE forge.namespaces SET parent_id = {} WHERE id = {}; \
         ALTER TABLE forge.namespaces ENABLE TRIGGER USER",
        core.namespace_id.get(),
        api.namespace_id.get()
    ))
    .await;
    let err = load_group_access(&t.db, core.group_id, alice)
        .await
        .expect_err("a cycle never resolves");
    assert_reason(
        &err,
        ErrorCode::MembershipDataInvalid,
        "namespace_chain_too_deep",
    );
    t.drop_now().await;
}
