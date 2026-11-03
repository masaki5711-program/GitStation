#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The service account routes of M3 S0 through the product router against
//! the development database (ADR-0011), with the real loaders and
//! commands: create and issue in an administrator's session only (a token
//! never issues one), list / read / tokens / revoke with an `admin`-scoped
//! token, a person's id under the routes answered like an absent one, a
//! disabled account's tokens revoked, and a service account that sees only
//! its member projects. Principals are inserted as request extensions (the
//! authentication layer is tested elsewhere). Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::{CursorKeys, RequestId, Role, UserId, Visibility};
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
}

/// An administrator, a person and one service account owned by her
/// (created over HTTP in the administrator's session).
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
    assert_eq!(created.json["username"], "kanflow-sync");
    assert_eq!(created.json["display_name"], "kanflow-sync");
    assert_eq!(created.json["owner"]["id"], alice.user_id.get());
    assert_eq!(created.json["created_by"], root.get());
    assert_eq!(created.json["state"], "active");
    let sa = UserId::new(created.json["id"].as_i64().expect("id"));
    World { t, root, alice, sa }
}

fn issue_body() -> Value {
    json!({"name": "feed", "scopes": ["read_api"], "expires_in_days": 90})
}

#[tokio::test]
async fn service_account_routes_create_list_read_and_update_for_administrators() {
    let w = world().await;
    let db = &w.t.db;
    let body = json!({"username": "ci-two", "owner_id": w.alice.user_id.get(), "purpose": "CI"});
    // Create: a session's only; a person without rights is refused.
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
    ] {
        let refused = call(
            db,
            "POST",
            "/api/v1/admin/service-accounts",
            who,
            Some(body.clone()),
        )
        .await;
        assert_eq!(
            (refused.status, refused.code()),
            (status, code),
            "{}",
            refused.json
        );
    }
    // The owner must be a person: the service account itself is refused.
    let refused = call(
        db,
        "POST",
        "/api/v1/admin/service-accounts",
        session(w.root, true),
        Some(json!({"username": "ci-two", "owner_id": w.sa.get(), "purpose": "CI"})),
    )
    .await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::BAD_REQUEST, "FG-AUTH-0006")
    );

    // List and read with an admin-scoped token.
    let listed = call(
        db,
        "GET",
        "/api/v1/admin/service-accounts",
        token(w.root, true, ADMIN_TOKEN),
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    assert_eq!(listed.json["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed.json["truncated"], false);
    let one = format!("/api/v1/admin/service-accounts/{}", w.sa.get());
    let read = call(db, "GET", &one, token(w.root, true, ADMIN_TOKEN), None).await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(
        read.json["purpose"],
        "Kanflow reads the event feed of bound projects"
    );
    // Without administrator rights: refused; a person's id: not found.
    let refused = call(db, "GET", &one, token(w.root, true, &[Scope::Api]), None).await;
    assert_eq!(refused.code(), "FG-AUTH-0003");
    let person_one = format!("/api/v1/admin/service-accounts/{}", w.alice.user_id.get());
    let absent = call(db, "GET", &person_one, session(w.root, true), None).await;
    assert_eq!(
        (absent.status, absent.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    let missing = call(
        db,
        "GET",
        "/api/v1/admin/service-accounts/99999",
        session(w.root, true),
        None,
    )
    .await;
    assert_eq!(missing.json["user_message"], absent.json["user_message"]);

    // Update: hand it on to bob.
    let bob = person(&w.t, "bob", false).await.user_id;
    let updated = call(
        db,
        "PATCH",
        &one,
        token(w.root, true, ADMIN_TOKEN),
        Some(json!({"owner_id": bob.get(), "purpose": "Kanflow sync"})),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.json);
    assert_eq!(updated.json["outcome"], "updated");
    assert_eq!(updated.json["fields"], json!(["purpose", "owner"]));
    assert_eq!(updated.json["service_account"]["owner"]["id"], bob.get());
    let same = call(
        db,
        "PATCH",
        &one,
        session(w.root, true),
        Some(json!({"owner_id": bob.get()})),
    )
    .await;
    assert_eq!(same.json["outcome"], "unchanged");
    // An agent's admin token never changes who answers for it.
    let agent = token(w.root, true, ADMIN_TOKEN).as_agent(true);
    let refused = call(db, "PATCH", &one, agent, Some(json!({"purpose": "x"}))).await;
    assert_eq!(refused.code(), "FG-AUTH-0003");
    w.t.drop_now().await;
}

#[tokio::test]
async fn service_account_token_issue_is_admin_session_only() {
    let w = world().await;
    let db = &w.t.db;
    let tokens = format!("/api/v1/admin/service-accounts/{}/tokens", w.sa.get());
    let issued = call(
        db,
        "POST",
        &tokens,
        session(w.root, true),
        Some(issue_body()),
    )
    .await;
    assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.json);
    assert!(issued.json["token"]
        .as_str()
        .is_some_and(|s| s.starts_with("fgp_")));
    assert_eq!(issued.json["issued_by_user_id"], w.root.get());
    assert_eq!(issued.json["service_account_id"], w.sa.get());
    // Listed with its issuer, never the secret.
    let listed = call(db, "GET", &tokens, token(w.root, true, ADMIN_TOKEN), None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    let items = listed.json["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["issued_by_user_id"], w.root.get());
    assert!(items[0].get("token").is_none(), "never the secret");
    // A person without administrator rights, in a session.
    let refused = call(
        db,
        "POST",
        &tokens,
        session(w.alice.user_id, false),
        Some(issue_body()),
    )
    .await;
    assert_eq!(refused.code(), "FG-AUTH-0003");
    w.t.drop_now().await;
}

#[tokio::test]
async fn token_cannot_issue_service_account_token() {
    let w = world().await;
    let db = &w.t.db;
    let tokens = format!("/api/v1/admin/service-accounts/{}/tokens", w.sa.get());
    for who in [
        token(w.root, true, ADMIN_TOKEN),
        token(w.root, true, ADMIN_TOKEN).as_agent(true),
        sa_token(w.sa, &[Scope::Api]),
    ] {
        let refused = call(db, "POST", &tokens, who, Some(issue_body())).await;
        assert_eq!(
            (refused.status, refused.code()),
            (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
            "{}",
            refused.json
        );
    }
    let listed = call(db, "GET", &tokens, session(w.root, true), None).await;
    assert_eq!(listed.json["items"], json!([]), "nothing was issued");
    w.t.drop_now().await;
}

#[tokio::test]
async fn issue_refused_for_human_or_disabled_target() {
    let w = world().await;
    let db = &w.t.db;
    let person = format!(
        "/api/v1/admin/service-accounts/{}/tokens",
        w.alice.user_id.get()
    );
    let refused = call(
        db,
        "POST",
        &person,
        session(w.root, true),
        Some(issue_body()),
    )
    .await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    let tokens = format!("/api/v1/admin/service-accounts/{}/tokens", w.sa.get());
    let admin_scope = json!({"name": "x", "scopes": ["read_api", "admin"]});
    let refused = call(
        db,
        "POST",
        &tokens,
        session(w.root, true),
        Some(admin_scope),
    )
    .await;
    assert_eq!(refused.code(), "FG-AUTH-0003");
    let disabled = call(
        db,
        "POST",
        &format!("/api/v1/admin/users/{}/disable", w.sa.get()),
        token(w.root, true, ADMIN_TOKEN),
        None,
    )
    .await;
    assert_eq!(disabled.status, StatusCode::OK, "{}", disabled.json);
    let refused = call(
        db,
        "POST",
        &tokens,
        session(w.root, true),
        Some(issue_body()),
    )
    .await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn disabling_service_account_revokes_tokens_and_revoke_answers_a_leak() {
    let w = world().await;
    let db = &w.t.db;
    let tokens = format!("/api/v1/admin/service-accounts/{}/tokens", w.sa.get());
    let mut ids = Vec::new();
    for _ in 0..2 {
        let issued = call(
            db,
            "POST",
            &tokens,
            session(w.root, true),
            Some(issue_body()),
        )
        .await;
        ids.push(issued.json["id"].as_i64().expect("id"));
    }
    // A leak: the admin-scoped token revokes one at once (204, twice).
    let one = format!("{tokens}/{}", ids[0]);
    for _ in 0..2 {
        let revoked = call(db, "DELETE", &one, token(w.root, true, ADMIN_TOKEN), None).await;
        assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    }
    // Another user's token under this account is not found.
    let own = call(
        db,
        "POST",
        "/api/v1/user/tokens",
        session(w.root, true),
        Some(issue_body()),
    )
    .await;
    let foreign = format!("{tokens}/{}", own.json["id"].as_i64().expect("id"));
    let absent = call(db, "DELETE", &foreign, session(w.root, true), None).await;
    assert_eq!(
        (absent.status, absent.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    // Disabling the account revokes the rest.
    let disabled = call(
        db,
        "POST",
        &format!("/api/v1/admin/users/{}/disable", w.sa.get()),
        session(w.root, true),
        None,
    )
    .await;
    assert_eq!(disabled.json["outcome"], "disabled");
    let listed = call(db, "GET", &tokens, session(w.root, true), None).await;
    let items = listed.json["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert!(
        items.iter().all(|i| i["revoked_at"].is_string()),
        "{items:?}"
    );
    let read = call(
        db,
        "GET",
        &format!("/api/v1/admin/service-accounts/{}", w.sa.get()),
        session(w.root, true),
        None,
    )
    .await;
    assert_eq!(read.json["state"], "disabled");
    assert_eq!(read.json["active_tokens"], 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn service_account_sees_only_member_projects() {
    let w = world().await;
    let db = &w.t.db;
    let make = |segment: &'static str| {
        let alice = w.alice;
        async move {
            projects::create(
                db,
                &ctx(&format!("req-{segment}")),
                &NewProject {
                    visibility: Visibility::Internal,
                    ..NewProject::new(segment, alice.namespace_id, segment, alice.user_id)
                },
            )
            .await
            .expect("project")
        }
    };
    let open = make("open").await;
    let member = make("member").await;
    projects::add_member(db, &ctx("req-m"), member.project_id, w.sa, Role::Reporter)
        .await
        .expect("member");
    let reader = sa_token(w.sa, &[Scope::ReadApi]);
    let hidden = call(
        db,
        "GET",
        &format!("/api/v1/projects/{}", open.project_id.get()),
        reader.clone(),
        None,
    )
    .await;
    assert_eq!(
        (hidden.status, hidden.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    let seen = call(
        db,
        "GET",
        &format!("/api/v1/projects/{}", member.project_id.get()),
        reader.clone(),
        None,
    )
    .await;
    assert_eq!(seen.status, StatusCode::OK, "{}", seen.json);
    let listed = call(db, "GET", "/api/v1/projects", reader, None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    let ids: Vec<i64> = listed.json["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|p| p["id"].as_i64())
        .collect();
    assert_eq!(ids, vec![member.project_id.get()]);
    // A person sees both internal projects.
    let bob = person(&w.t, "bob", false).await.user_id;
    let listed = call(
        db,
        "GET",
        "/api/v1/projects",
        token(bob, false, &[Scope::ReadApi]),
        None,
    )
    .await;
    assert_eq!(listed.json["items"].as_array().map(Vec::len), Some(2));
    w.t.drop_now().await;
}

#[tokio::test]
async fn service_account_cannot_create_project_in_own_namespace() {
    let w = world().await;
    let db = &w.t.db;
    let sa_row = call(
        db,
        "GET",
        &format!("/api/v1/admin/service-accounts/{}", w.sa.get()),
        session(w.root, true),
        None,
    )
    .await;
    assert_eq!(sa_row.status, StatusCode::OK);
    let resolved = call(
        db,
        "GET",
        "/api/v1/namespaces/resolve?path=kanflow-sync",
        session(w.root, true),
        None,
    )
    .await;
    assert_eq!(resolved.json["kind"], "user", "{}", resolved.json);
    let namespace = resolved.json["namespace_id"]
        .as_i64()
        .expect("its personal namespace");
    let creator = sa_token(w.sa, &[Scope::Api]);
    let refused = call(
        db,
        "POST",
        "/api/v1/projects",
        creator,
        Some(json!({"namespace_id": namespace, "path": "mine", "display_name": "Mine"})),
    )
    .await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{}",
        refused.json
    );
    let _ = w.root;
    w.t.drop_now().await;
}
