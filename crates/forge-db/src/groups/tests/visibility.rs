//! Group visibility (plan 15 MR 3): the explicit value on creation, the
//! ceiling of a private chain (`FG-REPO-0033`), the deciding loaders'
//! effective visibility and pass-through facts, and the change decided in
//! its transaction.

use forge_core::{GroupSight, Role, Visibility};
use forge_errors::ForgeError;

use super::*;
use crate::projects::{self, NewProject};

fn private(mut new: NewGroup) -> NewGroup {
    new.visibility = Visibility::Private;
    new
}

/// acme (private) ⊃ team; `alice` owns both.
async fn private_acme(t: &FreshDb) -> (CreatedGroup, UserId) {
    let alice = user(t, "alice").await;
    let acme = create_ok(t, &private(group("acme", None, alice))).await;
    (acme, alice)
}

fn allow(_: &GroupUpdateFacts) -> Result<(), ForgeError> {
    Ok(())
}

#[tokio::test]
async fn the_visibility_is_written_explicitly_and_never_wider_than_the_parent() {
    let t = FreshDb::migrated().await;
    let (acme, alice) = private_acme(&t).await;
    let mut su = t.superuser().await;
    let stored: String = sqlx::query_scalar("SELECT visibility FROM forge.groups WHERE id = $1")
        .bind(acme.group_id.get())
        .fetch_one(&mut su)
        .await
        .expect("stored");
    assert_eq!(stored, "private");

    let err = create(
        &t.db,
        &ctx("req-open"),
        &group("open", Some(acme.namespace_id), alice),
    )
    .await
    .expect_err("internal below private");
    assert_eq!(err.code(), ErrorCode::VisibilityAboveParent);
    assert!(err.to_string().contains("group_above_private"), "{err}");
    let err = projects::create(
        &t.db,
        &ctx("req-open-project"),
        &NewProject {
            visibility: Visibility::Internal,
            ..NewProject::new("app", acme.namespace_id, "app", alice)
        },
    )
    .await
    .expect_err("internal project below private");
    assert_eq!(err.code(), ErrorCode::VisibilityAboveParent);
    // Nothing was written.
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.namespaces WHERE full_path LIKE 'acme/%'"
        )
        .await,
        0
    );
    // Private below private is fine.
    create_ok(
        &t,
        &private(group("closed", Some(acme.namespace_id), alice)),
    )
    .await;
}

#[tokio::test]
async fn the_loaders_fold_the_chain_and_see_roles_below() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let carol = user(&t, "carol").await;
    let bob = user(&t, "bob").await;
    let acme = create_ok(&t, &group("acme", None, alice)).await;
    let team = create_ok(&t, &group("team", Some(acme.namespace_id), alice)).await;
    let app = projects::create(
        &t.db,
        &ctx("req-app"),
        &NewProject {
            visibility: Visibility::Internal,
            ..NewProject::new("app", team.namespace_id, "app", alice)
        },
    )
    .await
    .expect("app");
    projects::add_member(&t.db, &ctx("req-carol"), app.project_id, carol, Role::Guest)
        .await
        .expect("carol");

    // Narrow acme: team and app keep their stored internal.
    let (outcome, ()) = change_visibility_decided(
        &t.db,
        &ctx("req-narrow"),
        acme.group_id,
        alice,
        Visibility::Private,
        false,
        allow,
    )
    .await
    .expect("narrow");
    assert!(outcome.changed);
    assert_eq!(outcome.affected_projects, 1);

    let team_for = |who| load(&t.db, team.group_id, Some(who));
    let as_bob = team_for(bob).await.expect("load").expect("team");
    assert_eq!(as_bob.own_visibility, Visibility::Internal);
    assert_eq!(
        as_bob.sight,
        GroupSight {
            visibility: Visibility::Private,
            descendant: false,
        }
    );
    let as_carol = team_for(carol).await.expect("load").expect("team");
    assert!(as_carol.sight.descendant, "{as_carol:?}");
    assert_eq!(as_carol.role, None);
    // The project's effective visibility: private, for everyone.
    let record = crate::resolve::load_project(&t.db, app.project_id, Some(bob))
        .await
        .expect("load")
        .expect("app");
    assert_eq!(record.access.visibility, Visibility::Private);
    // The group page agrees with the single loader.
    let page = crate::lists::group_page(&t.db, None, carol, 100)
        .await
        .expect("page");
    let team_row = page
        .iter()
        .find(|g| g.id == team.group_id)
        .expect("team row");
    assert_eq!(team_row.sight, as_carol.sight);
    assert_eq!(team_row.own_visibility, Visibility::Internal);
}

#[tokio::test]
async fn a_preview_counts_without_writing_and_widening_below_private_is_refused() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = create_ok(&t, &group("acme", None, alice)).await;
    let team = create_ok(&t, &group("team", Some(acme.namespace_id), alice)).await;
    let closed = create_ok(
        &t,
        &private(group("closed", Some(acme.namespace_id), alice)),
    )
    .await;
    for (segment, parent) in [("a", &team), ("b", &acme), ("c", &closed)] {
        projects::create(
            &t.db,
            &ctx(&format!("req-p-{segment}")),
            &NewProject {
                visibility: if segment == "c" {
                    Visibility::Private
                } else {
                    Visibility::Internal
                },
                ..NewProject::new(segment, parent.namespace_id, segment, alice)
            },
        )
        .await
        .expect("project");
    }
    let mut su = t.superuser().await;
    let (preview, ()) = change_visibility_decided(
        &t.db,
        &ctx("req-preview"),
        acme.group_id,
        alice,
        Visibility::Private,
        true,
        allow,
    )
    .await
    .expect("preview");
    // a (via internal team) and b; c is below a private group already.
    assert_eq!((preview.changed, preview.affected_projects), (false, 2));
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'group.visibility_changed'"
        )
        .await,
        0
    );
    let (done, ()) = change_visibility_decided(
        &t.db,
        &ctx("req-change"),
        acme.group_id,
        alice,
        Visibility::Private,
        false,
        allow,
    )
    .await
    .expect("change");
    assert_eq!((done.changed, done.from), (true, Visibility::Internal));
    let err = change_visibility_decided(
        &t.db,
        &ctx("req-widen"),
        team.group_id,
        alice,
        Visibility::Internal,
        false,
        allow,
    )
    .await
    .expect_err("team below private acme");
    assert_eq!(err.code(), ErrorCode::VisibilityAboveParent);
    // The refusal comes after the decision: a denial wins.
    let err = change_visibility_decided(
        &t.db,
        &ctx("req-denied"),
        team.group_id,
        alice,
        Visibility::Internal,
        false,
        |_| -> Result<(), ForgeError> { Err(ForgeError::new(ErrorCode::Forbidden)) },
    )
    .await
    .expect_err("denied");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    // Unchanged: nothing written, nothing affected.
    let (same, ()) = change_visibility_decided(
        &t.db,
        &ctx("req-same"),
        acme.group_id,
        alice,
        Visibility::Private,
        false,
        allow,
    )
    .await
    .expect("same");
    assert_eq!((same.changed, same.affected_projects), (false, 0));
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'group.visibility_changed'"
        )
        .await,
        1
    );
    let wider = wider_than_parent(&t.db)
        .await
        .expect("count")
        .expect("migrated");
    // team (internal below acme), a and b (internal below acme).
    assert_eq!((wider.groups, wider.projects), (1, 2));
}

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(
        crate::change::Actor::User(user),
        RequestId::parse(request).expect("request id"),
    )
}

fn options() -> crate::namespaces::MoveOptions {
    crate::namespaces::MoveOptions {
        reclaim: false,
        rate_limit: crate::namespaces::RateLimit::Enforce,
    }
}

/// Review of plan 15 MR 3: a move out of a private group never widens —
/// the moved row is narrowed to its effective visibility first.
#[tokio::test]
async fn moving_out_of_a_private_group_lands_private() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = create_ok(&t, &group("acme", None, alice)).await;
    let open = create_ok(&t, &group("open", None, alice)).await;
    let team = create_ok(&t, &group("team", Some(acme.namespace_id), alice)).await;
    let app = projects::create(
        &t.db,
        &ctx("req-app"),
        &NewProject {
            visibility: Visibility::Internal,
            ..NewProject::new("app", acme.namespace_id, "app", alice)
        },
    )
    .await
    .expect("app");
    let (narrowed, ()) = change_visibility_decided(
        &t.db,
        &ctx("req-narrow"),
        acme.group_id,
        alice,
        Visibility::Private,
        false,
        allow,
    )
    .await
    .expect("narrow");
    assert!(narrowed.changed);

    let to_open = |segment: &str| crate::namespaces::Destination {
        parent: Some(open.namespace_id),
        segment: segment.to_owned(),
    };
    projects::move_decided(
        &t.db,
        &as_user(alice, "req-move-app"),
        app.project_id,
        alice,
        &to_open("app"),
        options(),
        |_| Ok(()),
    )
    .await
    .expect("move app");
    move_decided(
        &t.db,
        &as_user(alice, "req-move-team"),
        team.group_id,
        alice,
        &to_open("team"),
        options(),
        |_| Ok(()),
    )
    .await
    .expect("move team");

    let mut su = t.superuser().await;
    let project: String = sqlx::query_scalar("SELECT visibility FROM forge.projects WHERE id = $1")
        .bind(app.project_id.get())
        .fetch_one(&mut su)
        .await
        .expect("project");
    let group_row: String = sqlx::query_scalar("SELECT visibility FROM forge.groups WHERE id = $1")
        .bind(team.group_id.get())
        .fetch_one(&mut su)
        .await
        .expect("group");
    assert_eq!(
        (project.as_str(), group_row.as_str()),
        ("private", "private")
    );
    // Under the internal `open` they stay private in effect too.
    let bob = user(&t, "bob").await;
    let record = crate::resolve::load_project(&t.db, app.project_id, Some(bob))
        .await
        .expect("load")
        .expect("app");
    assert_eq!(record.access.visibility, Visibility::Private);
    let moved_team = load(&t.db, team.group_id, Some(bob))
        .await
        .expect("load")
        .expect("team");
    assert_eq!(moved_team.sight.visibility, Visibility::Private);
}
