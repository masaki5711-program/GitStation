#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The sign-in routes over HTTP (M2 MR4, ADR-0021 §3): `GET /-/auth/login`
//! and `/-/auth/callback` against the mock identity provider and a migrated
//! database — the cookies and redirects, the rate limit of the start
//! (FG-WEB-0010), the unconfigured server (FG-AUTH-0018), and a provider
//! outage that stops new sign-ins only (FG-AUTH-0015; the signed-in session
//! keeps working). Synthetic data only.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Router;
use forge_core::authenticator::{AuthFuture, Authenticator, SessionAuthenticator};
use forge_core::RequestId;
use forge_db::test_support::FreshDb;
use forge_errors::{ErrorCode, ForgeError};
use forge_oidc::test_support::{web_keys, Endpoint, Fault, MockOp};
use forge_oidc::Oidc;
use forge_web::server::PeerAddr;
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const ORIGIN: &str = "http://localhost:8080";

/// No tokens in these tests.
struct NoTokens;

impl Authenticator for NoTokens {
    fn authenticate<'a>(&'a self, _: &'a RequestId, _: &'a str) -> AuthFuture<'a> {
        Box::pin(async {
            Err(ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", "unknown"))
        })
    }
}

fn state(t: &FreshDb) -> AppState {
    let lookups = Lookups {
        namespaces: Arc::new(t.db.clone()),
        projects: Arc::new(t.db.clone()),
    };
    let sessions: Arc<dyn SessionAuthenticator> = Arc::new(forge_db::sessions::SessionStore::new(
        t.db.clone(),
        Duration::from_secs(4 * 3600),
    ));
    AppState::new("test", Vec::new(), Arc::new(NoTokens), lookups)
        .with_web_origin(ORIGIN)
        .with_sessions(sessions)
}

fn router(state: AppState) -> Router {
    forge_web::app::app(state, AppConfig::default()).expect("router")
}

fn get(uri: &str, cookie: Option<&str>, peer: &str) -> Request<Body> {
    let mut req = Request::builder().method("GET").uri(uri);
    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }
    let mut req = req.body(Body::empty()).expect("request");
    let ip: IpAddr = peer.parse().expect("ip");
    req.extensions_mut().insert(PeerAddr(ip));
    req
}

async fn call(router: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let res = router.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}

fn set_cookies(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().expect("ascii").to_owned())
        .collect()
}

/// `name=value` of the Set-Cookie that sets `name` (not a clearing one).
fn cookie_pair(headers: &HeaderMap, name: &str) -> String {
    set_cookies(headers)
        .into_iter()
        .find(|c| c.starts_with(&format!("{name}=")) && !c.contains("Max-Age=0"))
        .and_then(|c| c.split(';').next().map(str::to_owned))
        .unwrap_or_else(|| panic!("no {name} cookie"))
}

/// One whole sign-in over HTTP; returns the session cookie pair.
async fn sign_in(app: &Router, op: &MockOp, peer: &str) -> (StatusCode, HeaderMap, Value) {
    let (status, headers, _) = call(app, get("/-/auth/login?next=%2Facme", None, peer)).await;
    assert_eq!(status, StatusCode::FOUND);
    let location = headers["location"].to_str().expect("location").to_owned();
    let state_cookie = cookie_pair(&headers, "__Host-gstn_oidc");
    let (code, state) = op.authorize(&location, json!({}));
    let uri = format!("/-/auth/callback?code={code}&state={state}");
    call(app, get(&uri, Some(&state_cookie), peer)).await
}

#[tokio::test]
async fn the_browser_signs_in_and_the_session_works() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = Oidc::new(op.settings(ORIGIN), &web_keys(), t.db.clone()).expect("oidc");
    let app = router(state(&t).with_oidc(Arc::new(oidc)));

    let (status, headers, _) =
        call(&app, get("/-/auth/login?next=%2Facme", None, "192.0.2.10")).await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(headers["cache-control"], "no-store");
    let state_cookie = set_cookies(&headers)
        .into_iter()
        .find(|c| c.starts_with("__Host-gstn_oidc="))
        .expect("state cookie");
    for attr in [
        "Max-Age=600",
        "Path=/",
        "Secure",
        "HttpOnly",
        "SameSite=Lax",
    ] {
        assert!(state_cookie.contains(attr), "{attr}: {state_cookie}");
    }
    assert!(!state_cookie.contains("Domain"), "{state_cookie}");

    let (status, headers, body) = sign_in(&app, &op, "192.0.2.10").await;
    assert_eq!(status, StatusCode::FOUND, "{body}");
    assert_eq!(headers["location"], "/acme");
    assert_eq!(headers["cache-control"], "no-store");
    let cookies = set_cookies(&headers);
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with("__Host-gstn_oidc=;") && c.contains("Max-Age=0")),
        "state cleared: {cookies:?}"
    );
    let session = cookies
        .iter()
        .find(|c| c.starts_with("__Host-gstn_session=gss_"))
        .expect("session cookie");
    assert!(
        session.contains("Max-Age=43200") && session.contains("HttpOnly"),
        "{session}"
    );
    let pair = cookie_pair(&headers, "__Host-gstn_session");
    let (status, _, me) = call(&app, get("/api/v1/session", Some(&pair), "192.0.2.10")).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["user"]["username"], "alice");

    // A failed callback clears the state cookie too, and names the code only.
    let (status, headers, body) = call(
        &app,
        get("/-/auth/callback?code=x&state=y", None, "192.0.2.10"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error_code"], ErrorCode::LoginStateInvalid.id());
    assert_eq!(body.as_object().map(|o| o.len()), Some(3), "{body}");
    assert!(set_cookies(&headers)
        .iter()
        .any(|c| c.starts_with("__Host-gstn_oidc=;")));
    t.drop_now().await;
}

/// The errors.toml test of FG-AUTH-0018.
#[tokio::test]
async fn login_without_oidc_is_login_not_configured() {
    let t = FreshDb::migrated().await;
    let app = router(state(&t));
    for uri in ["/-/auth/login", "/-/auth/callback?code=a&state=b"] {
        let (status, headers, body) = call(&app, get(uri, None, "192.0.2.1")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(body["error_code"], ErrorCode::LoginNotConfigured.id());
        assert!(body["request_id"].is_string());
        assert_eq!(headers["cache-control"], "no-store");
    }
    t.drop_now().await;
}

/// The errors.toml test of FG-WEB-0010.
#[tokio::test]
async fn login_start_is_rate_limited_per_peer() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = Oidc::new(op.settings(ORIGIN), &web_keys(), t.db.clone()).expect("oidc");
    let app = router(state(&t).with_oidc(Arc::new(oidc)));
    for _ in 0..forge_web::routes::auth::LOGIN_LIMIT {
        let (status, _, _) = call(&app, get("/-/auth/login", None, "198.51.100.7")).await;
        assert_eq!(status, StatusCode::FOUND);
    }
    let (status, headers, body) = call(&app, get("/-/auth/login", None, "198.51.100.7")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error_code"], ErrorCode::RateLimited.id());
    assert!(headers.contains_key("retry-after"));
    let (status, _, _) = call(&app, get("/-/auth/login", None, "198.51.100.8")).await;
    assert_eq!(status, StatusCode::FOUND, "another peer is not limited");
    t.drop_now().await;
}

/// Keycloak unreachable: new sign-ins fail with FG-AUTH-0015 (503), the
/// session issued before keeps working (ADR-0021 §1, plan 09 §8.2).
#[tokio::test]
async fn an_idp_outage_stops_new_sign_ins_but_not_signed_in_users() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = Oidc::new(op.settings(ORIGIN), &web_keys(), t.db.clone()).expect("oidc");
    let app = router(state(&t).with_oidc(Arc::new(oidc)));
    let (status, headers, body) = sign_in(&app, &op, "192.0.2.20").await;
    assert_eq!(status, StatusCode::FOUND, "{body}");
    let pair = cookie_pair(&headers, "__Host-gstn_session");

    op.set_fault(Endpoint::Token, Some(Fault::Delay(Duration::from_secs(3))));
    let (status, _, body) = sign_in(&app, &op, "192.0.2.21").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        body["error_code"],
        ErrorCode::IdentityProviderUnavailable.id()
    );
    let (status, _, me) = call(&app, get("/api/v1/session", Some(&pair), "192.0.2.20")).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    t.drop_now().await;
}

/// One whole sign-in over HTTP whose ID token has `overrides`.
async fn sign_in_as(
    app: &Router,
    op: &MockOp,
    peer: &str,
    overrides: Value,
) -> (StatusCode, HeaderMap, Value) {
    let (status, headers, _) = call(app, get("/-/auth/login", None, peer)).await;
    assert_eq!(status, StatusCode::FOUND);
    let location = headers["location"].to_str().expect("location").to_owned();
    let state_cookie = cookie_pair(&headers, "__Host-gstn_oidc");
    let (code, state) = op.authorize(&location, overrides);
    let uri = format!("/-/auth/callback?code={code}&state={state}");
    call(app, get(&uri, Some(&state_cookie), peer)).await
}

/// The callback is limited per peer like the start, counted apart; the
/// refusal clears the state cookie.
#[tokio::test]
async fn the_callback_is_rate_limited_per_peer() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = Oidc::new(op.settings(ORIGIN), &web_keys(), t.db.clone()).expect("oidc");
    let app = router(state(&t).with_oidc(Arc::new(oidc)));
    let uri = "/-/auth/callback?code=c&state=s";
    for _ in 0..forge_web::routes::auth::LOGIN_LIMIT {
        let (status, _, body) = call(&app, get(uri, None, "198.51.100.17")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error_code"], ErrorCode::LoginStateInvalid.id());
    }
    let (status, headers, body) = call(&app, get(uri, None, "198.51.100.17")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error_code"], ErrorCode::RateLimited.id());
    assert!(headers.contains_key("retry-after"));
    assert!(set_cookies(&headers)
        .iter()
        .any(|c| c.starts_with("__Host-gstn_oidc=;") && c.contains("Max-Age=0")));
    let (status, _, _) = call(&app, get(uri, None, "198.51.100.18")).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "another peer is not limited"
    );
    // The starts have their own count.
    let (status, _, _) = call(&app, get("/-/auth/login", None, "198.51.100.17")).await;
    assert_eq!(status, StatusCode::FOUND);
    t.drop_now().await;
}

/// A disabled user, a bot and a person without the access role are
/// refused over HTTP with FG-AUTH-0013: no session cookie, no session row.
#[tokio::test]
async fn refused_accounts_get_no_session() {
    use forge_db::change::{Actor, ChangeContext};
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = Oidc::new(op.settings(ORIGIN), &web_keys(), t.db.clone()).expect("oidc");
    let app = router(state(&t).with_oidc(Arc::new(oidc)));
    let system = || ChangeContext::new(Actor::System, RequestId::generate());

    let mut alice = forge_db::users::NewUser::human("alice", "Alice", "alice@example.test");
    alice.employee_number = Some("SYN0001".to_owned());
    let alice = forge_db::users::create(&t.db, &system(), &alice)
        .await
        .expect("alice")
        .user_id;
    let _linked =
        forge_oidc::operator_link_identity(&t.db, &system(), alice, &op.issuer, "sub-off")
            .await
            .expect("link");
    let _off = forge_db::users::disable(&t.db, &system(), alice)
        .await
        .expect("disable");
    let robot = forge_db::users::NewUser {
        kind: forge_core::UserKind::Bot,
        email: None,
        ..forge_db::users::NewUser::human("robot", "Robot", "unused@example.test")
    };
    let robot = forge_db::users::create(&t.db, &system(), &robot)
        .await
        .expect("robot")
        .user_id;
    let _linked =
        forge_oidc::operator_link_identity(&t.db, &system(), robot, &op.issuer, "sub-robot")
            .await
            .expect("link robot");

    let cases = [
        ("user_disabled", json!({"sub": "sub-off"})),
        (
            "not_human",
            json!({"sub": "sub-robot", "employee_number": "SYN0900", "email": "robot@example.test"}),
        ),
        (
            "no_access_role",
            json!({"sub": "sub-new", "employee_number": "SYN0901", "email": "new@example.test",
                   "resource_access": {"gitstation": {"roles": []}}}),
        ),
    ];
    for (i, (why, overrides)) in cases.into_iter().enumerate() {
        let peer = format!("192.0.2.{}", 40 + i);
        let (status, headers, body) = sign_in_as(&app, &op, &peer, overrides).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{why}: {body}");
        assert_eq!(
            body["error_code"],
            ErrorCode::LoginNotPermitted.id(),
            "{why}"
        );
        let cookies = set_cookies(&headers);
        assert!(
            !cookies
                .iter()
                .any(|c| c.starts_with("__Host-gstn_session=")),
            "{why}: {cookies:?}"
        );
        assert!(
            cookies
                .iter()
                .any(|c| c.starts_with("__Host-gstn_oidc=;") && c.contains("Max-Age=0")),
            "{why}: the state cookie is cleared: {cookies:?}"
        );
    }
    assert!(t
        .json_rows("SELECT id FROM forge.web_sessions")
        .await
        .is_empty());
    t.drop_now().await;
}
