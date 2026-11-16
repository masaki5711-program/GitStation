#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Group visibility (plan 15 MR 3, ADR-0010 Amendment "グループの可視性")
//! through the product router, against the development database
//! (ADR-0011) with the real loaders and forge-hosting's commands:
//!
//! - a private group hides itself, its subgroups (an internal one too:
//!   narrowest wins) and their projects from people without a role —
//!   by id, by path resolution and in the lists — with the absent 404;
//! - a member of a project below sees the groups above as a pass-through
//!   (name and path, no description, no members);
//! - `PATCH /api/v1/groups/{id}/visibility`: the Owner in a session,
//!   preview and change (`affected_projects`, one audit row), a token
//!   `403`, a stranger `404`, widening below a private group `409`
//!   `FG-REPO-0033`; new groups are private unless asked.
//!
//! Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal, Scope};
use forge_core::{CursorKeys, GroupId, ProjectId, RequestId, Role, UserId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, CreatedGroup, NewGroup};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_hosting::Hosting;
use forge_web::{AppConfig, AppState, Lists, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

struct NoTokens;

impl forge_core::Authenticator for NoTokens {
    fn authenticate<'a>(
        &'a self,
        _: &'a RequestId,
        _: &'a str,
    ) -> forge_core::authenticator::AuthFuture<'a> {
        Box::pin(async {
            Err(forge_errors::ForgeError::new(
                forge_errors::ErrorCode::Unauthenticated,
            ))
        })
    }
}

/// A synthetic key (never a real one).
fn keys() -> Arc<CursorKeys> {
    Arc::new(CursorKeys::parse(&format!("7 {}\n", "5a".repeat(32))).expect("keys"))
}

fn router(db: &Db) -> axum::Router {
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
    .with_lists(Lists::new(
        Arc::new(db.clone()) as Arc<dyn forge_core::ProjectPageLoader>,
        Arc::new(db.clone()) as Arc<dyn forge_core::GroupPageLoader>,
        keys(),
    ))
    .with_hosting(Hosting::new(db.clone()));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
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
    groups::create(
        &t.db,
        &ctx(&format!("req-group-{segment}")),
        &NewGroup {
            visibility: Visibility::Internal,
            segment: segment.to_owned(),
            parent: parent.map(|p| p.namespace_id),
            display_name: segment.to_owned(),
            description: format!("about {segment}"),
            owner,
        },
    )
    .await
    .expect("group")
}

async fn project(t: &FreshDb, in_group: &CreatedGroup, owner: UserId) -> CreatedProject {
    projects::create(
        &t.db,
        &ctx("req-project-app"),
        &NewProject {
            visibility: Visibility::Internal,
            ..NewProject::new("app", in_group.namespace_id, "app", owner)
        },
    )
    .await
    .expect("project")
}

struct Answer {
    status: StatusCode,
    json: Value,
}

async fn call(
    router: axum::Router,
    method: &str,
    uri: &str,
    who: Principal,
    body: Option<Value>,
) -> Answer {
    let builder = Request::builder().method(method).uri(uri);
    let mut req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    req.extensions_mut().insert(who);
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    Answer {
        status,
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Session,
        forge_auth::AccountKind::Human,
    )
}

fn token(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(vec![Scope::Api]),
        forge_auth::AccountKind::Human,
    )
}

fn code(a: &Answer) -> &str {
    a.json["error_code"].as_str().unwrap_or("")
}

/// The paths of a list page's items.
fn paths(page: &Answer) -> Vec<String> {
    page.json["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["path"].as_str().expect("path").to_owned())
        .collect()
}

struct World {
    alice: users::Created,
    bob: users::Created,
    carol: users::Created,
    acme: CreatedGroup,
    team: CreatedGroup,
    app: CreatedProject,
}

/// acme (alice owns) ⊃ team ⊃ app, all internal; carol is a Developer of
/// app only; bob holds no role anywhere.
async fn world(t: &FreshDb) -> World {
    let alice = user(t, "alice").await;
    let bob = user(t, "bob").await;
    let carol = user(t, "carol").await;
    let acme = group(t, "acme", None, alice.user_id).await;
    let team = group(t, "team", Some(&acme), alice.user_id).await;
    let app = project(t, &team, alice.user_id).await;
    projects::add_member(
        &t.db,
        &ctx("req-carol"),
        app.project_id,
        carol.user_id,
        Role::Developer,
    )
    .await
    .expect("carol joins app");
    World {
        alice,
        bob,
        carol,
        acme,
        team,
        app,
    }
}

fn group_uri(id: GroupId) -> String {
    format!("/api/v1/groups/{}", id.get())
}

fn project_uri(id: ProjectId) -> String {
    format!("/api/v1/projects/{}", id.get())
}

#[tokio::test]
async fn a_private_group_hides_its_subgroups_and_projects_from_people_without_a_role() {
    let t = FreshDb::migrated().await;
    let w = world(&t).await;
    let r = || router(&t.db);
    let bob = session(w.bob.user_id);
    let alice = session(w.alice.user_id);
    let visibility = format!("{}/visibility", group_uri(w.acme.group_id));

    // Internal: every signed-in person sees all three.
    let seen = call(r(), "GET", &group_uri(w.team.group_id), bob.clone(), None).await;
    assert_eq!(seen.status, StatusCode::OK, "{}", seen.json);
    assert_eq!(seen.json["visibility"], "internal");
    assert_eq!(seen.json["effective_visibility"], "internal");
    assert_eq!(seen.json["pass_through"], false);

    // The Owner previews the narrowing: one project follows, nothing written.
    let preview = call(
        r(),
        "PATCH",
        &visibility,
        alice.clone(),
        Some(json!({"visibility": "private", "preview": true})),
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.json);
    assert_eq!(preview.json["changed"], false);
    assert_eq!(preview.json["affected_projects"], 1);
    assert_eq!(preview.json["group"]["visibility"], "internal");
    let still = call(
        r(),
        "GET",
        &project_uri(w.app.project_id),
        bob.clone(),
        None,
    )
    .await;
    assert_eq!(still.status, StatusCode::OK);

    // A stranger, a token and a Maintainer may not change it.
    let stranger = call(
        r(),
        "PATCH",
        &visibility,
        bob.clone(),
        Some(json!({"visibility": "private"})),
    )
    .await;
    assert_eq!(
        (stranger.status, code(&stranger)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let by_token = call(
        r(),
        "PATCH",
        &visibility,
        token(w.alice.user_id),
        Some(json!({"visibility": "private"})),
    )
    .await;
    assert_eq!(
        (by_token.status, code(&by_token)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002")
    );

    // The narrowing: the rows below keep their values, the chain decides.
    let narrowed = call(
        r(),
        "PATCH",
        &visibility,
        alice.clone(),
        Some(json!({"visibility": "private"})),
    )
    .await;
    assert_eq!(narrowed.status, StatusCode::OK, "{}", narrowed.json);
    assert_eq!(narrowed.json["changed"], true);
    assert_eq!(narrowed.json["affected_projects"], 1);
    assert_eq!(narrowed.json["group"]["visibility"], "private");
    let mut su = t.superuser().await;
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.audit_log WHERE action = 'group.visibility_changed' \
         AND details->>'affected_projects' = '1' AND details->>'to' = 'private'",
    )
    .fetch_one(&mut su)
    .await
    .expect("audit");
    assert_eq!(audited, 1);

    // bob: the absent 404 everywhere — by id, by path, in the lists.
    for uri in [
        group_uri(w.acme.group_id),
        group_uri(w.team.group_id),
        project_uri(w.app.project_id),
        format!("{}/members", group_uri(w.acme.group_id)),
        "/api/v1/namespaces/resolve?path=acme".to_owned(),
        "/api/v1/namespaces/resolve?path=acme/team".to_owned(),
        "/api/v1/namespaces/resolve?path=acme/team/app".to_owned(),
    ] {
        let hidden = call(r(), "GET", &uri, bob.clone(), None).await;
        assert_eq!(
            (hidden.status, code(&hidden)),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
            "{uri}: {}",
            hidden.json
        );
    }
    let groups_page = call(r(), "GET", "/api/v1/groups", bob.clone(), None).await;
    assert_eq!(groups_page.status, StatusCode::OK);
    assert!(paths(&groups_page).is_empty(), "{}", groups_page.json);
    let projects_page = call(r(), "GET", "/api/v1/projects", bob.clone(), None).await;
    assert!(paths(&projects_page).is_empty(), "{}", projects_page.json);
    // Changing it is the same 404 for him now.
    let hidden_change = call(
        r(),
        "PATCH",
        &visibility,
        bob.clone(),
        Some(json!({"visibility": "internal"})),
    )
    .await;
    assert_eq!(
        (hidden_change.status, code(&hidden_change)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );

    // alice (Owner) still sees everything with its effective visibility.
    let own = call(r(), "GET", &group_uri(w.team.group_id), alice.clone(), None).await;
    assert_eq!(own.status, StatusCode::OK);
    assert_eq!(own.json["visibility"], "internal");
    assert_eq!(own.json["effective_visibility"], "private");
    let own_project = call(r(), "GET", &project_uri(w.app.project_id), alice, None).await;
    assert_eq!(own_project.json["visibility"], "private");

    // The doctor's info count: team and app are stored wider than acme.
    let wider = groups::wider_than_parent(&t.db)
        .await
        .expect("count")
        .expect("migrated");
    assert_eq!((wider.groups, wider.projects), (1, 1));
}

#[tokio::test]
async fn a_member_below_sees_the_groups_above_as_a_pass_through_only() {
    let t = FreshDb::migrated().await;
    let w = world(&t).await;
    let r = || router(&t.db);
    let carol = session(w.carol.user_id);
    let narrowed = call(
        r(),
        "PATCH",
        &format!("{}/visibility", group_uri(w.acme.group_id)),
        session(w.alice.user_id),
        Some(json!({"visibility": "private"})),
    )
    .await;
    assert_eq!(narrowed.status, StatusCode::OK, "{}", narrowed.json);

    // Her project: as a member.
    let app = call(
        r(),
        "GET",
        &project_uri(w.app.project_id),
        carol.clone(),
        None,
    )
    .await;
    assert_eq!(app.status, StatusCode::OK, "{}", app.json);
    // The groups above: name and path, nothing else.
    for id in [w.acme.group_id, w.team.group_id] {
        let above = call(r(), "GET", &group_uri(id), carol.clone(), None).await;
        assert_eq!(above.status, StatusCode::OK, "{}", above.json);
        assert_eq!(above.json["pass_through"], true);
        assert_eq!(above.json["description"], "");
        assert_eq!(above.json["role"], Value::Null);
        let members = call(
            r(),
            "GET",
            &format!("{}/members", group_uri(id)),
            carol.clone(),
            None,
        )
        .await;
        assert_eq!(
            (members.status, code(&members)),
            (StatusCode::NOT_FOUND, "FG-REPO-0005")
        );
    }
    let resolved = call(
        r(),
        "GET",
        "/api/v1/namespaces/resolve?path=acme",
        carol.clone(),
        None,
    )
    .await;
    assert_eq!(resolved.status, StatusCode::OK, "{}", resolved.json);
    // The list shows the chain to her project, no other group.
    let other = group(&t, "other", None, w.alice.user_id).await;
    let narrowed = call(
        r(),
        "PATCH",
        &format!("{}/visibility", group_uri(other.group_id)),
        session(w.alice.user_id),
        Some(json!({"visibility": "private"})),
    )
    .await;
    assert_eq!(narrowed.status, StatusCode::OK);
    let page = call(r(), "GET", "/api/v1/groups", carol.clone(), None).await;
    assert_eq!(paths(&page), ["acme", "acme/team"], "{}", page.json);
    assert!(page.json["items"]
        .as_array()
        .expect("items")
        .iter()
        .all(|g| g["pass_through"] == true && g["description"] == ""));
    // A pass-through is never a parent to create in.
    let sub = call(
        r(),
        "POST",
        &format!("{}/subgroups", group_uri(w.acme.group_id)),
        carol,
        Some(json!({"path": "mine", "display_name": "Mine"})),
    )
    .await;
    assert_eq!(
        (sub.status, code(&sub)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
}

#[tokio::test]
async fn widening_above_a_private_parent_is_refused_and_new_groups_are_private() {
    let t = FreshDb::migrated().await;
    let w = world(&t).await;
    let r = || router(&t.db);
    let alice = session(w.alice.user_id);
    let narrowed = call(
        r(),
        "PATCH",
        &format!("{}/visibility", group_uri(w.acme.group_id)),
        alice.clone(),
        Some(json!({"visibility": "private"})),
    )
    .await;
    assert_eq!(narrowed.status, StatusCode::OK);

    // Narrowing team below a private acme changes no effective visibility
    // (nothing follows it), and nothing below acme can be widened while
    // acme is private.
    let team_private = call(
        r(),
        "PATCH",
        &format!("{}/visibility", group_uri(w.team.group_id)),
        alice.clone(),
        Some(json!({"visibility": "private"})),
    )
    .await;
    assert_eq!(team_private.status, StatusCode::OK, "{}", team_private.json);
    assert_eq!(team_private.json["affected_projects"], 0);
    let widen = call(
        r(),
        "PATCH",
        &format!("{}/visibility", group_uri(w.team.group_id)),
        alice.clone(),
        Some(json!({"visibility": "internal"})),
    )
    .await;
    assert_eq!(
        (widen.status, code(&widen)),
        (StatusCode::CONFLICT, "FG-REPO-0033"),
        "{}",
        widen.json
    );
    let project_widen = call(
        r(),
        "PUT",
        &format!("{}/visibility", project_uri(w.app.project_id)),
        alice.clone(),
        Some(json!({"visibility": "internal"})),
    )
    .await;
    assert_eq!(
        (project_widen.status, code(&project_widen)),
        (StatusCode::CONFLICT, "FG-REPO-0033"),
        "{}",
        project_widen.json
    );
    let internal_sub = call(
        r(),
        "POST",
        &format!("{}/subgroups", group_uri(w.acme.group_id)),
        alice.clone(),
        Some(json!({"path": "open", "display_name": "Open", "visibility": "internal"})),
    )
    .await;
    assert_eq!(
        (internal_sub.status, code(&internal_sub)),
        (StatusCode::CONFLICT, "FG-REPO-0033")
    );
    // Without a visibility a new subgroup is private (plan 15 §6).
    let sub = call(
        r(),
        "POST",
        &format!("{}/subgroups", group_uri(w.acme.group_id)),
        alice,
        Some(json!({"path": "closed", "display_name": "Closed"})),
    )
    .await;
    assert_eq!(sub.status, StatusCode::CREATED, "{}", sub.json);
    assert_eq!(sub.json["visibility"], "private");
    assert_eq!(sub.json["effective_visibility"], "private");
}
