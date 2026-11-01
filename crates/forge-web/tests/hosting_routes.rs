#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The group and project write routes of M1 phase 8b MR7a through the
//! product router, against the development database (ADR-0011) with the
//! real loaders and forge-hosting's commands: the registry's proof, then
//! the command's own decision inside its transaction.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::{RequestId, Role, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_hosting::Hosting;
use forge_web::{AppConfig, AppState, Lookups};
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

fn state(db: &Db) -> AppState {
    AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
}

fn router(db: &Db) -> axum::Router {
    let state = state(db).with_hosting(Hosting::new(db.clone()));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn user(t: &FreshDb, name: &str, admin: bool) -> users::Created {
    let mut new = NewUser::human(name, name, &format!("{name}@example.test"));
    new.is_admin = admin;
    users::create(&t.db, &ctx(&format!("req-user-{name}")), &new)
        .await
        .expect("user")
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

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Session)
}

fn code(a: &Answer) -> &str {
    a.json["error_code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn groups_are_created_read_and_updated_through_the_routes() {
    let t = FreshDb::migrated().await;
    let root = user(&t, "root", true).await;
    let bob = user(&t, "bob", false).await;
    let admin = session(root.user_id, true);
    let as_bob = session(bob.user_id, false);
    let r = || router(&t.db);

    let body = json!({"path": "acme", "display_name": "Acme"});
    let denied = call(
        r(),
        "POST",
        "/api/v1/groups",
        as_bob.clone(),
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        (denied.status, code(&denied)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let created = call(r(), "POST", "/api/v1/groups", admin.clone(), Some(body)).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    assert_eq!(created.json["path"], "acme");
    assert_eq!(created.json["role"], "owner");
    assert_eq!(created.json["parent_id"], Value::Null);
    assert_eq!(created.json["description"], "");
    let id = created.json["id"].as_i64().expect("id");
    let one = format!("/api/v1/groups/{id}");

    // Every signed-in user reads a group; bob holds no role.
    let read = call(r(), "GET", &one, as_bob.clone(), None).await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.json["role"], Value::Null);
    assert_eq!(read.json["display_name"], "Acme");

    let patch = json!({"description": "synthetic"});
    let denied = call(r(), "PATCH", &one, as_bob.clone(), Some(patch.clone())).await;
    assert_eq!(
        (denied.status, code(&denied)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let updated = call(r(), "PATCH", &one, admin.clone(), Some(patch)).await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.json);
    assert_eq!(updated.json["description"], "synthetic");
    let empty = call(r(), "PATCH", &one, admin.clone(), Some(json!({}))).await;
    assert_eq!(
        (empty.status, code(&empty)),
        (StatusCode::BAD_REQUEST, "FG-REPO-0011")
    );

    // bob becomes Owner of acme and creates a subgroup.
    groups::add_member(
        &t.db,
        &ctx("req-bob"),
        forge_core::GroupId::new(id),
        bob.user_id,
        Role::Owner,
    )
    .await
    .expect("bob owns acme");
    let sub = call(
        r(),
        "POST",
        &format!("{one}/subgroups"),
        as_bob.clone(),
        Some(json!({"path": "web", "display_name": "Web", "description": "synthetic"})),
    )
    .await;
    assert_eq!(sub.status, StatusCode::CREATED, "{}", sub.json);
    assert_eq!(sub.json["path"], "acme/web");
    assert_eq!(sub.json["parent_id"], json!(id));
    let taken = call(
        r(),
        "POST",
        &format!("{one}/subgroups"),
        as_bob.clone(),
        Some(json!({"path": "web", "display_name": "Web"})),
    )
    .await;
    assert_eq!(
        (taken.status, code(&taken)),
        (StatusCode::CONFLICT, "FG-REPO-0009")
    );

    // An absent id is the 404 every caller gets; a malformed one a 400.
    let absent = call(r(), "GET", "/api/v1/groups/9999", as_bob.clone(), None).await;
    assert_eq!(
        (absent.status, code(&absent)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    let bad = call(r(), "GET", "/api/v1/groups/01", as_bob.clone(), None).await;
    assert_eq!(
        (bad.status, code(&bad)),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    let unknown = call(
        r(),
        "PATCH",
        &one,
        admin.clone(),
        Some(json!({"display_name": "x", "owner": 1})),
    )
    .await;
    assert_eq!(
        (unknown.status, code(&unknown)),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    let anonymous = call(r(), "GET", &one, Principal::anonymous(), None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    t.drop_now().await;
}

#[tokio::test]
async fn projects_are_created_updated_and_made_internal_through_the_routes() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let as_alice = session(alice.user_id, false);
    let as_bob = session(bob.user_id, false);
    let r = || router(&t.db);

    let new = json!({
        "namespace_id": alice.namespace_id.get(),
        "path": "web",
        "display_name": "Web",
        "kind": "spec",
    });
    // bob may not create in alice's personal namespace.
    let foreign = call(
        r(),
        "POST",
        "/api/v1/projects",
        as_bob.clone(),
        Some(new.clone()),
    )
    .await;
    assert_eq!(
        (foreign.status, code(&foreign)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let created = call(r(), "POST", "/api/v1/projects", as_alice.clone(), Some(new)).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    assert_eq!(created.json["path"], "alice/web");
    assert_eq!(created.json["kind"], "spec");
    assert_eq!(created.json["visibility"], "private");
    assert_eq!(created.json["role"], "owner");
    let id = created.json["id"].as_i64().expect("id");
    let project_ns = created.json["namespace_id"].as_i64().expect("namespace");

    // A project namespace as parent is the same 404 as an absent id.
    let under_project = call(
        r(),
        "POST",
        "/api/v1/projects",
        as_alice.clone(),
        Some(json!({"namespace_id": project_ns, "path": "x", "display_name": "X"})),
    )
    .await;
    let absent = call(
        r(),
        "POST",
        "/api/v1/projects",
        as_alice.clone(),
        Some(json!({"namespace_id": 99999, "path": "x", "display_name": "X"})),
    )
    .await;
    assert_eq!(under_project.status, StatusCode::NOT_FOUND);
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
    assert_eq!(code(&under_project), code(&absent));

    let one = format!("/api/v1/projects/{id}");
    let outsider = call(
        r(),
        "PATCH",
        &one,
        as_bob.clone(),
        Some(json!({"display_name": "Mine"})),
    )
    .await;
    assert_eq!(
        (outsider.status, code(&outsider)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    let updated = call(
        r(),
        "PATCH",
        &one,
        as_alice.clone(),
        Some(json!({"display_name": "Web site", "description": "synthetic"})),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.json);
    assert_eq!(updated.json["display_name"], "Web site");
    assert_eq!(updated.json["description"], "synthetic");

    let visibility = format!("{one}/visibility");
    let bad = call(
        r(),
        "PUT",
        &visibility,
        as_alice.clone(),
        Some(json!({"visibility": "public"})),
    )
    .await;
    assert_eq!(
        (bad.status, code(&bad)),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    forge_db::projects::add_member(
        &t.db,
        &ctx("req-bob"),
        forge_core::ProjectId::new(id),
        bob.user_id,
        Role::Maintainer,
    )
    .await
    .expect("bob maintains web");
    let maintainer = call(
        r(),
        "PUT",
        &visibility,
        as_bob.clone(),
        Some(json!({"visibility": "internal"})),
    )
    .await;
    assert_eq!(
        (maintainer.status, code(&maintainer)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    // An agent's token never changes visibility, even the Owner's.
    let agent = Principal::authenticated(
        alice.user_id,
        false,
        true,
        Credential::Token(vec![forge_core::Scope::Api]),
    )
    .as_agent(true);
    let by_agent = call(
        r(),
        "PUT",
        &visibility,
        agent.clone(),
        Some(json!({"visibility": "internal"})),
    )
    .await;
    assert_eq!(
        (by_agent.status, code(&by_agent)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let agent_update = call(
        r(),
        "PATCH",
        &one,
        agent,
        Some(json!({"description": "by agent"})),
    )
    .await;
    assert_eq!(agent_update.status, StatusCode::OK, "{}", agent_update.json);
    let internal = call(
        r(),
        "PUT",
        &visibility,
        as_alice.clone(),
        Some(json!({"visibility": "internal"})),
    )
    .await;
    assert_eq!(internal.status, StatusCode::OK, "{}", internal.json);
    assert_eq!(internal.json["visibility"], "internal");
    let events = t
        .json_rows(
            "SELECT event_type, project_id FROM forge.events \
             WHERE event_type = 'project.visibility_changed'",
        )
        .await;
    assert_eq!(
        events,
        [json!({"event_type": "project.visibility_changed", "project_id": id})]
    );
    t.drop_now().await;
}

#[tokio::test]
async fn without_hosting_the_routes_fail_loudly() {
    let t = FreshDb::migrated().await;
    let root = user(&t, "root", true).await;
    let app = forge_web::app(state(&t.db), AppConfig::default()).expect("router");
    let answer = call(
        app,
        "GET",
        "/api/v1/groups/1",
        session(root.user_id, true),
        None,
    )
    .await;
    assert_eq!(
        (answer.status, code(&answer)),
        (StatusCode::INTERNAL_SERVER_ERROR, "FG-SYS-0001")
    );
    t.drop_now().await;
}
