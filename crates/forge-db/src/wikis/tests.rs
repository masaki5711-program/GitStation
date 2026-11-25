//! Plan 20 W1 (ADR-0040 §5): the wiki link and the role inheritance it
//! brings, through the real role computation (`roles`) on a migrated
//! database. Fixtures are written through the user, group and project
//! commands.

use forge_core::ids::{NamespaceId, ProjectId, UserId};
use forge_core::{RequestId, Role};
use forge_errors::ErrorCode;

use super::*;
use crate::audited_tests::commit;
use crate::change::{Actor, ChangeContext};
use crate::groups::{self, CreatedGroup, NewGroup};
use crate::projects::{self, CreatedProject, NewProject};
use crate::roles::{load_project_roles, MemberRoles};
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

async fn group(t: &FreshDb, segment: &str, owner: UserId) -> CreatedGroup {
    let new = NewGroup {
        visibility: forge_core::Visibility::Private,
        segment: segment.to_owned(),
        parent: None,
        display_name: format!("Group {segment}"),
        description: String::new(),
        owner,
    };
    groups::create(&t.db, &ctx(&format!("req-group-{segment}")), &new)
        .await
        .expect("group")
}

async fn project(t: &FreshDb, segment: &str, parent: NamespaceId, creator: UserId) -> ProjectId {
    let new = NewProject::new(segment, parent, &format!("Project {segment}"), creator);
    let made: CreatedProject = projects::create(&t.db, &ctx(&format!("req-p-{segment}")), &new)
        .await
        .expect("project");
    made.project_id
}

async fn member(t: &FreshDb, p: ProjectId, who: UserId, role: Role) {
    projects::add_member(&t.db, &ctx("req-member"), p, who, role)
        .await
        .expect("add member");
}

async fn link(
    t: &FreshDb,
    owner: ProjectId,
    wiki: ProjectId,
    by: UserId,
) -> Result<(), ForgeError> {
    link_decided(&t.db, &ctx("req-link"), owner, wiki, by, |_| Ok(())).await
}

async fn native(t: &FreshDb, p: ProjectId, who: UserId) -> Option<Role> {
    let MemberRoles { native, .. } = load_project_roles(&t.db, p, who)
        .await
        .expect("load_project_roles");
    native
}

fn reason(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// The shape of every test: Alice owns a group with `web` and its wiki
/// `web-wiki` (linked); Bob is a Developer of `web` only.
struct World {
    t: FreshDb,
    alice: UserId,
    bob: UserId,
    group: CreatedGroup,
    web: ProjectId,
    wiki: ProjectId,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await.user_id;
    let bob = user(&t, "bob").await.user_id;
    let g = group(&t, "team", alice).await;
    let web = project(&t, "web", g.namespace_id, alice).await;
    let wiki = project(&t, "web-wiki", g.namespace_id, alice).await;
    member(&t, web, bob, Role::Developer).await;
    link(&t, web, wiki, alice).await.expect("link");
    World {
        t,
        alice,
        bob,
        group: g,
        web,
        wiki,
    }
}

#[tokio::test]
async fn owner_project_developer_can_push_wiki() {
    let w = world().await;
    // Bob holds no role on the wiki himself: he inherits Developer.
    assert_eq!(native(&w.t, w.wiki, w.bob).await, Some(Role::Developer));
    // ... and `authorize()` lets him push it (the role is the input).
    let view = forge_auth::ProjectView {
        visibility: forge_core::Visibility::Private,
        role: native(&w.t, w.wiki, w.bob).await,
        pending_deletion: false,
    };
    let principal = forge_auth::Principal::authenticated(
        w.bob,
        false,
        true,
        forge_auth::Credential::Session,
        forge_auth::AccountKind::Human,
    );
    assert!(forge_auth::authorize(
        &principal,
        forge_auth::Action::RepoPush,
        &forge_auth::Resource::Project(view)
    )
    .is_ok());
    // A higher own role on the wiki wins over the inherited one.
    member(&w.t, w.wiki, w.bob, Role::Maintainer).await;
    assert_eq!(native(&w.t, w.wiki, w.bob).await, Some(Role::Maintainer));
    // The owner project's Owner stays Owner on the wiki.
    assert_eq!(native(&w.t, w.wiki, w.alice).await, Some(Role::Owner));
}

#[tokio::test]
async fn wiki_only_member_gets_nothing_on_owner() {
    let w = world().await;
    let carol = user(&w.t, "carol").await.user_id;
    member(&w.t, w.wiki, carol, Role::Maintainer).await;
    assert_eq!(native(&w.t, w.wiki, carol).await, Some(Role::Maintainer));
    // One way: the wiki gives nothing on the owner project.
    assert_eq!(native(&w.t, w.web, carol).await, None);
}

#[tokio::test]
async fn inheritance_not_transitive() {
    let w = world().await;
    let notes = project(&w.t, "notes", w.group.namespace_id, w.alice).await;
    let other = project(&w.t, "other", w.group.namespace_id, w.alice).await;
    // The wiki cannot own a wiki, the owner cannot be a wiki, a project is
    // never its own wiki: FG-KNOW-0003, nothing written.
    let e = link(&w.t, w.wiki, notes, w.alice)
        .await
        .expect_err("wiki owns");
    assert_eq!(e.code(), ErrorCode::WikiLinkRefused);
    assert_eq!(reason(&e), "chain");
    let e = link(&w.t, other, w.web, w.alice)
        .await
        .expect_err("owner as wiki");
    assert_eq!(e.code(), ErrorCode::WikiLinkRefused);
    assert_eq!(reason(&e), "chain");
    let e = link(&w.t, notes, notes, w.alice).await.expect_err("self");
    assert_eq!(reason(&e), "self");
    let e = link(&w.t, w.web, notes, w.alice)
        .await
        .expect_err("second wiki");
    assert_eq!(reason(&e), "already_linked");
    let e = link(&w.t, notes, w.wiki, w.alice)
        .await
        .expect_err("wiki twice");
    assert_eq!(reason(&e), "wiki_already_linked");
    // The table itself refuses a chain written around the command.
    let raw = commit(
        &w.t,
        &[format!(
            "INSERT INTO forge.project_wikis (project_id, wiki_project_id, linked_by) \
             VALUES ({}, {}, {})",
            w.wiki.get(),
            notes.get(),
            w.alice.get()
        )],
    )
    .await;
    assert!(raw.is_err(), "the trigger refuses a chain");
    // Bob (Developer of web) inherits on the wiki only, never further.
    assert_eq!(native(&w.t, notes, w.bob).await, None);
    assert_eq!(native(&w.t, other, w.bob).await, None);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM forge.project_wikis")
        .fetch_one(w.t.db.pool().executor())
        .await
        .expect("count");
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn removed_member_loses_wiki_next_request() {
    let w = world().await;
    assert_eq!(native(&w.t, w.wiki, w.bob).await, Some(Role::Developer));
    projects::remove_member(&w.t.db, &ctx("req-remove"), w.web, w.bob, Role::Developer)
        .await
        .expect("remove");
    // Nothing is cached: the next read has no role.
    assert_eq!(native(&w.t, w.wiki, w.bob).await, None);
    // Unlinking removes the inheritance the same way.
    member(&w.t, w.web, w.bob, Role::Reporter).await;
    assert_eq!(native(&w.t, w.wiki, w.bob).await, Some(Role::Reporter));
    let (wiki, ()) = unlink_decided(&w.t.db, &ctx("req-unlink"), w.web, w.alice, |_| Ok(()))
        .await
        .expect("unlink");
    assert_eq!(wiki, w.wiki);
    assert_eq!(native(&w.t, w.wiki, w.bob).await, None);
}

#[tokio::test]
async fn inherited_role_is_the_one_decided_inside_writes() {
    let w = world().await;
    let _changed = projects::change_member(
        &w.t.db,
        &ctx("req-promote"),
        w.web,
        w.bob,
        Role::Developer,
        Role::Maintainer,
    )
    .await
    .expect("promote");
    // The write transaction's re-read (`lock_roles`) sees the same role.
    let ((), role) = {
        let facts = std::sync::Mutex::new(None);
        let r = link_decided(&w.t.db, &ctx("req-x"), w.wiki, w.wiki, w.bob, |f| {
            *facts.lock().expect("lock") = Some(f.project.access.role);
            Err::<(), _>(ForgeError::new(ErrorCode::Forbidden))
        })
        .await;
        assert!(r.is_err());
        ((), facts.into_inner().expect("lock"))
    };
    assert_eq!(role, Some(Some(Role::Maintainer)));
}

#[tokio::test]
async fn link_needs_the_same_namespace_and_a_live_wiki() {
    let w = world().await;
    let elsewhere = project(
        &w.t,
        "elsewhere",
        user(&w.t, "dave").await.namespace_id,
        w.alice,
    )
    .await;
    let fresh = project(&w.t, "fresh", w.group.namespace_id, w.alice).await;
    let e = link(&w.t, fresh, elsewhere, w.alice)
        .await
        .expect_err("other ns");
    assert_eq!(reason(&e), "other_namespace");
    let got = link_of(&w.t.db, w.wiki)
        .await
        .expect("read")
        .expect("linked");
    assert_eq!(
        (got.project, got.wiki, got.linked_by),
        (w.web, w.wiki, w.alice)
    );
}
