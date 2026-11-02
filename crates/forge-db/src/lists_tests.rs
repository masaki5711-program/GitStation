//! The list page scans (M1 phase 8b MR7b): every row after the position in
//! id order, visible or not, with the same facts the single-object loader
//! returns; one identical statement per page whatever the rows and the
//! viewer; group roles direct, inherited and none. Fixtures go through the
//! user, group and project commands (synthetic data).

use forge_core::{RequestId, Role, Visibility};
use forge_errors::ErrorCode;

use super::*;
use crate::audited_tests::{audit, commit};
use crate::change::{Actor, ChangeContext};
use crate::groups::{self, CreatedGroup, NewGroup};
use crate::jobs::runner_stop_tests::Captured;
use crate::projects::{self, CreatedProject, NewProject};
use crate::resolve::load_project;
use crate::roles::load_group_roles;
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
        description: "synthetic".to_owned(),
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
    visibility: Visibility,
) -> CreatedProject {
    let new = NewProject {
        visibility,
        ..NewProject::new(segment, parent, &format!("Project {segment}"), creator)
    };
    projects::create(&t.db, &ctx(&format!("req-project-{segment}")), &new)
        .await
        .unwrap_or_else(|e| panic!("{segment}: {e}"))
}

fn ids(page: &[ProjectRecord]) -> Vec<ProjectId> {
    page.iter().map(|p| p.id).collect()
}

/// The sqlx statements run while `capture` was installed.
fn statements(capture: &Captured) -> Vec<String> {
    capture
        .events()
        .into_iter()
        .filter(|e| e["target"] == "sqlx::query")
        .map(|e| {
            let f = &e["fields"];
            let full = f["db.statement"].as_str().unwrap_or_default().trim();
            if full.is_empty() {
                f["summary"].as_str().unwrap_or_default().to_owned()
            } else {
                full.to_owned()
            }
        })
        .collect()
}

/// Every project after the position, in id order, bounded; invisible and
/// pending-deletion rows included, each exactly what `load_project`
/// returns for the same viewer (never filtered here).
#[tokio::test]
async fn a_project_page_is_every_row_after_the_position_with_the_loader_facts() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let a = project(
        &t,
        "a",
        acme.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;
    let b = project(&t, "b", bob.namespace_id, bob.user_id, Visibility::Private).await;
    let c = project(&t, "c", bob.namespace_id, bob.user_id, Visibility::Internal).await;
    let d = project(
        &t,
        "d",
        alice.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;
    commit(
        &t,
        &[
            audit("project", d.project_id.get(), "succeeded"),
            format!(
                "UPDATE forge.projects SET deleted_at = now(), \
                 purge_after = now() + interval '7 days' WHERE id = {}",
                d.project_id.get()
            ),
        ],
    )
    .await
    .expect("mark deleted");
    let all = [a.project_id, b.project_id, c.project_id, d.project_id];

    for viewer in [alice.user_id, bob.user_id] {
        let page = project_page(&t.db, None, viewer, 100).await.expect("page");
        assert_eq!(ids(&page), all, "every row, in id order");
        for record in &page {
            let single = load_project(&t.db, record.id, Some(viewer))
                .await
                .expect("by id")
                .expect("exists");
            assert_eq!(record, &single, "the page row is the single-object row");
        }
    }
    let page = project_page(&t.db, None, bob.user_id, 100)
        .await
        .expect("page");
    assert_eq!(page[0].access.role, None, "alice's private project, raw");
    assert_eq!(page[1].access.role, Some(Role::Owner));
    assert!(page[3].access.pending_deletion);
    assert_eq!(page[0].path.full_path(), "acme/a");

    // After a position, and bounded.
    let after = project_page(&t.db, Some(a.project_id), bob.user_id, 2)
        .await
        .expect("after");
    assert_eq!(ids(&after), [b.project_id, c.project_id]);
    let end = project_page(&t.db, Some(d.project_id), bob.user_id, 2)
        .await
        .expect("end");
    assert!(end.is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn a_group_page_carries_direct_inherited_and_no_roles() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = group(&t, "api", Some(&acme), alice.user_id).await;
    let other = group(&t, "other", None, bob.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-member-bob"),
        acme.group_id,
        bob.user_id,
        Role::Developer,
    )
    .await
    .expect("member");

    let page = group_page(&t.db, None, bob.user_id, 100)
        .await
        .expect("page");
    let got: Vec<(GroupId, Option<Role>, String)> = page
        .iter()
        .map(|g| (g.id, g.viewer_role, g.path.full_path()))
        .collect();
    assert_eq!(
        got,
        [
            (acme.group_id, Some(Role::Developer), "acme".to_owned()),
            (api.group_id, Some(Role::Developer), "acme/api".to_owned()),
            (other.group_id, Some(Role::Owner), "other".to_owned()),
        ]
    );
    for g in &page {
        let roles = load_group_roles(&t.db, g.id, bob.user_id)
            .await
            .expect("roles");
        assert_eq!(g.viewer_role, roles.native, "the role loaders' answer");
        assert_eq!(g.description, "synthetic");
        assert!(g.created_at.ends_with('Z'), "{}", g.created_at);
    }
    let stranger = user(&t, "stranger").await;
    let page = group_page(&t.db, Some(acme.group_id), stranger.user_id, 1)
        .await
        .expect("page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, api.group_id);
    assert_eq!(page[0].viewer_role, None);
    assert_eq!(page[0].namespace_id, api.namespace_id);
    t.drop_now().await;
}

/// One statement per page, the same text, whatever the page holds (full,
/// partial, empty) and whoever looks.
#[tokio::test]
async fn every_page_runs_the_same_single_statement() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let stranger = user(&t, "stranger").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let p = project(
        &t,
        "p",
        acme.namespace_id,
        alice.user_id,
        Visibility::Private,
    )
    .await;
    project_page(&t.db, None, alice.user_id, 1)
        .await
        .expect("warm");
    let cases = [
        (None, alice.user_id, "visible"),
        (None, stranger.user_id, "invisible"),
        (Some(p.project_id), stranger.user_id, "empty"),
    ];
    for (after, viewer, label) in cases {
        let capture = Captured::default();
        let guard = capture.install();
        let answer = project_page(&t.db, after, viewer, 10).await;
        drop(guard);
        answer.unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(statements(&capture), [PROJECT_PAGE_SQL], "{label}");
        let capture = Captured::default();
        let guard = capture.install();
        let answer = group_page(&t.db, after.map(|_| acme.group_id), viewer, 10).await;
        drop(guard);
        answer.unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(statements(&capture), [GROUP_PAGE_SQL], "{label}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn a_page_size_out_of_range_is_refused_before_any_statement() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    for rows in [0, MAX_PAGE_ROWS + 1] {
        let err = project_page(&t.db, None, alice.user_id, rows)
            .await
            .expect_err("out of range");
        assert_eq!(err.code(), ErrorCode::Internal);
        let err = group_page(&t.db, None, alice.user_id, rows)
            .await
            .expect_err("out of range");
        assert_eq!(err.code(), ErrorCode::Internal);
    }
    t.drop_now().await;
}

/// The trait objects forge-web holds answer exactly like the functions.
#[tokio::test]
async fn the_db_is_the_page_loader() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    project(
        &t,
        "p",
        acme.namespace_id,
        alice.user_id,
        Visibility::Internal,
    )
    .await;
    let projects: &dyn forge_core::ProjectPageLoader = &t.db;
    let groups: &dyn forge_core::GroupPageLoader = &t.db;
    assert_eq!(
        projects
            .project_page(None, alice.user_id, 5)
            .await
            .expect("trait"),
        project_page(&t.db, None, alice.user_id, 5)
            .await
            .expect("fn")
    );
    assert_eq!(
        groups
            .group_page(None, alice.user_id, 5)
            .await
            .expect("trait"),
        group_page(&t.db, None, alice.user_id, 5).await.expect("fn")
    );
    t.drop_now().await;
}
