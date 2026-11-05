#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The notification settings routes of M3 N1 through the product router
//! against the development database (ADR-0011), with the real loader and
//! command: `GET|PUT /api/v1/user/notification-preferences`. Only the
//! person's own settings are reachable (there is no way to name another
//! user), scopes decide read versus write, a bad body is a precise
//! `FG-WEB-0006`, and a stored value never needs a second request to be
//! read back. The permission matrix's rows over HTTP. Principals are
//! inserted as request extensions (the authentication layer is tested
//! elsewhere). Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal, Scope};
use forge_core::{RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_notify::Settings;
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const URI: &str = "/api/v1/user/notification-preferences";

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
    let state = state(db).with_notifications(Settings::new(db.clone()));
    forge_web::app(state, AppConfig::default()).expect("router")
}

async fn user(t: &FreshDb, name: &str, admin: bool) -> UserId {
    let mut new = NewUser::human(
        name,
        &format!("Synthetic {name}"),
        &format!("{name}@example.test"),
    );
    new.is_admin = admin;
    users::create(
        &t.db,
        &ChangeContext::new(
            Actor::System,
            RequestId::parse(&format!("req-user-{name}")).expect("id"),
        ),
        &new,
    )
    .await
    .expect("user")
    .user_id
}

struct Answer {
    status: StatusCode,
    json: Value,
    cache_control: String,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }
}

async fn send(
    router: axum::Router,
    method: &str,
    uri: &str,
    who: Option<Principal>,
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
    if let Some(who) = who {
        req.extensions_mut().insert(who);
    }
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let cache_control = res
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let raw = res.into_body().collect().await.expect("body").to_bytes();
    Answer {
        status,
        json: serde_json::from_slice(&raw).unwrap_or(Value::Null),
        cache_control,
    }
}

async fn call(db: &Db, method: &str, who: Option<Principal>, body: Option<Value>) -> Answer {
    send(router(db), method, URI, who, body).await
}

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(
        user,
        admin,
        true,
        Credential::Session,
        forge_auth::AccountKind::Human,
    )
}

fn token(user: UserId, scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        forge_auth::AccountKind::Human,
    )
}

fn kinds() -> Value {
    json!([
        "mr_approved",
        "mr_merged",
        "mr_commented",
        "mr_conflict_predicted"
    ])
}

#[tokio::test]
async fn a_person_reads_the_defaults_and_replaces_their_settings() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;

    let got = call(&t.db, "GET", Some(session(alice, false)), None).await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.json);
    assert_eq!(
        got.json,
        json!({"email_enabled": true, "language": null, "muted_kinds": [], "kinds": kinds()})
    );
    assert_eq!(got.cache_control, "private, no-store");

    let put = call(
        &t.db,
        "PUT",
        Some(session(alice, false)),
        Some(json!({"email_enabled": false, "language": "id", "muted_kinds": ["mr_commented", "mr_approved", "mr_commented"]})),
    )
    .await;
    assert_eq!(put.status, StatusCode::OK, "{}", put.json);
    // Normalised: sorted, distinct.
    assert_eq!(
        put.json,
        json!({"email_enabled": false, "language": "id", "muted_kinds": ["mr_approved", "mr_commented"], "kinds": kinds()})
    );
    let again = call(&t.db, "GET", Some(session(alice, false)), None).await;
    assert_eq!(again.json, put.json);
    // Bob's are his own and untouched.
    let bobs = call(&t.db, "GET", Some(session(bob, false)), None).await;
    assert_eq!(bobs.json["email_enabled"], true);
    assert_eq!(bobs.json["language"], Value::Null);

    // A second PUT replaces (language back to the default, nothing muted).
    let reset = call(
        &t.db,
        "PUT",
        Some(session(alice, false)),
        Some(json!({"email_enabled": true, "language": null, "muted_kinds": []})),
    )
    .await;
    assert_eq!(reset.status, StatusCode::OK);
    assert_eq!(reset.json["muted_kinds"], json!([]));
    assert_eq!(reset.json["language"], Value::Null);
    t.drop_now().await;
}

#[tokio::test]
async fn scopes_decide_read_versus_write_and_anonymous_is_refused() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let body = json!({"email_enabled": false, "language": "ja", "muted_kinds": []});

    // read_api reads; api writes; a repository token neither.
    let read = call(&t.db, "GET", Some(token(alice, &[Scope::ReadApi])), None).await;
    assert_eq!(read.status, StatusCode::OK);
    let refused = call(
        &t.db,
        "PUT",
        Some(token(alice, &[Scope::ReadApi])),
        Some(body.clone()),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.json);
    let written = call(
        &t.db,
        "PUT",
        Some(token(alice, &[Scope::Api])),
        Some(body.clone()),
    )
    .await;
    assert_eq!(written.status, StatusCode::OK, "{}", written.json);
    let repo = call(
        &t.db,
        "GET",
        Some(token(alice, &[Scope::ReadRepository])),
        None,
    )
    .await;
    assert_eq!(repo.status, StatusCode::FORBIDDEN);
    // The refused write changed nothing (the earlier api write is what is stored).
    let stored = call(&t.db, "GET", Some(session(alice, false)), None).await;
    assert_eq!(stored.json["language"], "ja");

    for method in ["GET", "PUT"] {
        let anon = call(&t.db, method, None, Some(body.clone())).await;
        assert_eq!(anon.status, StatusCode::UNAUTHORIZED, "{method}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn nobody_can_name_another_users_settings_not_even_an_administrator() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let root = user(&t, "root", true).await;
    call(
        &t.db,
        "PUT",
        Some(session(alice, false)),
        Some(json!({"email_enabled": false, "language": "en", "muted_kinds": []})),
    )
    .await;
    // The route has no user in it: the administrator reads and writes
    // their own, and another user's address does not exist.
    let mine = call(&t.db, "GET", Some(session(root, true)), None).await;
    assert_eq!(mine.json["email_enabled"], true);
    for (method, uri) in [
        (
            "GET",
            format!("/api/v1/users/{}/notification-preferences", alice.get()),
        ),
        (
            "PUT",
            format!(
                "/api/v1/admin/users/{}/notification-preferences",
                alice.get()
            ),
        ),
        ("GET", format!("{URI}?user_id={}", alice.get())),
    ] {
        let answer = send(
            router(&t.db),
            method,
            &uri,
            Some(session(root, true)),
            (method == "PUT")
                .then(|| json!({"email_enabled": true, "language": null, "muted_kinds": []})),
        )
        .await;
        if uri.starts_with(URI) {
            // A query parameter is not an address: still the caller's own.
            assert_eq!(answer.json["email_enabled"], true, "{uri}: {}", answer.json);
        } else {
            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{uri}");
        }
    }
    let alices = call(&t.db, "GET", Some(session(alice, false)), None).await;
    assert_eq!(alices.json["email_enabled"], false, "untouched");
    t.drop_now().await;
}

#[tokio::test]
async fn a_bad_body_is_a_precise_field_error_and_changes_nothing() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let who = || Some(session(alice, false));
    let ok = json!({"email_enabled": true, "language": "en", "muted_kinds": ["mr_merged"]});
    assert_eq!(
        call(&t.db, "PUT", who(), Some(ok.clone())).await.status,
        StatusCode::OK
    );

    for (body, field) in [
        (
            json!({"email_enabled": true, "language": "fr", "muted_kinds": []}),
            "language",
        ),
        (
            json!({"email_enabled": true, "language": "en", "muted_kinds": ["mr_closed"]}),
            "muted_kinds",
        ),
        (
            json!({"email_enabled": true, "language": "en", "muted_kinds": ["MR_MERGED"]}),
            "muted_kinds",
        ),
    ] {
        let bad = call(&t.db, "PUT", who(), Some(body)).await;
        assert_eq!(
            (bad.status, bad.code()),
            (StatusCode::BAD_REQUEST, "FG-WEB-0006"),
            "{}",
            bad.json
        );
        // The field is in the log, not in what a person sees.
        assert!(
            !bad.json.to_string().contains(field),
            "{field}: {}",
            bad.json
        );
    }
    for body in [
        json!({"language": "en", "muted_kinds": []}),
        json!({"email_enabled": "yes", "language": "en", "muted_kinds": []}),
        json!({"email_enabled": true, "language": "en", "muted_kinds": [], "user_id": 2}),
        json!({"email_enabled": true, "language": "en", "muted_kinds": "mr_merged"}),
        json!([]),
    ] {
        let bad = call(&t.db, "PUT", who(), Some(body.clone())).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{body}: {}", bad.json);
    }
    // Nothing changed.
    let stored = call(&t.db, "GET", who(), None).await;
    assert_eq!(
        stored.json,
        json!({"email_enabled": true, "language": "en", "muted_kinds": ["mr_merged"], "kinds": kinds()})
    );
    t.drop_now().await;
}

#[tokio::test]
async fn without_the_settings_handle_the_route_names_what_is_missing() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let router = forge_web::app(state(&t.db), AppConfig::default()).expect("router");
    let answer = send(router, "GET", URI, Some(session(alice, false)), None).await;
    assert_eq!(
        (answer.status, answer.code()),
        (StatusCode::INTERNAL_SERVER_ERROR, "FG-SYS-0001")
    );
    t.drop_now().await;
}
