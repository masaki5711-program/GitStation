#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Personal access tokens over HTTP (step D3 MR3, ADR-0019 §7): the
//! authentication middleware turns `Authorization: Bearer <token>` or
//! `Authorization: Basic` whose **password** is the token into a
//! `forge_auth::Principal` with `Credential::Token(scopes)`; the guard and
//! `authorize()` decide as before. A fake `Authenticator` stands in for
//! `forge_db::tokens::authenticate` (no database needed); `forge serve`'s
//! wiring of the real one is tested in forge-cli.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use forge_auth::{Action, Credential, InstanceCtx, Principal, Scope};
use forge_core::authenticator::{AuthFuture, Authenticator, TokenIdentity};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{LookupPath, ProjectId, ProjectRecord, Resolution};
use forge_core::{RequestId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_web::registry::{Dispatched, Registry, Verb};
use forge_web::{AppConfig, AppState, BoundedAuthenticator, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

// Synthetic tokens in the real format (fgp_<16 hex>_<64 hex>).
const ALICE: &str =
    "fgp_00000000000000a1_1111111111111111111111111111111111111111111111111111111111111111";
const ADMIN_NO_SCOPE: &str =
    "fgp_00000000000000b2_2222222222222222222222222222222222222222222222222222222222222222";
const ADMIN_WITH_SCOPE: &str =
    "fgp_00000000000000b3_3333333333333333333333333333333333333333333333333333333333333333";
const DISABLED_OWNER: &str =
    "fgp_00000000000000c4_4444444444444444444444444444444444444444444444444444444444444444";
const DB_DOWN: &str =
    "fgp_00000000000000d5_5555555555555555555555555555555555555555555555555555555555555555";
const REVOKED: &str =
    "fgp_00000000000000e6_6666666666666666666666666666666666666666666666666666666666666666";

const CORRUPT_ROW: &str =
    "fgp_00000000000000f7_7777777777777777777777777777777777777777777777777777777777777777";
/// Never answers (the request timeout must end the request).
const HANGS: &str =
    "fgp_00000000000000f8_8888888888888888888888888888888888888888888888888888888888888888";

/// A synthetic user password: never a credential (hand-off 9).
const USER_PASSWORD: &str = "correct-horse-battery-staple";
const BASIC_USER: &str = "alice-the-username";

/// Records what it was asked and answers from a fixed table.
#[derive(Default)]
struct FakeAuthenticator {
    presented: Mutex<Vec<String>>,
}

impl FakeAuthenticator {
    fn presented(&self) -> Vec<String> {
        self.presented.lock().expect("presented").clone()
    }
}

fn identity(user: i64, is_admin: bool, scopes: &[Scope]) -> TokenIdentity {
    TokenIdentity {
        token_id: user * 10,
        user_id: UserId::new(user),
        username: format!("user{user}"),
        display_name: format!("User {user}"),
        is_admin,
        scopes: scopes.to_vec(),
        is_agent: false,
    }
}

fn rejected(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", reason)
}

impl Authenticator for FakeAuthenticator {
    fn authenticate<'a>(
        &'a self,
        _request_id: &'a RequestId,
        presented: &'a str,
    ) -> AuthFuture<'a> {
        self.presented
            .lock()
            .expect("presented")
            .push(presented.to_owned());
        let result = match presented {
            ALICE => Ok(identity(1, false, &[Scope::ReadApi, Scope::Api])),
            ADMIN_NO_SCOPE => Ok(identity(2, true, &[Scope::Api])),
            ADMIN_WITH_SCOPE => Ok(identity(2, true, &[Scope::Api, Scope::Admin])),
            DISABLED_OWNER => Err(ForgeError::new(ErrorCode::AccountDisabled)
                .with_context("reason", "owner_disabled")
                .with_context("user", 4)
                .with_context("public_id", "00000000000000c4")),
            DB_DOWN => Err(ForgeError::new(ErrorCode::DbUnavailable)
                .with_context("reason", "connect_refused")
                .with_context("target", "db-host.internal:5432/forge")),
            REVOKED => Err(rejected("revoked").with_context("public_id", "00000000000000e6")),
            CORRUPT_ROW => Err(ForgeError::new(ErrorCode::CredentialDataInvalid)
                .with_context("reason", "bad_hash")
                .with_context("token_id", 70)),
            HANGS => {
                return Box::pin(async {
                    std::future::pending::<()>().await;
                    Err(rejected("unreachable"))
                })
            }
            other if other.starts_with("fgp_") => Err(rejected("unknown")),
            _ => Err(rejected("malformed")),
        };
        Box::pin(async move { result })
    }
}

/// Runs only after the registry proved `UserCreate` on the instance with
/// the principal the authentication middleware built.
async fn whoami(_: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let principal = request.principal();
    let scopes: Vec<&str> = match principal.credential() {
        Credential::Token(scopes) => scopes.iter().map(|s| s.as_str()).collect(),
        Credential::Session => vec!["<session>"],
    };
    Json(json!({
        "user": principal.user().map(|u| u.get()),
        "scopes": scopes,
        "admin_rights": principal.admin_rights(),
        "authorization_header_seen": request.headers().contains_key("authorization"),
    }))
    .into_response()
}

/// No lookups in these tests (the dispatcher's own tests script them).
struct NoNamespaces;

impl forge_core::NamespaceResolver for NoNamespaces {
    fn resolve<'a>(&'a self, _: &'a LookupPath, _: Option<UserId>) -> LookupFuture<'a, Resolution> {
        Box::pin(async { Ok(Resolution::Absent) })
    }
}

impl forge_core::ProjectAccessLoader for NoNamespaces {
    fn load_project<'a>(
        &'a self,
        _: ProjectId,
        _: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>> {
        Box::pin(async { Ok(None) })
    }
}

async fn healthy(_: AppState, _: Dispatched<InstanceCtx>) -> &'static str {
    "ok"
}

fn router(auth: Arc<FakeAuthenticator>) -> axum::Router {
    router_with(auth, AppConfig::default())
}

fn router_with(auth: Arc<dyn Authenticator>, config: AppConfig) -> axum::Router {
    let registry = Registry::new()
        .route(
            Verb::Post,
            "/admin/users",
            Action::UserCreate,
            "test",
            whoami,
        )
        .route(Verb::Get, "/healthz", Action::Health, "test", healthy);
    let lookups = Lookups {
        namespaces: Arc::new(NoNamespaces),
        projects: Arc::new(NoNamespaces),
    };
    let state = AppState::new("test", Vec::new(), auth, lookups);
    forge_web::app::with_registry(registry, state, config).expect("router")
}

fn post(headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::post("/admin/users");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).expect("request")
}

fn basic(user: &str, password: &str) -> String {
    let raw = format!("{user}:{password}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

async fn call(router: axum::Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, json)
}

fn challenges(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("www-authenticate")
        .iter()
        .map(|v| v.to_str().expect("ascii").to_owned())
        .collect()
}

fn assert_unauthenticated(status: StatusCode, headers: &HeaderMap, body: &Value) {
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error_code"], ErrorCode::Unauthenticated.id());
    let challenges = challenges(headers);
    assert!(
        challenges.iter().any(|c| c.starts_with("Basic realm=")),
        "git needs a Basic challenge to ask its credential helper: {challenges:?}"
    );
}

// ── log capture (a JSON subscriber writing to this thread's buffer) ──────

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

thread_local! {
    static SINK: std::cell::RefCell<Option<Captured>> = const { std::cell::RefCell::new(None) };
}

struct ThreadSink;

impl Write for ThreadSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        SINK.with(|s| {
            if let Some(c) = s.borrow().as_ref() {
                c.0.lock().expect("log buffer").extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        SINK.with(|s| s.borrow_mut().take());
    }
}

impl Captured {
    /// Capture this thread's events (`#[tokio::test]` runs on the current
    /// thread, so the request's events land here).
    fn install(&self) -> CaptureGuard {
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(|| ThreadSink)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("the only global subscriber");
        });
        SINK.with(|s| *s.borrow_mut() = Some(self.clone()));
        CaptureGuard
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("log buffer").clone()).expect("utf-8 logs")
    }

    fn events(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|l| serde_json::from_str(l).expect("json log line"))
            .collect()
    }

    /// The events that carry an error code.
    fn failures(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e.get("error_code").is_some())
            .collect()
    }
}

// ── tests ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn bearer_token_reaches_the_guarded_handler_as_its_owner() {
    let auth = Arc::new(FakeAuthenticator::default());
    let (status, _, body) = call(
        router(Arc::clone(&auth)),
        post(&[("authorization", &bearer(ADMIN_WITH_SCOPE))]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user"], 2);
    assert_eq!(body["scopes"], json!(["api", "admin"]));
    assert_eq!(body["admin_rights"], true);
    assert_eq!(
        body["authorization_header_seen"], false,
        "the credential is removed before the handler"
    );
    assert_eq!(auth.presented(), [ADMIN_WITH_SCOPE]);
}

#[tokio::test]
async fn the_bearer_scheme_is_case_insensitive() {
    let auth = Arc::new(FakeAuthenticator::default());
    let header = format!("bearer {ADMIN_WITH_SCOPE}");
    let (status, _, body) = call(router(auth), post(&[("authorization", &header)])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn basic_auth_whose_password_is_a_token_authenticates_and_ignores_the_username() {
    let auth = Arc::new(FakeAuthenticator::default());
    let (status, _, body) = call(
        router(Arc::clone(&auth)),
        post(&[("authorization", &basic(BASIC_USER, ADMIN_WITH_SCOPE))]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["user"], 2,
        "the token decides who it is, not the username"
    );
    assert_eq!(
        auth.presented(),
        [ADMIN_WITH_SCOPE],
        "only the password is presented"
    );
}

#[tokio::test]
async fn basic_auth_with_a_user_password_or_a_wrong_token_is_unauthenticated() {
    for password in [USER_PASSWORD, REVOKED, ""] {
        let auth = Arc::new(FakeAuthenticator::default());
        let (status, headers, body) = call(
            router(auth),
            post(&[("authorization", &basic(BASIC_USER, password))]),
        )
        .await;
        assert_unauthenticated(status, &headers, &body);
    }
}

#[tokio::test]
async fn an_unknown_or_revoked_bearer_token_is_unauthenticated() {
    let unknown =
        "fgp_00000000000000ff_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    for token in [unknown, REVOKED, "not-a-token"] {
        let auth = Arc::new(FakeAuthenticator::default());
        let (status, headers, body) =
            call(router(auth), post(&[("authorization", &bearer(token))])).await;
        assert_unauthenticated(status, &headers, &body);
    }
}

#[tokio::test]
async fn malformed_or_unsupported_authorization_headers_are_unauthenticated() {
    let not_base64 = "Basic %%%not-base64%%%".to_owned();
    let no_colon = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(ALICE)
    );
    let cases = [
        not_base64,
        no_colon,
        "Bearer".to_owned(),
        "Bearer ".to_owned(),
        format!("Digest {ALICE}"),
        ALICE.to_owned(),
    ];
    for header in &cases {
        let auth = Arc::new(FakeAuthenticator::default());
        let (status, headers, body) = call(
            router(Arc::clone(&auth)),
            post(&[("authorization", header.as_str())]),
        )
        .await;
        assert_unauthenticated(status, &headers, &body);
        assert!(
            !auth.presented().iter().any(|p| p == ALICE),
            "{header:?} must not be accepted as a token"
        );
    }
}

#[tokio::test]
async fn two_authorization_headers_are_ambiguous_and_neither_is_tried() {
    let auth = Arc::new(FakeAuthenticator::default());
    let logs = Captured::default();
    let _guard = logs.install();
    let (status, headers, body) = call(
        router(Arc::clone(&auth)),
        post(&[
            ("authorization", &bearer(ALICE)),
            ("authorization", &basic(BASIC_USER, ADMIN_WITH_SCOPE)),
        ]),
    )
    .await;
    assert_unauthenticated(status, &headers, &body);
    assert!(auth.presented().is_empty(), "no token is tried");
    let failures = logs.failures();
    let rejection = failures
        .iter()
        .find(|e| e["error_code"] == ErrorCode::Unauthenticated.id())
        .expect("the rejection is logged");
    assert_eq!(rejection["reason"], "ambiguous", "{rejection}");
}

#[tokio::test]
async fn a_token_on_top_of_an_existing_principal_is_ambiguous() {
    let auth = Arc::new(FakeAuthenticator::default());
    let mut req = post(&[("authorization", &bearer(ALICE))]);
    req.extensions_mut().insert(Principal::authenticated(
        UserId::new(9),
        false,
        true,
        Credential::Session,
    ));
    let (status, headers, body) = call(router(Arc::clone(&auth)), req).await;
    assert_unauthenticated(status, &headers, &body);
    assert!(auth.presented().is_empty());
}

#[tokio::test]
async fn no_header_on_a_guarded_route_is_unauthenticated_with_a_basic_challenge() {
    let auth = Arc::new(FakeAuthenticator::default());
    let (status, headers, body) = call(router(Arc::clone(&auth)), post(&[])).await;
    assert_unauthenticated(status, &headers, &body);
    assert!(auth.presented().is_empty());
}

#[tokio::test]
async fn public_routes_work_without_credentials_and_never_consult_the_authenticator() {
    for headers in [vec![], vec![("authorization", bearer(DB_DOWN))]] {
        let auth = Arc::new(FakeAuthenticator::default());
        let mut b = Request::get("/healthz");
        for (k, v) in &headers {
            b = b.header(*k, v.as_str());
        }
        let res = router(Arc::clone(&auth))
            .oneshot(b.body(Body::empty()).expect("request"))
            .await
            .expect("infallible");
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            auth.presented().is_empty(),
            "liveness never depends on the token store"
        );
    }
}

#[tokio::test]
async fn a_database_outage_during_authentication_is_503_not_401() {
    let auth = Arc::new(FakeAuthenticator::default());
    let (status, headers, body) =
        call(router(auth), post(&[("authorization", &bearer(DB_DOWN))])).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error_code"], ErrorCode::DbUnavailable.id());
    assert!(
        challenges(&headers).is_empty(),
        "a 503 must not make git drop its credentials"
    );
    assert!(!body.to_string().contains("db-host"), "{body}");
}

#[tokio::test]
async fn a_disabled_owners_token_is_account_disabled_before_the_handler() {
    let auth = Arc::new(FakeAuthenticator::default());
    let (status, _, body) = call(
        router(auth),
        post(&[("authorization", &basic(BASIC_USER, DISABLED_OWNER))]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error_code"], ErrorCode::AccountDisabled.id());
}

#[tokio::test]
async fn an_administrators_token_without_the_admin_scope_has_no_admin_rights() {
    let auth = Arc::new(FakeAuthenticator::default());
    let (status, _, body) = call(
        router(auth),
        post(&[("authorization", &bearer(ADMIN_NO_SCOPE))]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error_code"], ErrorCode::Forbidden.id());
}

#[tokio::test]
async fn a_rejected_token_is_logged_once_with_request_id_reason_and_public_id() {
    let auth = Arc::new(FakeAuthenticator::default());
    let logs = Captured::default();
    let _guard = logs.install();
    let (status, headers, body) =
        call(router(auth), post(&[("authorization", &bearer(REVOKED))])).await;
    assert_unauthenticated(status, &headers, &body);
    let failures = logs.failures();
    assert_eq!(failures.len(), 1, "logged once: {failures:?}");
    let e = &failures[0];
    assert_eq!(e["message"], "forge.auth.token_rejected", "{e}");
    assert_eq!(e["error_code"], ErrorCode::Unauthenticated.id());
    assert_eq!(e["request_id"], body["request_id"]);
    assert_eq!(e["reason"], "revoked");
    assert_eq!(e["public_id"], "00000000000000e6");
    assert_eq!(e["scheme"], "bearer");
    assert!(
        e.get("context").is_none(),
        "one representation (fields): {e}"
    );
}

#[tokio::test]
async fn a_disabled_owner_is_logged_once_with_the_owner_and_no_challenge() {
    let auth = Arc::new(FakeAuthenticator::default());
    let logs = Captured::default();
    let _guard = logs.install();
    let (status, headers, body) = call(
        router(auth),
        post(&[("authorization", &bearer(DISABLED_OWNER))]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(challenges(&headers).is_empty(), "a 403 is no challenge");
    let failures = logs.failures();
    assert_eq!(failures.len(), 1, "logged once: {failures:?}");
    let e = &failures[0];
    assert_eq!(e["message"], "forge.auth.token_rejected", "{e}");
    assert_eq!(e["error_code"], ErrorCode::AccountDisabled.id());
    assert_eq!(e["request_id"], body["request_id"]);
    assert_eq!(e["reason"], "owner_disabled");
    // Internal ids are logged like FG-AUTH-0004's catalog diagnosis says.
    assert_eq!(e["user"], "4");
}

#[tokio::test]
async fn a_database_outage_is_logged_once_by_the_renderer() {
    let auth = Arc::new(FakeAuthenticator::default());
    let logs = Captured::default();
    let _guard = logs.install();
    let (status, headers, body) =
        call(router(auth), post(&[("authorization", &bearer(DB_DOWN))])).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(challenges(&headers).is_empty());
    let failures = logs.failures();
    assert_eq!(failures.len(), 1, "logged once: {failures:?}");
    assert_eq!(failures[0]["error_code"], ErrorCode::DbUnavailable.id());
    assert_eq!(failures[0]["request_id"], body["request_id"]);
    assert_ne!(failures[0]["message"], "forge.auth.token_rejected");
}

#[tokio::test]
async fn unreadable_stored_credentials_pass_through_as_a_non_401() {
    let auth = Arc::new(FakeAuthenticator::default());
    let (status, headers, body) = call(
        router(auth),
        post(&[("authorization", &bearer(CORRUPT_ROW))]),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error_code"], ErrorCode::CredentialDataInvalid.id());
    assert!(challenges(&headers).is_empty());
}

#[tokio::test]
async fn the_authorization_header_is_removed_for_basic_too() {
    let auth = Arc::new(FakeAuthenticator::default());
    let (status, _, body) = call(
        router(auth),
        post(&[("authorization", &basic(BASIC_USER, ADMIN_WITH_SCOPE))]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["authorization_header_seen"], false);
}

#[tokio::test]
async fn a_scheme_separated_by_a_tab_is_accepted() {
    let auth = Arc::new(FakeAuthenticator::default());
    let header = format!("Bearer\t{ADMIN_WITH_SCOPE}");
    let (status, _, body) = call(router(auth), post(&[("authorization", &header)])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_hanging_authenticator_is_ended_by_the_request_timeout() {
    let auth: Arc<dyn Authenticator> = Arc::new(FakeAuthenticator::default());
    let config = AppConfig {
        request_timeout: Duration::from_millis(50),
        ..AppConfig::default()
    };
    let (status, _, body) = call(
        router_with(auth, config),
        post(&[("authorization", &bearer(HANGS))]),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error_code"], ErrorCode::RequestTimeout.id());
}

#[tokio::test]
async fn an_unknown_path_with_a_bad_header_never_consults_the_authenticator() {
    let auth = Arc::new(FakeAuthenticator::default());
    let req = Request::get("/no/such/path")
        .header("authorization", bearer(REVOKED))
        .body(Body::empty())
        .expect("request");
    let (status, _, body) = call(router(Arc::clone(&auth)), req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error_code"], ErrorCode::RouteNotFound.id());
    assert!(auth.presented().is_empty());
}

/// Counts how many authentications run at once.
#[derive(Default)]
struct Concurrency {
    now: AtomicUsize,
    max: AtomicUsize,
}

impl Authenticator for Concurrency {
    fn authenticate<'a>(
        &'a self,
        _request_id: &'a RequestId,
        _presented: &'a str,
    ) -> AuthFuture<'a> {
        Box::pin(async move {
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.max.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.now.fetch_sub(1, Ordering::SeqCst);
            Ok(identity(2, true, &[Scope::Api, Scope::Admin]))
        })
    }
}

#[tokio::test]
async fn no_more_than_the_permitted_authentications_run_at_once() {
    let inner = Arc::new(Concurrency::default());
    let bounded: Arc<dyn Authenticator> = Arc::new(BoundedAuthenticator::new(
        Arc::clone(&inner) as Arc<dyn Authenticator>,
        2,
    ));
    let router = router_with(bounded, AppConfig::default());
    let calls = (0..10).map(|_| {
        call(
            router.clone(),
            post(&[("authorization", &bearer(ADMIN_WITH_SCOPE))]),
        )
    });
    let results = futures_util::future::join_all(calls).await;
    assert!(
        results.iter().all(|(s, _, _)| *s == StatusCode::OK),
        "every request is served once a permit is free"
    );
    assert_eq!(inner.max.load(Ordering::SeqCst), 2, "at most 2 at once");
}

#[test]
fn zero_permits_still_allow_one_authentication() {
    let inner: Arc<dyn Authenticator> = Arc::new(Concurrency::default());
    assert_eq!(BoundedAuthenticator::new(inner, 0).permits(), 1);
}

#[tokio::test]
async fn tokens_usernames_and_header_values_never_reach_the_logs() {
    let logs = Captured::default();
    let _guard = logs.install();
    let wrong =
        "fgp_00000000000000ff_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let requests = [
        vec![("authorization", bearer(ALICE))],
        vec![("authorization", bearer(wrong))],
        vec![("authorization", basic(BASIC_USER, USER_PASSWORD))],
        vec![("authorization", basic(BASIC_USER, REVOKED))],
        vec![("authorization", basic(BASIC_USER, DB_DOWN))],
        vec![("authorization", basic(BASIC_USER, DISABLED_OWNER))],
        vec![("authorization", format!("Digest {USER_PASSWORD}"))],
        vec![("authorization", USER_PASSWORD.to_owned())],
        vec![
            ("authorization", bearer(ALICE)),
            ("authorization", basic(BASIC_USER, USER_PASSWORD)),
        ],
    ];
    let mut secrets: Vec<String> = vec![
        ALICE.into(),
        wrong.into(),
        REVOKED.into(),
        DB_DOWN.into(),
        DISABLED_OWNER.into(),
        USER_PASSWORD.into(),
        BASIC_USER.into(),
    ];
    for headers in &requests {
        for (_, v) in headers {
            secrets.push(v.clone());
            // The secret halves alone, too.
            if let Some(b64) = v.strip_prefix("Basic ") {
                secrets.push(b64.to_owned());
            }
        }
        let auth = Arc::new(FakeAuthenticator::default());
        let mut b = Request::post("/admin/users");
        for (k, v) in headers {
            b = b.header(*k, v.as_str());
        }
        let _ = call(router(auth), b.body(Body::empty()).expect("request")).await;
    }
    for token in [ALICE, wrong, REVOKED, DB_DOWN, DISABLED_OWNER] {
        // The secret part (after the public id) is the credential.
        secrets.push(token[21..].to_owned());
    }
    let text = logs.text();
    assert!(!logs.failures().is_empty(), "the failures were logged");
    for secret in &secrets {
        assert!(
            !text.contains(secret.as_str()),
            "{secret:?} leaked into the logs"
        );
    }
}
