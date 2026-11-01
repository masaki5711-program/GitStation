#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The logouts and the refresh check over HTTP (M2 MR5, ADR-0021 §7, §8)
//! against the mock identity provider and a migrated database:
//! `POST /-/auth/logout` (the form's CSRF field, the 303 to the provider,
//! the cleared cookie, sessions only), `POST /-/auth/backchannel-logout`
//! (a valid token ends the session for the next request, an invalid one
//! is FG-AUTH-0017, credentials are never read), the shell's
//! `form-action`, and a person the provider ended being refused on their
//! next request (09 §8.5, the mock in place of Keycloak). Synthetic data
//! only.

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
use forge_oidc::test_support::{web_keys, MockOp};
use forge_oidc::{Oidc, RefreshingSessions};
use forge_web::server::PeerAddr;
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const ORIGIN: &str = "http://localhost:8080";
const PEER: &str = "192.0.2.20";

struct NoTokens;

impl Authenticator for NoTokens {
    fn authenticate<'a>(&'a self, _: &'a RequestId, _: &'a str) -> AuthFuture<'a> {
        Box::pin(async {
            Err(ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", "unknown"))
        })
    }
}

struct World {
    t: FreshDb,
    op: MockOp,
    app: Router,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = Arc::new(Oidc::new(op.settings(ORIGIN), &web_keys(), t.db.clone()).expect("oidc"));
    let lookups = Lookups {
        namespaces: Arc::new(t.db.clone()),
        projects: Arc::new(t.db.clone()),
    };
    let store: Arc<dyn SessionAuthenticator> = Arc::new(
        forge_db::sessions::SessionStore::new(t.db.clone(), Duration::from_secs(4 * 3600))
            .deferring_touch_to_refresh_check(),
    );
    // As `forge serve` wires it with auth.oidc.
    let sessions: Arc<dyn SessionAuthenticator> =
        Arc::new(RefreshingSessions::new(store, Arc::clone(&oidc)));
    let state = AppState::new("test", Vec::new(), Arc::new(NoTokens), lookups)
        .with_web_origin(ORIGIN)
        .with_sessions(sessions)
        .with_oidc(oidc);
    let app = forge_web::app::app(state, AppConfig::default()).expect("router");
    World { t, op, app }
}

fn request(method: &str, uri: &str, headers: &[(&str, &str)], body: &str) -> Request<Body> {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let mut req = req.body(Body::from(body.to_owned())).expect("request");
    let ip: IpAddr = PEER.parse().expect("ip");
    req.extensions_mut().insert(PeerAddr(ip));
    req
}

async fn call(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let res = app.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn set_cookies(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().expect("ascii").to_owned())
        .collect()
}

fn cookie_pair(headers: &HeaderMap, name: &str) -> String {
    set_cookies(headers)
        .into_iter()
        .find(|c| c.starts_with(&format!("{name}=")) && !c.contains("Max-Age=0"))
        .and_then(|c| c.split(';').next().map(str::to_owned))
        .unwrap_or_else(|| panic!("no {name} cookie"))
}

impl World {
    /// Sign in over HTTP; returns the session cookie pair and its CSRF token.
    async fn sign_in(&self) -> (String, String) {
        let (status, headers, _) = call(&self.app, request("GET", "/-/auth/login", &[], "")).await;
        assert_eq!(status, StatusCode::FOUND);
        let location = headers["location"].to_str().expect("location").to_owned();
        let state_cookie = cookie_pair(&headers, "__Host-gstn_oidc");
        let (code, state) = self.op.authorize(&location, json!({}));
        let uri = format!("/-/auth/callback?code={code}&state={state}");
        let (status, headers, body) = call(
            &self.app,
            request("GET", &uri, &[("cookie", &state_cookie)], ""),
        )
        .await;
        assert_eq!(status, StatusCode::FOUND, "{body}");
        let pair = cookie_pair(&headers, "__Host-gstn_session");
        let (status, _, me) = self.session(&pair).await;
        assert_eq!(status, StatusCode::OK, "{me}");
        let csrf = me["csrf_token"].as_str().expect("csrf").to_owned();
        (pair, csrf)
    }

    async fn session(&self, pair: &str) -> (StatusCode, HeaderMap, Value) {
        call(
            &self.app,
            request("GET", "/api/v1/session", &[("cookie", pair)], ""),
        )
        .await
    }

    async fn backchannel(
        &self,
        body: &str,
        extra: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Value) {
        let mut headers = vec![("content-type", "application/x-www-form-urlencoded")];
        headers.extend_from_slice(extra);
        call(
            &self.app,
            request("POST", "/-/auth/backchannel-logout", &headers, body),
        )
        .await
    }
}

#[tokio::test]
async fn the_logout_form_ends_the_session_and_goes_to_the_provider() {
    let w = world().await;
    let (pair, csrf) = w.sign_in().await;
    // Without the form field (and no header): the CSRF check refuses.
    let (status, _, body) = call(
        &w.app,
        request(
            "POST",
            "/-/auth/logout",
            &[
                ("cookie", &pair),
                ("origin", ORIGIN),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            "other=1",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error_code"], ErrorCode::CsrfRejected.id());
    // Another origin: refused before anything is ended.
    let form = format!("csrf={csrf}");
    let (status, _, body) = call(
        &w.app,
        request(
            "POST",
            "/-/auth/logout",
            &[
                ("cookie", &pair),
                ("origin", "https://evil.example.invalid"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            &form,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _, _) = w.session(&pair).await;
    assert_eq!(status, StatusCode::OK, "still signed in");

    let (status, headers, _) = call(
        &w.app,
        request(
            "POST",
            "/-/auth/logout",
            &[
                ("cookie", &pair),
                ("origin", ORIGIN),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            &form,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["cache-control"], "no-store");
    let location = headers["location"].to_str().expect("location");
    assert!(
        location.starts_with(&format!("{}/logout?", w.op.issuer)),
        "{location}"
    );
    assert!(location.contains("id_token_hint="), "{location}");
    assert!(
        set_cookies(&headers)
            .iter()
            .any(|c| c.starts_with("__Host-gstn_session=;") && c.contains("Max-Age=0")),
        "cookie cleared"
    );
    let (status, _, body) = w.session(&pair).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    // The header form of the token works the same (a fetch from the page).
    let (pair2, csrf2) = w.sign_in().await;
    let (status, _, _) = call(
        &w.app,
        request(
            "POST",
            "/-/auth/logout",
            &[
                ("cookie", &pair2),
                ("origin", ORIGIN),
                ("x-gitstation-csrf", &csrf2),
            ],
            "",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    // Anonymous: the guard refuses (the page only offers logout with a session).
    let (status, _, body) = call(
        &w.app,
        request("POST", "/-/auth/logout", &[("origin", ORIGIN)], ""),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    w.t.drop_now().await;
}

/// The errors.toml test of FG-AUTH-0017 over HTTP lives in forge-oidc
/// (`logout_token_failures_are_logout_token_invalid`); here the route:
/// a valid token ends the session for the next request, a bad body or
/// token is 400, and credentials on the request change nothing.
#[tokio::test]
async fn a_backchannel_logout_ends_the_session_for_the_next_request() {
    let w = world().await;
    let (pair, _) = w.sign_in().await;
    for (body, want) in [
        ("logout_token=not.a.jwt".to_owned(), "malformed"),
        ("nothing=here".to_owned(), "body"),
    ] {
        let (status, headers, err) = w.backchannel(&body, &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{want}: {err}");
        assert_eq!(err["error_code"], ErrorCode::LogoutTokenInvalid.id());
        assert_eq!(headers["cache-control"], "no-store");
    }
    let (status, _, err) = call(
        &w.app,
        request(
            "POST",
            "/-/auth/backchannel-logout",
            &[("content-type", "application/json")],
            "{}",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let (status, _, _) = w.session(&pair).await;
    assert_eq!(status, StatusCode::OK, "nothing ended yet");

    let token = w.op.logout_token(json!({}));
    // A garbage cookie and Authorization are not read on this public route.
    let (status, headers, body) = w
        .backchannel(
            &format!("logout_token={token}"),
            &[
                ("cookie", "__Host-gstn_session=gss_junk"),
                ("authorization", "Bearer junk"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    let (status, headers, body) = w.session(&pair).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error_code"], ErrorCode::Unauthenticated.id());
    assert!(set_cookies(&headers)
        .iter()
        .any(|c| c.starts_with("__Host-gstn_session=;")));
    w.t.drop_now().await;
}

/// 09 §8.5 with the mock in place of Keycloak: the provider ends the
/// person (the refresh token is refused); their next request after the
/// 5-minute check is refused and the cookie cleared.
#[tokio::test]
async fn a_person_the_provider_ended_is_refused_on_the_next_request() {
    let w = world().await;
    let (pair, _) = w.sign_in().await;
    w.op.revoke_refresh_tokens();
    let (status, _, _) = w.session(&pair).await;
    assert_eq!(status, StatusCode::OK, "not due yet: no call");
    w.t.exec_superuser(
        "UPDATE forge.web_sessions SET refresh_checked_at = now() - interval '301 seconds'",
    )
    .await;
    // Any cookie route runs the check, not only /api/v1/session (the
    // `decided_in` representative): here the caller's own account.
    let (status, headers, body) = call(
        &w.app,
        request("GET", "/api/v1/user", &[("cookie", &pair)], ""),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error_code"], ErrorCode::Unauthenticated.id());
    assert!(set_cookies(&headers)
        .iter()
        .any(|c| c.starts_with("__Host-gstn_session=;")));
    let (status, _, _) = w.session(&pair).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked for every route");
    w.t.drop_now().await;
}

/// The shell's CSP lets the logout form's redirect reach the provider.
#[tokio::test]
async fn the_shell_form_action_allows_the_identity_provider() {
    let w = world().await;
    let (_, headers, _) = call(&w.app, request("GET", "/acme", &[], "")).await;
    let csp = headers["content-security-policy"].to_str().expect("csp");
    let issuer_origin = url::Url::parse(&w.op.issuer)
        .expect("issuer")
        .origin()
        .ascii_serialization();
    assert!(
        csp.contains(&format!("form-action 'self' {issuer_origin}")),
        "{csp}"
    );
    w.t.drop_now().await;
}
