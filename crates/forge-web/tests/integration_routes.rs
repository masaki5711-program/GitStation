#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The integration routes of M3 I1a through the product router against the
//! development database (ADR-0011), with the real loaders and commands:
//! managing an integration in an administrator's web session only (a token
//! never), reading them with an `admin`-scoped token of a person, binding a
//! project as its Maintainer (a Developer, an agent and a service account
//! refused; a project pending deletion refused), the service account made
//! a Reporter by the binding and removed by the unbinding. Principals are
//! inserted as request extensions (the authentication layer is tested
//! elsewhere). Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::{CursorKeys, ProjectId, RequestId, Role, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
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

fn router(db: &Db) -> axum::Router {
    let keys = Arc::new(CursorKeys::parse(&format!("3 {}\n", "6b".repeat(32))).expect("keys"));
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
    .with_hosting(Hosting::new(db.clone()))
    .with_lists(Lists::new(Arc::new(db.clone()), Arc::new(db.clone()), keys));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn person(t: &FreshDb, name: &str, admin: bool) -> users::Created {
    let mut new = NewUser::human(
        name,
        &format!("Synthetic {name}"),
        &format!("{name}@example.test"),
    );
    new.is_admin = admin;
    users::create(&t.db, &ctx(&format!("req-user-{name}")), &new)
        .await
        .expect("user")
}

struct Answer {
    status: StatusCode,
    json: Value,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }
}

async fn call(db: &Db, method: &str, uri: &str, who: Principal, body: Option<Value>) -> Answer {
    let builder = Request::builder().method(method).uri(uri);
    let mut req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    req.extensions_mut().insert(who);
    let res = router(db).oneshot(req).await.expect("infallible");
    let status = res.status();
    let raw = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    Answer {
        status,
        json: serde_json::from_slice(&raw).unwrap_or(Value::Null),
    }
}

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Session, AccountKind::Human)
}

fn token(user: UserId, admin: bool, scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        user,
        admin,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::Human,
    )
}

fn sa_token(user: UserId, scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::ServiceAccount,
    )
}

const ADMIN_TOKEN: &[Scope] = &[Scope::Api, Scope::Admin];

struct World {
    t: FreshDb,
    root: UserId,
    alice: users::Created,
    sa: UserId,
    integration: i64,
    web: ProjectId,
}

/// An administrator, a person, a service account owned by her and its
/// kanflow integration (both created over HTTP in the administrator's
/// session), and her project.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let root = person(&t, "root", true).await.user_id;
    let alice = person(&t, "alice", false).await;
    let created = call(
        &t.db,
        "POST",
        "/api/v1/admin/service-accounts",
        session(root, true),
        Some(json!({
            "username": "kanflow-sync",
            "owner_id": alice.user_id.get(),
            "purpose": "Kanflow reads the event feed of bound projects",
        })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    let sa = UserId::new(created.json["id"].as_i64().expect("id"));
    let integration = call(
        &t.db,
        "POST",
        "/api/v1/admin/integrations",
        session(root, true),
        Some(json!({"kind": "kanflow", "service_account_id": sa.get()})),
    )
    .await;
    assert_eq!(
        integration.status,
        StatusCode::CREATED,
        "{}",
        integration.json
    );
    assert_eq!(integration.json["kind"], "kanflow");
    assert_eq!(integration.json["service_account"]["id"], sa.get());
    assert_eq!(
        integration.json["service_account"]["username"],
        "kanflow-sync"
    );
    assert_eq!(integration.json["enabled"], true);
    assert_eq!(integration.json["event_types"], json!(["*"]));
    assert_eq!(integration.json["bindings"], 0);
    assert_eq!(integration.json["created_by"], root.get());
    let integration = integration.json["id"].as_i64().expect("integration id");
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("project")
    .project_id;
    World {
        t,
        root,
        alice,
        sa,
        integration,
        web,
    }
}

fn binding_uri(project: ProjectId, integration: i64) -> String {
    format!(
        "/api/v1/projects/{}/integration-bindings/{integration}",
        project.get()
    )
}

async fn member_role(t: &FreshDb, project: ProjectId, user: UserId) -> Option<String> {
    forge_db::members::direct_role(
        &t.db,
        forge_db::members::MemberScope::Project(project),
        user,
    )
    .await
    .expect("role")
    .map(|r| r.as_str().to_owned())
}

#[tokio::test]
async fn integration_manage_is_admin_session_only() {
    let w = world().await;
    let db = &w.t.db;
    let one = format!("/api/v1/admin/integrations/{}", w.integration);
    let second = person(&w.t, "ci-owner", false).await;
    let body = json!({"kind": "kanflow", "service_account_id": second.user_id.get()});
    // Create and change: never with a token (an administrator's `admin`
    // token included), never by a person without administrator rights.
    for (method, uri, body) in [
        ("POST", "/api/v1/admin/integrations", body.clone()),
        ("PATCH", one.as_str(), json!({"enabled": false})),
    ] {
        for (who, status, code) in [
            (
                token(w.root, true, ADMIN_TOKEN),
                StatusCode::FORBIDDEN,
                "FG-AUTH-0002",
            ),
            (
                session(w.alice.user_id, false),
                StatusCode::FORBIDDEN,
                "FG-AUTH-0003",
            ),
            (
                sa_token(w.sa, &[Scope::Api]),
                StatusCode::FORBIDDEN,
                "FG-AUTH-0002",
            ),
        ] {
            let refused = call(db, method, uri, who, Some(body.clone())).await;
            assert_eq!(refused.status, status, "{method} {uri}: {}", refused.json);
            assert_eq!(refused.code(), code, "{method} {uri}");
        }
    }
    // The session changes it: disabled, a narrower allowlist.
    let changed = call(
        db,
        "PATCH",
        &one,
        session(w.root, true),
        Some(json!({"enabled": false, "event_types": ["merge_request.*", "repository.pushed"]})),
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.json);
    assert_eq!(changed.json["outcome"], "updated");
    assert_eq!(changed.json["fields"], json!(["enabled", "event_types"]));
    assert_eq!(changed.json["integration"]["enabled"], false);
    let same = call(
        db,
        "PATCH",
        &one,
        session(w.root, true),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(same.json["outcome"], "unchanged", "{}", same.json);
    // A type of another class, or an unknown kind, is a bad request.
    let bad = call(
        db,
        "PATCH",
        &one,
        session(w.root, true),
        Some(json!({"event_types": ["member.added"]})),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.json);
    assert_eq!(bad.code(), "FG-WEB-0006");
    let unknown_kind = call(
        db,
        "POST",
        "/api/v1/admin/integrations",
        session(w.root, true),
        Some(json!({"kind": "gitlab", "service_account_id": w.sa.get()})),
    )
    .await;
    assert_eq!(unknown_kind.code(), "FG-WEB-0006", "{}", unknown_kind.json);
    // A person is no service account; one integration per account.
    for id in [second.user_id.get(), w.sa.get()] {
        let refused = call(
            db,
            "POST",
            "/api/v1/admin/integrations",
            session(w.root, true),
            Some(json!({"kind": "kanflow", "service_account_id": id})),
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.json);
        assert_eq!(refused.code(), "FG-WEB-0006");
    }

    // Read: an administrator's session or `admin` token; never a person
    // without rights, an agent or a service account.
    for uri in [
        "/api/v1/admin/integrations".to_owned(),
        one.clone(),
        format!("{one}/bindings"),
    ] {
        for who in [session(w.root, true), token(w.root, true, ADMIN_TOKEN)] {
            let read = call(db, "GET", &uri, who, None).await;
            assert_eq!(read.status, StatusCode::OK, "{uri}: {}", read.json);
        }
        for (who, code) in [
            (session(w.alice.user_id, false), "FG-AUTH-0003"),
            (
                token(w.root, true, ADMIN_TOKEN).as_agent(true),
                "FG-AUTH-0003",
            ),
            (token(w.root, false, &[Scope::Api]), "FG-AUTH-0003"),
            (sa_token(w.sa, &[Scope::ReadApi]), "FG-AUTH-0003"),
        ] {
            let refused = call(db, "GET", &uri, who, None).await;
            assert_eq!(
                refused.status,
                StatusCode::FORBIDDEN,
                "{uri}: {}",
                refused.json
            );
            assert_eq!(refused.code(), code, "{uri}");
        }
    }
    let listed = call(
        db,
        "GET",
        "/api/v1/admin/integrations",
        session(w.root, true),
        None,
    )
    .await;
    assert_eq!(listed.json["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed.json["truncated"], false);
    // An absent integration is not found.
    let absent = call(
        db,
        "GET",
        "/api/v1/admin/integrations/9999",
        session(w.root, true),
        None,
    )
    .await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND, "{}", absent.json);
    assert_eq!(absent.code(), "FG-REPO-0005");
    w.t.drop_now().await;
}

#[tokio::test]
async fn binding_requires_maintainer() {
    let w = world().await;
    let db = &w.t.db;
    let bob = person(&w.t, "bob", false).await.user_id;
    let carol = person(&w.t, "carol", false).await.user_id;
    projects::add_member(db, &ctx("req-bob"), w.web, bob, Role::Developer)
        .await
        .expect("developer");
    projects::add_member(db, &ctx("req-carol"), w.web, carol, Role::Maintainer)
        .await
        .expect("maintainer");
    let uri = binding_uri(w.web, w.integration);
    let body = json!({"external_project_id": "P1"});
    // A Developer, an agent of the Maintainer, a token without `api`, a
    // service account, a non-member (the project is private: not found).
    let dave = person(&w.t, "dave", false).await.user_id;
    for (who, status, code) in [
        (session(bob, false), StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        (
            token(carol, false, &[Scope::Api]).as_agent(true),
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
        (
            token(carol, false, &[Scope::ReadApi]),
            StatusCode::FORBIDDEN,
            "FG-AUTH-0002",
        ),
        (session(dave, false), StatusCode::NOT_FOUND, "FG-REPO-0005"),
    ] {
        let refused = call(db, "PUT", &uri, who, Some(body.clone())).await;
        assert_eq!(refused.status, status, "{}", refused.json);
        assert_eq!(refused.code(), code);
        let refused = call(db, "DELETE", &uri, session(bob, false), None).await;
        assert_eq!(refused.code(), "FG-AUTH-0003");
    }
    assert_eq!(member_role(&w.t, w.web, w.sa).await, None);
    // The Maintainer binds (her `api` token too, for the second project).
    let bound = call(db, "PUT", &uri, session(carol, false), Some(body.clone())).await;
    assert_eq!(bound.status, StatusCode::OK, "{}", bound.json);
    assert_eq!(bound.json["outcome"], "bound");
    assert_eq!(bound.json["binding"]["external_project_id"], "P1");
    assert_eq!(bound.json["binding"]["granted_membership"], true);
    assert_eq!(bound.json["binding"]["created_by"], carol.get());
    let again = call(
        db,
        "PUT",
        &uri,
        token(carol, false, &[Scope::Api]),
        Some(body.clone()),
    )
    .await;
    assert_eq!(again.json["outcome"], "unchanged", "{}", again.json);
    // A service account with a Maintainer membership is still refused.
    let api = projects::create(
        db,
        &ctx("req-api"),
        &NewProject::new("api", w.alice.namespace_id, "Api", w.alice.user_id),
    )
    .await
    .expect("api")
    .project_id;
    projects::add_member(db, &ctx("req-sa-m"), api, w.sa, Role::Maintainer)
        .await
        .expect("sa maintainer");
    let by_sa = call(
        db,
        "PUT",
        &binding_uri(api, w.integration),
        sa_token(w.sa, &[Scope::Api]),
        Some(body.clone()),
    )
    .await;
    assert_eq!(by_sa.status, StatusCode::FORBIDDEN, "{}", by_sa.json);
    assert_eq!(by_sa.code(), "FG-AUTH-0003");
    // Reporters read the project's binding; the service account (a
    // Reporter now) too.
    let read = call(
        db,
        "GET",
        &format!("/api/v1/projects/{}/integration-bindings", w.web.get()),
        sa_token(w.sa, &[Scope::ReadApi]),
        None,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.json);
    assert_eq!(read.json["items"][0]["integration_id"], w.integration);
    assert_eq!(read.json["items"][0]["kind"], "kanflow");
    // The administration sees the binding.
    let listed = call(
        db,
        "GET",
        &format!("/api/v1/admin/integrations/{}/bindings", w.integration),
        session(w.root, true),
        None,
    )
    .await;
    assert_eq!(
        listed.json["items"][0]["project_id"],
        w.web.get(),
        "{}",
        listed.json
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn binding_grants_and_unbinding_removes_reporter_membership_over_http() {
    let w = world().await;
    let db = &w.t.db;
    let uri = binding_uri(w.web, w.integration);
    let owner = session(w.alice.user_id, false);
    let bound = call(
        db,
        "PUT",
        &uri,
        owner.clone(),
        Some(json!({"external_project_id": "kanflow:42"})),
    )
    .await;
    assert_eq!(bound.status, StatusCode::OK, "{}", bound.json);
    assert_eq!(
        member_role(&w.t, w.web, w.sa).await.as_deref(),
        Some("reporter")
    );
    // A second kanflow project for the same forge project: 409.
    let moved = call(
        db,
        "PUT",
        &uri,
        owner.clone(),
        Some(json!({"external_project_id": "kanflow:43"})),
    )
    .await;
    assert_eq!(moved.status, StatusCode::CONFLICT, "{}", moved.json);
    assert_eq!(moved.code(), "FG-INT-0016");
    // An absent integration is not found; a bad external id a bad request.
    let absent = call(
        db,
        "PUT",
        &binding_uri(w.web, 9_999),
        owner.clone(),
        Some(json!({"external_project_id": "P1"})),
    )
    .await;
    assert_eq!(absent.code(), "FG-REPO-0005", "{}", absent.json);
    let bad = call(
        db,
        "PUT",
        &uri,
        owner.clone(),
        Some(json!({"external_project_id": "a/b"})),
    )
    .await;
    assert_eq!(bad.code(), "FG-WEB-0006", "{}", bad.json);
    let unbound = call(db, "DELETE", &uri, owner.clone(), None).await;
    assert_eq!(unbound.status, StatusCode::NO_CONTENT, "{}", unbound.json);
    assert_eq!(member_role(&w.t, w.web, w.sa).await, None);
    let again = call(db, "DELETE", &uri, owner, None).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "{}", again.json);
    assert_eq!(again.code(), "FG-REPO-0005");
    w.t.drop_now().await;
}

#[tokio::test]
async fn binding_refused_for_pending_deletion_project_over_http() {
    let w = world().await;
    let db = &w.t.db;
    let owner = session(w.alice.user_id, false);
    let deleted = call(
        db,
        "DELETE",
        &format!("/api/v1/projects/{}", w.web.get()),
        owner.clone(),
        None,
    )
    .await;
    assert!(deleted.status.is_success(), "{}", deleted.json);
    for who in [owner, session(w.root, true)] {
        let refused = call(
            db,
            "PUT",
            &binding_uri(w.web, w.integration),
            who,
            Some(json!({"external_project_id": "P1"})),
        )
        .await;
        assert_eq!(refused.status, StatusCode::GONE, "{}", refused.json);
        assert_eq!(refused.code(), "FG-REPO-0006");
    }
    assert_eq!(member_role(&w.t, w.web, w.sa).await, None);
    w.t.drop_now().await;
}
