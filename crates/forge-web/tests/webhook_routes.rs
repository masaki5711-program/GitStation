#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The webhook routes of M3 I2 through the product router against the
//! development database (ADR-0011), with the real loaders and commands: a
//! Maintainer creates (the secret shown once), reads, changes, rotates and
//! deletes; a Developer, an agent's token, a service account and a person
//! without a role are refused; a URL outside the operator's allowlist is
//! `400 FG-INT-0010` with one sentence (the reason only in the log); a
//! webhook or delivery of another project is not found. Principals are
//! inserted as request extensions. Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::webhook::{TargetPolicy, WebhookSecretKeys};
use forge_core::{ProjectId, RequestId, Role, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_integ::webhooks::Webhooks;
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

fn router(db: &Db) -> axum::Router {
    let keys = WebhookSecretKeys::parse(&format!("1 {}\n", "5a".repeat(32))).expect("keys");
    let policy = TargetPolicy::parse("kanflow.internal:443", false).expect("policy");
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
    .with_webhooks(Webhooks::new(db.clone(), policy, Some(Arc::new(keys))));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn person(t: &FreshDb, name: &str) -> users::Created {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(
            name,
            &format!("Synthetic {name}"),
            &format!("{name}@example.test"),
        ),
    )
    .await
    .expect("user")
}

struct Answer {
    status: StatusCode,
    json: Value,
    retry_after: Option<String>,
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
    let retry_after = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
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
        retry_after,
    }
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

fn token(user: UserId, scopes: &[Scope], agent: bool) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::Human,
    )
    .as_agent(agent)
}

struct World {
    t: FreshDb,
    alice: UserId,
    dev: UserId,
    outsider: UserId,
    web: ProjectId,
    other: ProjectId,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = person(&t, "alice").await;
    let dev = person(&t, "dev").await.user_id;
    let outsider = person(&t, "outsider").await.user_id;
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web")
    .project_id;
    let other = projects::create(
        &t.db,
        &ctx("req-other"),
        &NewProject::new("other", alice.namespace_id, "Other", alice.user_id),
    )
    .await
    .expect("other")
    .project_id;
    projects::add_member(&t.db, &ctx("req-dev"), web, dev, Role::Developer)
        .await
        .expect("developer");
    World {
        t,
        alice: alice.user_id,
        dev,
        outsider,
        web,
        other,
    }
}

fn hooks_uri(project: ProjectId) -> String {
    format!("/api/v1/projects/{}/webhooks", project.get())
}

#[tokio::test]
async fn a_maintainer_manages_a_webhook_and_sees_its_secret_once() {
    let w = world().await;
    let db = &w.t.db;
    let created = call(
        db,
        "POST",
        &hooks_uri(w.web),
        session(w.alice),
        Some(json!({"url": "https://kanflow.internal/hook?token=receiver", "event_types": ["merge_request.*"]})),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    let secret = created.json["secret"].as_str().expect("secret");
    assert!(secret.starts_with("whsec_"));
    let hook = created.json["webhook"]["id"].as_i64().expect("id");
    assert_eq!(created.json["webhook"]["owner_id"], w.alice.get());
    assert_eq!(
        created.json["webhook"]["event_types"],
        json!(["merge_request.*"])
    );
    let one = format!("{}/{hook}", hooks_uri(w.web));
    // Reads never show a secret.
    let listed = call(db, "GET", &hooks_uri(w.web), session(w.alice), None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    assert_eq!(listed.json["items"].as_array().map(Vec::len), Some(1));
    assert!(!listed.json.to_string().contains(secret));
    let read = call(
        db,
        "GET",
        &one,
        token(w.alice, &[Scope::ReadApi], false),
        None,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.json);
    assert!(read.json.get("secret").is_none());
    // Change it.
    let updated = call(
        db,
        "PATCH",
        &one,
        session(w.alice),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.json);
    assert_eq!(updated.json["outcome"], "updated");
    assert_eq!(updated.json["fields"], json!(["enabled"]));
    assert_eq!(updated.json["webhook"]["disabled_reason"], "manual");
    // Rotate: a new secret, the previous one still signing for 24 hours.
    let rotated = call(db, "POST", &format!("{one}/secret"), session(w.alice), None).await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.json);
    assert_eq!(rotated.json["secret_version"], 2);
    assert_ne!(rotated.json["secret"].as_str(), Some(secret));
    assert!(rotated.json["previous_secret_valid_until"].is_string());
    // The (empty) history, and a delivery that is not there.
    let deliveries = call(
        db,
        "GET",
        &format!("{one}/deliveries?limit=5"),
        session(w.alice),
        None,
    )
    .await;
    assert_eq!(deliveries.status, StatusCode::OK, "{}", deliveries.json);
    assert_eq!(deliveries.json["items"], json!([]));
    assert_eq!(deliveries.json["next_before"], Value::Null);
    let missing = call(
        db,
        "POST",
        &format!("{one}/deliveries/999/redeliver"),
        session(w.alice),
        None,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.json);
    let bad_query = call(
        db,
        "GET",
        &format!("{one}/deliveries?limit=101"),
        session(w.alice),
        None,
    )
    .await;
    assert_eq!(bad_query.status, StatusCode::BAD_REQUEST);
    // Delete.
    let deleted = call(db, "DELETE", &one, session(w.alice), None).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let gone = call(db, "GET", &one, session(w.alice), None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    w.t.drop_now().await;
}

#[tokio::test]
async fn webhooks_are_a_maintainers_and_managing_them_a_persons() {
    let w = world().await;
    let db = &w.t.db;
    let body = json!({"url": "https://kanflow.internal/hook"});
    for (who, status, code) in [
        (session(w.dev), StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        (session(w.outsider), StatusCode::NOT_FOUND, "FG-REPO-0005"),
        (
            token(w.alice, &[Scope::Api], true),
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
        (
            token(w.alice, &[Scope::ReadApi], false),
            StatusCode::FORBIDDEN,
            "FG-AUTH-0002",
        ),
        (
            Principal::authenticated(
                w.alice,
                false,
                true,
                Credential::Token(vec![Scope::Api]),
                AccountKind::ServiceAccount,
            ),
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
    ] {
        let refused = call(db, "POST", &hooks_uri(w.web), who, Some(body.clone())).await;
        assert_eq!(refused.status, status, "{}", refused.json);
        assert_eq!(refused.code(), code, "{}", refused.json);
    }
    // Reading: a Developer no, an agent's token of a Maintainer yes.
    let dev = call(db, "GET", &hooks_uri(w.web), session(w.dev), None).await;
    assert_eq!(dev.status, StatusCode::FORBIDDEN, "{}", dev.json);
    let agent = call(
        db,
        "GET",
        &hooks_uri(w.web),
        token(w.alice, &[Scope::ReadApi], true),
        None,
    )
    .await;
    assert_eq!(agent.status, StatusCode::OK, "{}", agent.json);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_target_outside_the_allowlist_is_400_int_0010_with_one_sentence() {
    let w = world().await;
    for url in [
        "https://127.0.0.1/hook",
        "https://169.254.169.254/latest/meta-data",
        "https://user:pw@kanflow.internal/hook",
        "https://elsewhere.internal/hook",
        "http://kanflow.internal/hook",
    ] {
        let refused = call(
            &w.t.db,
            "POST",
            &hooks_uri(w.web),
            session(w.alice),
            Some(json!({"url": url})),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{url}: {}",
            refused.json
        );
        assert_eq!(refused.code(), "FG-INT-0010", "{url}");
        let text = refused.json.to_string();
        assert!(!text.contains("reason"), "{text}");
        assert!(!text.contains("169.254"), "{text}");
        assert!(!text.contains("pw@"), "{text}");
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn another_projects_webhook_is_not_found() {
    let w = world().await;
    let db = &w.t.db;
    let created = call(
        db,
        "POST",
        &hooks_uri(w.web),
        session(w.alice),
        Some(json!({"url": "https://kanflow.internal/hook"})),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    let hook = created.json["webhook"]["id"].as_i64().expect("id");
    let across = format!("{}/{hook}", hooks_uri(w.other));
    for (method, uri) in [
        ("GET", across.clone()),
        ("DELETE", across.clone()),
        ("POST", format!("{across}/secret")),
        ("GET", format!("{across}/deliveries")),
        ("POST", format!("{across}/deliveries/1/redeliver")),
    ] {
        let refused = call(db, method, &uri, session(w.alice), None).await;
        assert_eq!(
            refused.status,
            StatusCode::NOT_FOUND,
            "{method} {uri}: {}",
            refused.json
        );
        assert_eq!(refused.code(), "FG-REPO-0005");
    }
    let still = call(
        db,
        "GET",
        &format!("{}/{hook}", hooks_uri(w.web)),
        session(w.alice),
        None,
    )
    .await;
    assert_eq!(still.status, StatusCode::OK);
    assert!(still.retry_after.is_none());
    w.t.drop_now().await;
}
