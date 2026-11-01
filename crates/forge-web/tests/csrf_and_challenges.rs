#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The web session cookie, CSRF and the 401 challenges (M2 MR3, ADR-0021
//! §6, §10, ADR-0019 Amendment M2) — the "MR3 に要るテスト" list of
//! ADR-0021 §6. Fake token and session authenticators stand in for
//! forge-db (no database), except the last test, which runs the real
//! session store (`forge_db::sessions`) through a disable.

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::{Action, Credential, InstanceCtx, ProjectCtx, Scope};
use forge_core::authenticator::{
    AuthFuture, Authenticator, SessionAuthFuture, SessionAuthenticator, SessionIdentity,
    TokenIdentity,
};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::session_token::{self, SessionToken};
use forge_core::web_path::{GitRoute, RouteWord};
use forge_core::{
    LookupPath, NamespaceId, ProjectAccess, ProjectId, ProjectRecord, ProjectRepository, RepoId,
    RepositoryState, RequestId, Resolution, Resolved, ResolvedTarget, UserId, Visibility,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_web::registry::{Dispatched, Registry, RouteScope, ScopedRoute, Verb};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const ORIGIN: &str = "https://git.example.test";
// A synthetic token in the real format (fgp_<16 hex>_<64 hex>).
const ALICE: &str =
    "fgp_00000000000000a1_1111111111111111111111111111111111111111111111111111111111111111";
const API: &str = "/api/v1/things";
const RAW: &str = "/acme/app/-/raw/main/README.md";
const ARCHIVE: &str = "/acme/app/-/archive/main.tar.gz";
const GIT: &str = "/acme/app.git/info/refs?service=git-upload-pack";
const BASIC: &str = "Basic realm=\"GitStation\", charset=\"UTF-8\"";
const BEARER: &str = "Bearer realm=\"GitStation\"";

// ── fakes ─────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Tokens {
    presented: Mutex<Vec<String>>,
}

impl Authenticator for Tokens {
    fn authenticate<'a>(&'a self, _: &'a RequestId, presented: &'a str) -> AuthFuture<'a> {
        self.presented
            .lock()
            .expect("presented")
            .push(presented.to_owned());
        let result = if presented == ALICE {
            Ok(TokenIdentity {
                token_id: 10,
                user_id: UserId::new(1),
                username: "alice".to_owned(),
                display_name: "Alice".to_owned(),
                is_admin: false,
                scopes: vec![Scope::ReadApi, Scope::Api, Scope::ReadRepository],
                is_agent: false,
            })
        } else {
            Err(ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", "unknown"))
        };
        Box::pin(async move { result })
    }
}

/// What a fake session stands for.
#[derive(Clone, Copy)]
enum Kind {
    Valid,
    Expired,
    Disabled,
    Outage,
}

/// Sessions by public id; records what it was asked.
#[derive(Default)]
struct Sessions {
    known: Mutex<Vec<(String, Kind)>>,
    presented: Mutex<Vec<String>>,
}

impl Sessions {
    fn add(&self, kind: Kind) -> SessionToken {
        let token = session_token::generate().expect("rng");
        self.known
            .lock()
            .expect("known")
            .push((token.public_id().as_str().to_owned(), kind));
        token
    }

    fn presented(&self) -> Vec<String> {
        self.presented.lock().expect("presented").clone()
    }
}

impl SessionAuthenticator for Sessions {
    fn authenticate_session<'a>(
        &'a self,
        _: &'a RequestId,
        presented: &'a str,
    ) -> SessionAuthFuture<'a> {
        self.presented
            .lock()
            .expect("presented")
            .push(presented.to_owned());
        let parsed = session_token::parse(presented);
        let kind = parsed.as_ref().ok().and_then(|t| {
            self.known
                .lock()
                .expect("known")
                .iter()
                .find(|(id, _)| id == t.public_id().as_str())
                .map(|(_, k)| *k)
        });
        let rejected = |reason: &'static str| {
            ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", reason)
        };
        let result =
            match (parsed, kind) {
                (Err(_), _) => Err(rejected("session_malformed")),
                (Ok(_), None) => Err(rejected("session_unknown")),
                (Ok(t), Some(Kind::Valid)) => Ok(SessionIdentity {
                    session_id: 7,
                    public_id: t.public_id().as_str().to_owned(),
                    user_id: UserId::new(2),
                    username: "bob".to_owned(),
                    display_name: "Bob".to_owned(),
                    is_admin: false,
                    idle_expires_at: "2026-11-01T16:00:00Z".to_owned(),
                    absolute_expires_at: "2026-11-02T00:00:00Z".to_owned(),
                    refresh_due: false,
                }),
                (Ok(_), Some(Kind::Expired)) => Err(rejected("session_expired")),
                (Ok(_), Some(Kind::Disabled)) => Err(ForgeError::new(ErrorCode::AccountDisabled)
                    .with_context("reason", "owner_disabled")),
                (Ok(_), Some(Kind::Outage)) => Err(ForgeError::new(ErrorCode::DbUnavailable)
                    .with_context("reason", "connect_refused")),
            };
        Box::pin(async move { result })
    }
}

/// Every path is one internal project.
struct OneProject;

impl forge_core::NamespaceResolver for OneProject {
    fn resolve<'a>(&'a self, _: &'a LookupPath, _: Option<UserId>) -> LookupFuture<'a, Resolution> {
        Box::pin(async {
            Ok(Resolution::Live(Resolved {
                namespace_id: NamespaceId::new(10),
                target: ResolvedTarget::Project {
                    id: ProjectId::new(11),
                    access: ProjectAccess {
                        visibility: Visibility::Internal,
                        role: None,
                        pending_deletion: false,
                    },
                    repository: ProjectRepository {
                        id: RepoId::new(12),
                        state: RepositoryState::Ready,
                    },
                },
            }))
        })
    }
}

impl forge_core::ProjectAccessLoader for OneProject {
    fn load_project<'a>(
        &'a self,
        _: ProjectId,
        _: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>> {
        Box::pin(async { Ok(None) })
    }
}

fn describe(request_user: Option<UserId>, credential: &Credential, headers: &HeaderMap) -> Value {
    json!({
        "user": request_user.map(UserId::get),
        "session": matches!(credential, Credential::Session),
        "cookie_seen": headers
            .get_all("cookie")
            .iter()
            .any(|v| v.to_str().is_ok_and(|v| v.contains("__Host-gstn_session"))),
        "cookies": headers
            .get_all("cookie")
            .iter()
            .map(|v| v.to_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>(),
    })
}

async fn instance(_: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let p = request.principal();
    Json(describe(p.user(), p.credential(), request.headers())).into_response()
}

async fn project(_: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let p = request.principal();
    Json(describe(p.user(), p.credential(), request.headers())).into_response()
}

fn app(tokens: Arc<Tokens>, sessions: Option<Arc<Sessions>>) -> axum::Router {
    let registry = Registry::new()
        .route(Verb::Get, API, Action::UserSelfRead, "test", instance)
        .route(Verb::Post, API, Action::UserSelfRead, "test", instance)
        .route(
            Verb::Get,
            forge_web::routes::session::PATH,
            Action::SessionRead,
            "test",
            forge_web::routes::session::read,
        )
        .scoped(
            RouteScope::Project,
            Verb::Get,
            ScopedRoute::Word(RouteWord::Raw),
            None,
            Action::RepoRead,
            "test",
            project,
        )
        .scoped(
            RouteScope::Project,
            Verb::Get,
            ScopedRoute::Word(RouteWord::Archive),
            None,
            Action::RepoRead,
            "test",
            project,
        )
        .scoped(
            RouteScope::ProjectGit,
            Verb::Get,
            ScopedRoute::Git(GitRoute::InfoRefsUploadPack),
            Some(("service", "git-upload-pack")),
            Action::RepoRead,
            "test",
            project,
        );
    let lookups = Lookups {
        namespaces: Arc::new(OneProject),
        projects: Arc::new(OneProject),
    };
    let mut state = AppState::new("test", Vec::new(), tokens, lookups).with_web_origin(ORIGIN);
    if let Some(sessions) = sessions {
        state = state.with_sessions(sessions);
    }
    forge_web::app::with_registry(registry, state, AppConfig::default()).expect("router")
}

fn fakes() -> (Arc<Tokens>, Arc<Sessions>) {
    (Arc::new(Tokens::default()), Arc::new(Sessions::default()))
}

fn request(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).expect("request")
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

fn cookie(token: &SessionToken) -> String {
    format!("__Host-gstn_session={}", token.reveal())
}

fn csrf(token: &SessionToken) -> String {
    token.csrf_token().expect("mac")
}

fn assert_csrf(status: StatusCode, body: &Value, case: &str) {
    assert_eq!(status, StatusCode::FORBIDDEN, "{case}: {body}");
    assert_eq!(body["error_code"], ErrorCode::CsrfRejected.id(), "{case}");
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

    fn failures(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).expect("json log line"))
            .filter(|e| e.get("error_code").is_some())
            .collect()
    }
}

// ── the session cookie ───────────────────────────────────────────────────

#[tokio::test]
async fn a_session_cookie_authenticates_the_api_as_a_session_and_is_not_passed_on() {
    let (tokens, sessions) = fakes();
    let s = sessions.add(Kind::Valid);
    let both = format!("theme=dark; {}; lang=ja", cookie(&s));
    let (status, _, body) = call(
        app(tokens, Some(Arc::clone(&sessions))),
        request("GET", API, &[("cookie", &both)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user"], 2);
    assert_eq!(body["session"], true);
    assert_eq!(body["cookie_seen"], false, "the session cookie is removed");
    assert_eq!(body["cookies"], json!(["theme=dark; lang=ja"]));
    assert_eq!(sessions.presented(), [s.reveal()]);
}

#[tokio::test]
async fn get_session_returns_the_user_and_this_sessions_csrf_token_and_refuses_tokens() {
    let (tokens, sessions) = fakes();
    let s = sessions.add(Kind::Valid);
    let router = app(tokens, Some(Arc::clone(&sessions)));
    let (status, headers, body) = call(
        router.clone(),
        request(
            "GET",
            "/api/v1/session",
            &[("cookie", &cookie(&s)), ("sec-fetch-mode", "cors")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user"]["id"], 2);
    assert_eq!(body["user"]["username"], "bob");
    assert_eq!(body["csrf_token"], csrf(&s));
    assert_eq!(body["public_id"], s.public_id().as_str());
    assert_eq!(body["absolute_expires_at"], "2026-11-02T00:00:00Z");
    assert!(!body.to_string().contains(&s.reveal()[21..]), "no secret");
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("private, no-store")
    );
    let (status, _, body) = call(
        router,
        request(
            "GET",
            "/api/v1/session",
            &[("authorization", &format!("Bearer {ALICE}"))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error_code"], ErrorCode::InsufficientScope.id());
}

#[tokio::test]
async fn an_expired_session_is_401_and_clears_the_cookie_but_an_outage_keeps_it() {
    let (tokens, sessions) = fakes();
    let expired = sessions.add(Kind::Expired);
    let down = sessions.add(Kind::Outage);
    let disabled = sessions.add(Kind::Disabled);
    let router = app(tokens, Some(Arc::clone(&sessions)));
    let clears = |headers: &HeaderMap| {
        headers.get_all("set-cookie").iter().any(|v| {
            v.to_str()
                .is_ok_and(|v| v.starts_with("__Host-gstn_session=;") && v.contains("Max-Age=0"))
        })
    };
    let (status, headers, body) = call(
        router.clone(),
        request("GET", API, &[("cookie", &cookie(&expired))]),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(clears(&headers), "{headers:?}");
    assert_eq!(challenges(&headers), [BEARER]);
    let (status, headers, body) = call(
        router.clone(),
        request("GET", API, &[("cookie", &cookie(&disabled))]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error_code"], ErrorCode::AccountDisabled.id());
    assert!(clears(&headers));
    let (status, headers, body) =
        call(router, request("GET", API, &[("cookie", &cookie(&down))])).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(!clears(&headers), "an outage is no verdict on the cookie");
}

#[tokio::test]
async fn a_server_without_a_session_store_fails_loudly_on_a_cookie() {
    let (tokens, sessions) = fakes();
    let s = sessions.add(Kind::Valid);
    let (status, _, body) = call(
        app(tokens, None),
        request("GET", API, &[("cookie", &cookie(&s))]),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error_code"], ErrorCode::Internal.id());
}

// ── ambiguity (ADR-0021 §10) ────────────────────────────────────────────

#[tokio::test]
async fn a_session_cookie_with_authorization_is_ambiguous_and_neither_is_tried() {
    let (tokens, sessions) = fakes();
    let s = sessions.add(Kind::Valid);
    let other = sessions.add(Kind::Valid);
    let two = format!("{}; {}", cookie(&s), cookie(&other));
    let bearer = format!("Bearer {ALICE}");
    for (path, headers) in [
        (
            API,
            vec![("cookie", cookie(&s)), ("authorization", bearer.clone())],
        ),
        (
            RAW,
            vec![("cookie", cookie(&s)), ("authorization", bearer.clone())],
        ),
        (API, vec![("cookie", two.clone())]),
    ] {
        let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (tokens_now, sessions_now) = (Arc::clone(&tokens), Arc::clone(&sessions));
        let logs = Captured::default();
        let _guard = logs.install();
        let (status, _, body) = call(
            app(tokens_now, Some(sessions_now)),
            request("GET", path, &headers),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}: {body}");
        let failures = logs.failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0]["reason"], "ambiguous");
    }
    assert!(tokens.presented.lock().expect("presented").is_empty());
    assert!(sessions.presented().is_empty(), "no credential is tried");
}

#[tokio::test]
async fn other_cookies_with_a_bearer_token_are_not_ambiguous() {
    let (tokens, sessions) = fakes();
    for other in [
        "__Host-gstn_oidc=sealed-state",
        "theme=dark",
        "__Host-gstn_session_old=x",
    ] {
        let (status, _, body) = call(
            app(Arc::clone(&tokens), Some(Arc::clone(&sessions))),
            request(
                "GET",
                API,
                &[
                    ("cookie", other),
                    ("authorization", &format!("Bearer {ALICE}")),
                ],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{other}: {body}");
        assert_eq!(body["session"], false);
    }
    assert!(sessions.presented().is_empty());
}

#[tokio::test]
async fn git_routes_ignore_the_session_cookie() {
    let (tokens, sessions) = fakes();
    let s = sessions.add(Kind::Valid);
    let router = app(Arc::clone(&tokens), Some(Arc::clone(&sessions)));
    // The cookie alone: no credential on git.
    let (status, headers, body) = call(
        router.clone(),
        request("GET", GIT, &[("cookie", &cookie(&s))]),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(
        challenges(&headers),
        [BASIC, BEARER],
        "git always gets Basic"
    );
    // The cookie with a token: the token, never ambiguous.
    let (status, _, body) = call(
        router,
        request(
            "GET",
            GIT,
            &[
                ("cookie", &cookie(&s)),
                ("authorization", &format!("Bearer {ALICE}")),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["session"], false);
    assert!(
        sessions.presented().is_empty(),
        "git never asks the session store"
    );
}

#[tokio::test]
async fn raw_and_archive_accept_the_session_cookie() {
    let (tokens, sessions) = fakes();
    let s = sessions.add(Kind::Valid);
    for path in [RAW, ARCHIVE] {
        let (status, _, body) = call(
            app(Arc::clone(&tokens), Some(Arc::clone(&sessions))),
            request(
                "GET",
                path,
                &[("cookie", &cookie(&s)), ("sec-fetch-dest", "image")],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(body["session"], true);
    }
}

// ── CSRF (ADR-0021 §6) ──────────────────────────────────────────────────

/// FG-AUTH-0016's catalog test: every refusal of a session's
/// state-changing request names its reason, is logged once, and the
/// complete request passes. GET is not checked.
#[tokio::test]
async fn csrf_rejections_name_their_reason() {
    let (tokens, sessions) = fakes();
    let s = sessions.add(Kind::Valid);
    let other = sessions.add(Kind::Valid);
    let (c, good, foreign) = (cookie(&s), csrf(&s), csrf(&other));
    let router = app(tokens, Some(Arc::clone(&sessions)));
    let cases: Vec<(Vec<(&str, &str)>, &str)> = vec![
        (vec![("origin", ORIGIN)], "token_missing"),
        (
            vec![("origin", ORIGIN), ("x-gitstation-csrf", "00")],
            "token_mismatch",
        ),
        (
            vec![("origin", ORIGIN), ("x-gitstation-csrf", &foreign)],
            "token_mismatch",
        ),
        (vec![("x-gitstation-csrf", &good)], "origin_missing"),
        (
            vec![
                ("origin", "https://other.example.test"),
                ("x-gitstation-csrf", &good),
            ],
            "origin_mismatch",
        ),
        (
            vec![
                ("origin", "http://git.example.test"),
                ("x-gitstation-csrf", &good),
            ],
            "origin_mismatch",
        ),
        (
            vec![
                ("origin", "https://git.example.test:8443"),
                ("x-gitstation-csrf", &good),
            ],
            "origin_mismatch",
        ),
        (
            vec![
                ("origin", ORIGIN),
                ("sec-fetch-site", "same-site"),
                ("x-gitstation-csrf", &good),
            ],
            "fetch_site",
        ),
        (
            vec![
                ("origin", ORIGIN),
                ("sec-fetch-site", "cross-site"),
                ("x-gitstation-csrf", &good),
            ],
            "fetch_site",
        ),
    ];
    for (extra, reason) in cases {
        let mut headers = vec![("cookie", c.as_str())];
        headers.extend(extra);
        let logs = Captured::default();
        let _guard = logs.install();
        let (status, _, body) = call(router.clone(), request("POST", API, &headers)).await;
        assert_csrf(status, &body, reason);
        let failures = logs.failures();
        assert_eq!(failures.len(), 1, "{reason}: logged once: {failures:?}");
        assert_eq!(failures[0]["message"], "forge.auth.csrf_rejected");
        assert_eq!(failures[0]["reason"], reason);
        assert_eq!(failures[0]["request_id"], body["request_id"]);
        let text = logs.text();
        assert!(
            !text.contains(&good) && !text.contains(&s.reveal()),
            "no secrets logged"
        );
    }
    let (status, _, body) = call(
        router.clone(),
        request(
            "POST",
            API,
            &[
                ("cookie", &c),
                ("origin", ORIGIN),
                ("sec-fetch-site", "same-origin"),
                ("x-gitstation-csrf", &good),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["session"], true);
    let (status, _, body) = call(router, request("GET", API, &[("cookie", &c)])).await;
    assert_eq!(status, StatusCode::OK, "a GET is not checked: {body}");
}

#[tokio::test]
async fn a_bearer_request_needs_no_csrf_token_but_a_browser_bearer_request_is_checked() {
    let (tokens, sessions) = fakes();
    let router = app(tokens, Some(sessions));
    let bearer = format!("Bearer {ALICE}");
    let (status, _, body) = call(
        router.clone(),
        request("POST", API, &[("authorization", &bearer)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = call(
        router.clone(),
        request(
            "POST",
            API,
            &[
                ("authorization", &bearer),
                ("origin", ORIGIN),
                ("sec-fetch-site", "same-origin"),
            ],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "same-origin Bearer from the page: {body}"
    );
    for (extra, reason) in [
        (("sec-fetch-site", "cross-site"), "fetch_site"),
        (("origin", "https://evil.example.test"), "origin_mismatch"),
    ] {
        let (status, _, body) = call(
            router.clone(),
            request("POST", API, &[("authorization", &bearer), extra]),
        )
        .await;
        assert_csrf(status, &body, reason);
    }
    // Anonymous browser requests are checked too (before authentication).
    let (status, _, body) = call(
        router,
        request("POST", GIT, &[("origin", "https://evil.example.test")]),
    )
    .await;
    assert_csrf(status, &body, "anonymous");
}

// ── 401 challenges (ADR-0019 Amendment M2) ──────────────────────────────

#[tokio::test]
async fn raw_and_archive_401s_drop_basic_for_browsers_and_the_api_never_offers_it() {
    let (tokens, sessions) = fakes();
    let expired = sessions.add(Kind::Expired);
    let router = app(tokens, Some(sessions));
    let expired_cookie = cookie(&expired);
    for path in [RAW, ARCHIVE] {
        for headers in [
            vec![("sec-fetch-mode", "navigate")],
            vec![("sec-fetch-dest", "image")],
            vec![("cookie", expired_cookie.as_str())],
        ] {
            let (status, h, _) = call(router.clone(), request("GET", path, &headers)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} {headers:?}");
            assert_eq!(challenges(&h), [BEARER], "{path} {headers:?}");
        }
        let (status, h, _) = call(router.clone(), request("GET", path, &[])).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(challenges(&h), [BASIC, BEARER], "{path}: curl gets Basic");
    }
    for headers in [
        vec![],
        vec![("sec-fetch-mode", "navigate")],
        vec![("cookie", expired_cookie.as_str())],
        vec![("authorization", "Basic Zm9vOmJhcg==")],
        vec![("authorization", "Bearer fgp_unknown")],
    ] {
        let (status, h, _) = call(router.clone(), request("GET", API, &headers)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{headers:?}");
        assert_eq!(
            challenges(&h),
            [BEARER],
            "/api/v1 never offers Basic: {headers:?}"
        );
    }
    for headers in [vec![], vec![("sec-fetch-mode", "navigate")]] {
        let (status, h, _) = call(router.clone(), request("GET", GIT, &headers)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(challenges(&h), [BASIC, BEARER], "git always: {headers:?}");
    }
}

// ── the real session store (needs the development database) ─────────────

/// ADR-0019 Amendment M2: after a disable, the next request with the
/// user's session cookie is FG-AUTH-0004 (and the cookie is cleared).
#[tokio::test]
async fn after_a_disable_the_next_request_with_the_session_is_account_disabled() {
    use forge_db::change::{Actor, ChangeContext};
    use forge_db::test_support::{issue_session, FreshDb};
    use forge_db::users::{self, NewUser};

    let t = FreshDb::migrated().await;
    let ctx = |r: &str| ChangeContext::new(Actor::System, RequestId::parse(r).expect("request id"));
    let alice = users::create(
        &t.db,
        &ctx("req-web-user"),
        &NewUser::human("alice", "Alice", "alice@example.test"),
    )
    .await
    .expect("user")
    .user_id;
    let issued = issue_session(&t.db, alice).await;
    let store: Arc<dyn SessionAuthenticator> = Arc::new(forge_db::sessions::SessionStore::new(
        t.db.clone(),
        std::time::Duration::from_secs(4 * 3600),
    ));
    let lookups = Lookups {
        namespaces: Arc::new(OneProject),
        projects: Arc::new(OneProject),
    };
    let state = AppState::new("test", Vec::new(), Arc::new(Tokens::default()), lookups)
        .with_web_origin(ORIGIN)
        .with_sessions(store);
    let registry = Registry::new().route(
        Verb::Get,
        forge_web::routes::session::PATH,
        Action::SessionRead,
        "test",
        forge_web::routes::session::read,
    );
    let router =
        forge_web::app::with_registry(registry, state, AppConfig::default()).expect("router");
    let cookie = format!("__Host-gstn_session={}", issued.token.reveal());
    let (status, _, body) = call(
        router.clone(),
        request("GET", "/api/v1/session", &[("cookie", &cookie)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user"]["id"], alice.get());
    assert_eq!(body["csrf_token"], issued.token.csrf_token().expect("mac"));
    let outcome = users::disable(&t.db, &ctx("req-web-disable"), alice)
        .await
        .expect("disable");
    assert_eq!(outcome, users::DisableOutcome::Disabled);
    let (status, headers, body) = call(
        router,
        request("GET", "/api/v1/session", &[("cookie", &cookie)]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error_code"], ErrorCode::AccountDisabled.id());
    assert!(headers.contains_key("set-cookie"), "the cookie is cleared");
    t.drop_now().await;
}
