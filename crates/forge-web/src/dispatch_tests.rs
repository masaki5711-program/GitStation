//! The dispatcher (ADR-0015 "判定の順序", M1 phase 8 MR4) with a scripted
//! resolver that counts its calls: the shared URL vectors, the four ADR
//! dispatcher tests, exactly one resolver call per resolved request (must
//! d), byte-identical denials for absent / tombstone / kind mismatch /
//! invisible (must f), redirects only after authorize(), and the log
//! fields (reasons, `log_safe_path`, `namespace.redirected`).

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use forge_auth::{Action, Credential, Principal, ProjectCtx, Scope};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    GroupId, LookupPath, NamespaceId, NamespacePath, ProjectAccess, ProjectId, ProjectRecord,
    Resolution, Resolved, ResolvedTarget, Role, TombstoneId, UserId, Visibility,
};
use forge_errors::{ErrorCode, ForgeError};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::app::{build, AppConfig, AppState, Lookups};
use crate::context::REQUEST_ID_HEADER;
use crate::registry::{Dispatched, Registry, RouteScope, ScopedRoute, Verb};
use crate::ui::shell_html;
use forge_core::web_path::RouteWord;

// ── the scripted resolver ────────────────────────────────────────────────

/// Answers from a script by folded path (everything else is absent) and
/// counts every call.
#[derive(Default)]
struct Script {
    answers: HashMap<String, Resolution>,
    calls: AtomicUsize,
    hang: bool,
    fail: Option<ErrorCode>,
}

impl Script {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl forge_core::NamespaceResolver for Script {
    fn resolve<'a>(
        &'a self,
        path: &'a LookupPath,
        _viewer: Option<UserId>,
    ) -> LookupFuture<'a, Resolution> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = self
            .answers
            .get(&path.full_path())
            .cloned()
            .unwrap_or(Resolution::Absent);
        let hang = self.hang;
        let fail = self.fail;
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            match fail {
                Some(code) => Err(ForgeError::new(code).with_context("reason", "scripted")),
                None => Ok(answer),
            }
        })
    }
}

impl forge_core::ProjectAccessLoader for Script {
    fn load_project<'a>(
        &'a self,
        _project: ProjectId,
        _viewer: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>> {
        Box::pin(async { Ok(None) })
    }
}

const VISIBLE: i64 = 1;
const HIDDEN: i64 = 2;

fn project(id: i64, role: Option<Role>) -> Resolved {
    Resolved {
        namespace_id: NamespaceId::new(id * 10),
        target: ResolvedTarget::Project {
            id: ProjectId::new(id),
            access: ProjectAccess {
                visibility: Visibility::Private,
                role,
                pending_deletion: false,
            },
            repository: forge_core::ProjectRepository {
                id: forge_core::RepoId::new(id * 100),
                state: forge_core::RepositoryState::Ready,
            },
        },
    }
}

fn ns(raw: &str) -> NamespacePath {
    NamespacePath::parse(raw).expect("namespace path")
}

/// - `acme/visible`: a private project the caller is Reporter of;
/// - `acme/hidden`: a private project the caller has no role on;
/// - `acme/gone`: a deleted tombstone; `acme`: a group; `alice`: a user;
/// - `acme/old` → `acme/visible`, `acme/old-hidden` → `acme/hidden`: moved.
fn script() -> Script {
    let mut answers = HashMap::new();
    answers.insert(
        "acme/visible".to_owned(),
        Resolution::Live(project(VISIBLE, Some(Role::Reporter))),
    );
    answers.insert(
        "acme/hidden".to_owned(),
        Resolution::Live(project(HIDDEN, None)),
    );
    answers.insert(
        "acme/gone".to_owned(),
        Resolution::Deleted {
            tombstone: TombstoneId::new(9),
        },
    );
    answers.insert(
        "acme".to_owned(),
        Resolution::Live(Resolved {
            namespace_id: NamespaceId::new(30),
            target: ResolvedTarget::Group {
                id: GroupId::new(3),
                viewer_role: None,
            },
        }),
    );
    answers.insert(
        "alice".to_owned(),
        Resolution::Live(Resolved {
            namespace_id: NamespaceId::new(40),
            target: ResolvedTarget::User(UserId::new(4)),
        }),
    );
    answers.insert(
        "acme/old".to_owned(),
        Resolution::Moved {
            resolved: project(VISIBLE, Some(Role::Reporter)),
            tombstone: TombstoneId::new(11),
            current: ns("acme/visible"),
        },
    );
    answers.insert(
        "acme/old-hidden".to_owned(),
        Resolution::Moved {
            resolved: project(HIDDEN, None),
            tombstone: TombstoneId::new(12),
            current: ns("acme/hidden"),
        },
    );
    Script {
        answers,
        ..Script::default()
    }
}

// ── routers ──────────────────────────────────────────────────────────────

/// The project content route these tests mount instead of the product's
/// (`routes::content`, tested with real git in `routes/content_tests.rs`):
/// echoes what the handler may read — the proof's id, the tail and the
/// canonical path.
async fn content(_: AppState, request: Dispatched<ProjectCtx>) -> axum::Json<Value> {
    axum::Json(json!({
        "action": request.proof().action().name(),
        "project": request.proof().id().get(),
        "tail": request.tail().as_slice(),
        "canonical": request.canonical_path().map(NamespacePath::full_path),
    }))
}

fn state_with(script: &Arc<Script>) -> AppState {
    let lookups = Lookups {
        namespaces: Arc::clone(script) as Arc<dyn forge_core::NamespaceResolver>,
        projects: Arc::clone(script) as Arc<dyn forge_core::ProjectAccessLoader>,
    };
    AppState::new("test", Vec::new(), crate::tests::no_tokens(), lookups)
        .with_lookup_timeout(Duration::from_millis(100))
}

/// The product registry without its content routes, plus an echoing
/// `raw` (GET / HEAD / POST, `RepoRead`) on projects.
fn with_content(registry: Registry<AppState>) -> Registry<AppState> {
    let raw = ScopedRoute::Word(RouteWord::Raw);
    registry
        .scoped(
            RouteScope::Project,
            Verb::Get,
            raw,
            None,
            Action::RepoRead,
            "raw",
            content,
        )
        .scoped(
            RouteScope::Project,
            Verb::Head,
            raw,
            None,
            Action::RepoRead,
            "raw",
            content,
        )
        .scoped(
            RouteScope::Project,
            Verb::Post,
            raw,
            None,
            Action::RepoRead,
            "raw",
            content,
        )
}

fn router(registry: Registry<AppState>, script: &Arc<Script>) -> axum::Router {
    build(registry, state_with(script), AppConfig::default()).expect("router")
}

fn product(script: &Arc<Script>) -> axum::Router {
    router(crate::app::registry(), script)
}

fn content_app(script: &Arc<Script>) -> axum::Router {
    router(with_content(crate::app::registry_without_content()), script)
}

fn reporter() -> Principal {
    Principal::authenticated(UserId::new(5), false, true, Credential::Session)
}

fn request(method: &str, uri: &str, principal: Option<Principal>) -> Request<Body> {
    let mut r = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    if let Some(p) = principal {
        r.extensions_mut().insert(p);
    }
    r
}

/// Status, headers (name, bytes) and body of an answer without its id.
type Comparable = (StatusCode, Vec<(String, Vec<u8>)>, Vec<u8>);

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }

    fn code(&self) -> Value {
        self.json()["error_code"].clone()
    }

    /// Status, headers and body with the request id taken out: what must
    /// be byte-identical between two denials (must f).
    fn without_request_id(&self) -> Comparable {
        let id = self
            .headers
            .get(&REQUEST_ID_HEADER)
            .map(|v| v.as_bytes().to_vec())
            .unwrap_or_default();
        let headers = self
            .headers
            .iter()
            .filter(|(k, _)| *k != REQUEST_ID_HEADER)
            .map(|(k, v)| (k.as_str().to_owned(), v.as_bytes().to_vec()))
            .collect();
        let text = String::from_utf8(self.bytes.clone()).expect("utf-8 body");
        let id = String::from_utf8(id).expect("ascii id");
        assert!(
            !id.is_empty() && text.contains(&id),
            "the body names the request id"
        );
        (self.status, headers, text.replace(&id, "<id>").into_bytes())
    }
}

async fn send(router: axum::Router, req: Request<Body>) -> Answer {
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    Answer {
        status,
        headers,
        bytes,
    }
}

// ── shared URL vectors ───────────────────────────────────────────────────

const VECTORS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/url-vectors.json"
));

fn uri_of(v: &Value) -> Option<Uri> {
    let path = v["path"].as_str().expect("path");
    let full = match v.get("query").and_then(Value::as_str) {
        Some(q) => format!("{path}?{q}"),
        None => path.to_owned(),
    };
    full.parse::<Uri>().ok().filter(|u| u.path() == path)
}

/// Every vector the HTTP stack can carry reaches the dispatcher: a rejected
/// one is `FG-WEB-0001` (never the shell), an accepted page URL is the
/// shell, an accepted URL whose route is not built yet (`raw`, `archive`,
/// git) is `FG-WEB-0001` — and nothing is looked up for any of them.
#[tokio::test]
async fn url_vectors_through_the_dispatcher() {
    let doc: Value = serde_json::from_str(VECTORS).expect("url-vectors.json");
    let script = Arc::new(script());
    let mut carried = 0;
    let registry = crate::app::registry();
    let fixed_paths: Vec<&str> = registry
        .defs()
        .iter()
        .filter(|d| d.scope == crate::registry::RouteScope::Fixed && d.verb != Verb::Get)
        .filter(|d| {
            !registry
                .defs()
                .iter()
                .any(|g| g.path == d.path && g.verb == Verb::Get)
        })
        .map(|d| d.path)
        .collect();
    // Fixed GET routes that answer the shell (the site root `/`): the
    // namespace-path parser still rejects them, the router never asks it.
    let fixed_shell: Vec<&str> = registry
        .defs()
        .iter()
        .filter(|d| {
            d.scope == crate::registry::RouteScope::Fixed
                && d.verb == Verb::Get
                && d.action == Action::WebShell
        })
        .map(|d| d.path)
        .collect();
    for (list, accepted) in [("accept", true), ("reject", false)] {
        for v in doc[list].as_array().expect(list) {
            let Some(uri) = uri_of(v) else {
                // Not representable as an origin-form request target (raw
                // bytes only); the parser's own tests cover it.
                assert_ne!(v["form"], "both", "{}", v["name"]);
                continue;
            };
            carried += 1;
            let answer = send(product(&script), request("GET", &uri.to_string(), None)).await;
            let name = &v["name"];
            let git = accepted && v["expect"]["kind"] == "git";
            // The vector's asset name is no build's file: FG-WEB-0001
            // (never the shell).
            let unbuilt = accepted && v["expect"]["route"] == "assets";
            // `raw` / `archive` are routed (M2 MR7): credentials first.
            let content = accepted
                && ["raw", "archive"].contains(&v["expect"]["word"].as_str().unwrap_or(""));
            if content {
                assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{name}");
            } else if git {
                // A routed git URL needs credentials (a GET on a POST
                // endpoint has no route).
                let route = v["expect"]["route"].as_str().unwrap_or("");
                if route == "info_refs_upload_pack" || route == "info_refs_receive_pack" {
                    assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{name}");
                } else {
                    assert_eq!(answer.status, StatusCode::NOT_FOUND, "{name}: {route}");
                }
            } else if v["reason"] == "service_unsupported" {
                assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{name}");
                assert_eq!(
                    answer.code(),
                    ErrorCode::SmartHttpServiceUnsupported.id(),
                    "{name}"
                );
            } else if fixed_shell.contains(&uri.path()) {
                assert_eq!(answer.status, StatusCode::OK, "{name}");
                assert_eq!(answer.bytes, shell_html(), "{name}");
            } else if fixed_paths.contains(&uri.path()) {
                // A fixed API route without a GET (`POST /api/v1/projects`,
                // M1 phase 8b MR7a) answers 405 before the dispatcher.
                assert_eq!(answer.status, StatusCode::METHOD_NOT_ALLOWED, "{name}");
            } else if accepted && !unbuilt {
                assert_eq!(answer.status, StatusCode::OK, "{name}");
                assert_eq!(answer.bytes, shell_html(), "{name}");
            } else {
                assert_eq!(answer.status, StatusCode::NOT_FOUND, "{name}");
                assert_eq!(answer.code(), ErrorCode::RouteNotFound.id(), "{name}");
            }
        }
    }
    assert!(carried > 100, "only {carried} vectors carried");
    assert_eq!(script.calls(), 0, "public and rejected paths never look up");
}

// ── the four ADR-0015 dispatcher tests (1 is in registry_tests.rs) ───────

/// ADR test 2: a path of the wrong kind is answered like an absent one.
#[tokio::test]
async fn a_path_of_the_wrong_kind_is_not_found_like_an_absent_one() {
    let script = Arc::new(script());
    let group = send(
        content_app(&script),
        request("GET", "/acme/-/raw/main/x", Some(reporter())),
    )
    .await;
    let user = send(
        content_app(&script),
        request("GET", "/alice/-/raw/main/x", Some(reporter())),
    )
    .await;
    let absent = send(
        content_app(&script),
        request("GET", "/acme/nothing/-/raw/main/x", Some(reporter())),
    )
    .await;
    assert_eq!(group.code(), ErrorCode::NotFound.id());
    assert_eq!(group.without_request_id(), absent.without_request_id());
    assert_eq!(user.without_request_id(), absent.without_request_id());
    assert_eq!(script.calls(), 3, "one lookup per request");
}

/// ADR test 3: `/api/…` and unknown `/-/…` never get the shell.
#[tokio::test]
async fn api_paths_and_unknown_words_never_get_the_shell() {
    let script = Arc::new(script());
    for path in [
        "/api/v1/nope",
        "/api",
        "/acme/x/-/pulls",
        "/acme/x/-/Blob/main",
        "/-/unknown",
        "/-",
        "/acme/x.git/objects/info/packs",
        "/acme/x.git/HEAD",
    ] {
        for method in ["GET", "HEAD", "POST"] {
            let answer = send(product(&script), request(method, path, None)).await;
            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method} {path}");
            if method != "HEAD" {
                // (a HEAD answer has no body)
                assert_eq!(
                    answer.code(),
                    ErrorCode::RouteNotFound.id(),
                    "{method} {path}"
                );
            }
        }
    }
    // receive-pack is routed since M1 phase 9 MR4: credentials first,
    // nothing looked up without them.
    for (method, path) in [
        ("GET", "/acme/x.git/info/refs?service=git-receive-pack"),
        ("POST", "/acme/x.git/git-receive-pack"),
        // raw / archive are routed since M2 MR7 (GET / HEAD).
        ("GET", "/acme/x/-/raw/main/f"),
        ("HEAD", "/acme/x/-/raw/main/f"),
        ("GET", "/acme/x/-/archive/main.zip"),
        ("HEAD", "/acme/x/-/archive/main.zip"),
    ] {
        let answer = send(product(&script), request(method, path, None)).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
    for path in ["/acme/x/-/raw/main/f", "/acme/x/-/archive/main.zip"] {
        let answer = send(product(&script), request("POST", path, None)).await;
        assert_eq!(answer.code(), ErrorCode::RouteNotFound.id(), "POST {path}");
    }
    // A page word with a verb it has no route for: not the shell either.
    let post = send(
        product(&script),
        request("POST", "/acme/x/-/tree/main", None),
    )
    .await;
    assert_eq!(post.code(), ErrorCode::RouteNotFound.id());
    assert_eq!(script.calls(), 0);
}

/// ADR test 4 (amended in MR4): a word with a public and a non-public
/// route is refused when the router is built (`FG-WEB-0004`), so no
/// request can reach a group's shell through a non-public word without a
/// proof on the group; a non-public word needs authentication before any
/// lookup.
#[tokio::test]
async fn a_word_mixing_public_and_non_public_routes_is_refused_at_build() {
    let raw = ScopedRoute::Word(RouteWord::Raw);
    let mixed = Registry::new()
        .scoped(
            RouteScope::Group,
            Verb::Get,
            raw,
            None,
            Action::WebShell,
            "group shell",
            crate::routes::shell::shell,
        )
        .scoped(
            RouteScope::Project,
            Verb::Get,
            raw,
            None,
            Action::RepoRead,
            "raw",
            content,
        );
    let script = Arc::new(script());
    let refused = build(mixed, state_with(&script), AppConfig::default())
        .expect_err("a mixed word must not build");
    assert_eq!(refused.code(), ErrorCode::RouteRegistryInvalid);
    assert!(
        refused
            .to_string()
            .contains("word_mixes_public_and_non_public"),
        "{refused}"
    );

    let non_public = Registry::new().scoped(
        RouteScope::Project,
        Verb::Get,
        raw,
        None,
        Action::RepoRead,
        "raw",
        content,
    );
    let anonymous = send(
        router(non_public, &script),
        request("GET", "/acme/-/raw/x", None),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.code(), ErrorCode::Unauthenticated.id());
    assert!(anonymous.headers.contains_key("www-authenticate"));
    assert_eq!(script.calls(), 0, "no lookup before authentication");
}

// ── public words ─────────────────────────────────────────────────────────

#[tokio::test]
async fn the_shell_is_the_same_for_every_page_and_needs_nothing() {
    let script = Arc::new(script());
    let mut answers = Vec::new();
    for path in [
        "/acme",
        "/acme/visible",
        "/does/not/exist",
        "/acme/hidden/-/blob/main/README.md",
        "/acme/-/settings",
        "/acme/gone/-/members",
        "/-/projects/42/settings",
    ] {
        // Credentials on a public word are not even read.
        let mut req = request("GET", path, None);
        req.headers_mut().insert(
            "authorization",
            "Bearer fgp_whatever".parse().expect("header"),
        );
        let answer = send(product(&script), req).await;
        assert_eq!(answer.status, StatusCode::OK, "{path}");
        assert_eq!(answer.bytes, shell_html(), "{path}");
        assert_eq!(
            answer.headers.get("content-type").expect("type"),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            answer
                .headers
                .get("x-content-type-options")
                .expect("nosniff"),
            "nosniff"
        );
        answers.push(answer);
    }
    let first = answers[0].headers.clone();
    for a in &answers {
        let strip = |h: &HeaderMap| {
            h.iter()
                .filter(|(k, _)| *k != REQUEST_ID_HEADER)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(strip(&a.headers), strip(&first));
    }
    assert_eq!(script.calls(), 0);
}

#[tokio::test]
async fn head_answers_the_shell_headers_without_a_body() {
    let script = Arc::new(script());
    let get = send(
        product(&script),
        request("GET", "/acme/x/-/tree/main", None),
    )
    .await;
    let head = send(
        product(&script),
        request("HEAD", "/acme/x/-/tree/main", None),
    )
    .await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.bytes.is_empty());
    assert_eq!(
        head.headers.get("content-type"),
        get.headers.get("content-type")
    );
}

// ── non-public words: authentication, one lookup, identical denials ─────

#[tokio::test]
async fn authentication_comes_before_any_lookup() {
    let script = Arc::new(script());
    for path in ["/acme/visible/-/raw/main/f", "/acme/nothing/-/raw/main/f"] {
        let anonymous = send(content_app(&script), request("GET", path, None)).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{path}");
        let disabled = Principal::authenticated(UserId::new(5), false, false, Credential::Session);
        let disabled = send(content_app(&script), request("GET", path, Some(disabled))).await;
        assert_eq!(disabled.code(), ErrorCode::AccountDisabled.id(), "{path}");
    }
    // The same 401 whether the path exists or not.
    let a = send(
        content_app(&script),
        request("GET", "/acme/visible/-/raw/f", None),
    )
    .await;
    let b = send(
        content_app(&script),
        request("GET", "/acme/nothing/-/raw/f", None),
    )
    .await;
    assert_eq!(a.without_request_id(), b.without_request_id());
    assert_eq!(script.calls(), 0);
}

/// MR4 must d + f: one resolver call per request whatever the outcome, and
/// absent, deleted tombstone, kind mismatch, invisible and an invisible
/// target behind a moved tombstone answer byte-identical 404s.
#[tokio::test]
async fn every_denial_is_identical_and_costs_one_lookup() {
    let script = Arc::new(script());
    let paths = [
        "/acme/nothing/-/raw/main/f",
        "/acme/gone/-/raw/main/f",
        "/acme/-/raw/main/f",
        "/alice/-/raw/main/f",
        "/acme/hidden/-/raw/main/f",
        "/acme/old-hidden/-/raw/main/f",
    ];
    let mut answers = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        let answer = send(content_app(&script), request("GET", path, Some(reporter()))).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(answer.code(), ErrorCode::NotFound.id(), "{path}");
        assert_eq!(script.calls(), i + 1, "exactly one lookup for {path}");
        answers.push(answer.without_request_id());
    }
    for (path, a) in paths.iter().zip(&answers) {
        assert_eq!(a, &answers[0], "{path} differs from an absent path");
    }
    // An allowed request costs the same single lookup.
    let ok = send(
        content_app(&script),
        request("GET", "/acme/visible/-/raw/main/f", Some(reporter())),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(script.calls(), paths.len() + 1);
}

#[tokio::test]
async fn the_handler_gets_the_proof_tail_and_canonical_path() {
    let script = Arc::new(script());
    let answer = send(
        content_app(&script),
        request(
            "GET",
            "/Acme/Visible/-/raw/main/docs/a%20b.md",
            Some(reporter()),
        ),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.json(),
        json!({
            "action": "RepoRead",
            "project": VISIBLE,
            "tail": ["main", "docs", "a b.md"],
            "canonical": "acme/visible",
        })
    );
    let head = send(
        content_app(&script),
        request("HEAD", "/acme/visible/-/raw/main/f", Some(reporter())),
    )
    .await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.bytes.is_empty());
}

#[tokio::test]
async fn a_token_without_the_scope_is_refused_by_authorize() {
    let script = Arc::new(script());
    let token = Principal::authenticated(
        UserId::new(5),
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
    );
    let visible = send(
        content_app(&script),
        request("GET", "/acme/visible/-/raw/f", Some(token.clone())),
    )
    .await;
    assert_eq!(visible.code(), ErrorCode::InsufficientScope.id());
    // An absent path stays a 404 for it (no scope oracle on existence).
    let absent = send(
        content_app(&script),
        request("GET", "/acme/nothing/-/raw/f", Some(token)),
    )
    .await;
    assert_eq!(absent.code(), ErrorCode::NotFound.id());
}

// ── moved tombstones: redirect only after authorize() ────────────────────

#[tokio::test]
async fn a_moved_path_redirects_only_after_authorize_allowed() {
    let script = Arc::new(script());
    let answer = send(
        content_app(&script),
        request("GET", "/acme/old/-/raw/main/a%20b?x=1", Some(reporter())),
    )
    .await;
    assert_eq!(answer.status, StatusCode::FOUND);
    assert_eq!(
        answer.headers.get("location").expect("location"),
        "/acme/visible/-/raw/main/a%20b",
        "built from validated segments; the query is dropped"
    );
    assert_eq!(
        answer.headers.get("cache-control").expect("no-store"),
        "no-store"
    );
    assert_eq!(script.calls(), 1);

    // Not visible behind the tombstone: the 404 of an absent path, and the
    // current path is never revealed.
    let hidden = send(
        content_app(&script),
        request("GET", "/acme/old-hidden/-/raw/main/f", Some(reporter())),
    )
    .await;
    let absent = send(
        content_app(&script),
        request("GET", "/acme/nothing/-/raw/main/f", Some(reporter())),
    )
    .await;
    assert_eq!(hidden.without_request_id(), absent.without_request_id());
    assert!(!String::from_utf8_lossy(&hidden.bytes).contains("hidden"));
}

#[tokio::test]
async fn a_moved_path_is_never_redirected_for_a_post() {
    let script = Arc::new(script());
    let post = send(
        content_app(&script),
        request("POST", "/acme/old/-/raw/main/f", Some(reporter())),
    )
    .await;
    let absent = send(
        content_app(&script),
        request("POST", "/acme/nothing/-/raw/main/f", Some(reporter())),
    )
    .await;
    assert_eq!(post.status, StatusCode::NOT_FOUND);
    assert!(post.headers.get("location").is_none());
    assert_eq!(post.without_request_id(), absent.without_request_id());
}

#[test]
fn the_location_encodes_every_tail_segment() {
    use forge_core::web_path::GitRoute;
    use forge_core::TailSegments;
    let tail = TailSegments::decode(&["main", "%E6%97%A5%20%3F%23", "--x"]).expect("tail");
    let current = ns("acme/new");
    assert_eq!(
        super::location(&current, ScopedRoute::Word(RouteWord::Blob), &tail),
        "/acme/new/-/blob/main/%E6%97%A5%20%3F%23/--x"
    );
    assert_eq!(
        super::location(
            &current,
            ScopedRoute::Git(GitRoute::InfoRefsUploadPack),
            &tail
        ),
        "/acme/new.git/info/refs?service=git-upload-pack"
    );
    assert_eq!(
        super::location(&current, ScopedRoute::Home, &tail),
        "/acme/new/main/%E6%97%A5%20%3F%23/--x"
    );
}

// ── the resolver's failures ──────────────────────────────────────────────

#[tokio::test]
async fn a_resolver_failure_is_its_own_code_and_a_hang_is_bounded() {
    let failing = Arc::new(Script {
        fail: Some(ErrorCode::DbUnavailable),
        ..script()
    });
    let answer = send(
        content_app(&failing),
        request("GET", "/acme/visible/-/raw/f", Some(reporter())),
    )
    .await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.code(), ErrorCode::DbUnavailable.id());

    let hanging = Arc::new(Script {
        hang: true,
        ..script()
    });
    let answer = send(
        content_app(&hanging),
        request("GET", "/acme/visible/-/raw/f", Some(reporter())),
    )
    .await;
    assert_eq!(answer.code(), ErrorCode::RequestTimeout.id());
    assert_eq!(hanging.calls(), 1);
}

// ── PathResolve (`GET /api/v1/namespaces/resolve`) ───────────────────────

fn resolve_uri(path: &str) -> String {
    format!("/api/v1/namespaces/resolve?path={path}")
}

#[tokio::test]
async fn path_resolve_answers_visible_paths_and_the_current_path_of_moved_ones() {
    let script = Arc::new(script());
    let live = send(
        product(&script),
        request("GET", &resolve_uri("Acme%2FVisible"), Some(reporter())),
    )
    .await;
    assert_eq!(live.status, StatusCode::OK);
    assert_eq!(
        live.json(),
        json!({ "namespace_id": 10, "kind": "project", "project_id": 1, "canonical_path": "acme/visible", "moved": false })
    );
    assert_eq!(
        live.headers.get("cache-control").expect("cc"),
        "private, no-store"
    );
    let moved = send(
        product(&script),
        request("GET", &resolve_uri("/acme/old"), Some(reporter())),
    )
    .await;
    assert_eq!(
        moved.json(),
        json!({ "namespace_id": 10, "kind": "project", "project_id": 1, "canonical_path": "acme/visible", "moved": true })
    );
    let group = send(
        product(&script),
        request("GET", &resolve_uri("acme"), Some(reporter())),
    )
    .await;
    assert_eq!(
        group.json(),
        json!({ "namespace_id": 30, "kind": "group", "group_id": 3, "canonical_path": "acme", "moved": false })
    );
    let user = send(
        product(&script),
        request("GET", &resolve_uri("alice"), Some(reporter())),
    )
    .await;
    assert_eq!(
        user.json(),
        json!({ "namespace_id": 40, "kind": "user", "user_id": 4, "canonical_path": "alice", "moved": false })
    );
    assert_eq!(script.calls(), 4);
}

#[tokio::test]
async fn path_resolve_denials_are_identical_and_cost_one_lookup() {
    let script = Arc::new(script());
    let mut answers = Vec::new();
    for (i, path) in [
        "acme/nothing",
        "acme/gone",
        "acme/hidden",
        "acme/old-hidden",
    ]
    .iter()
    .enumerate()
    {
        let answer = send(
            product(&script),
            request("GET", &resolve_uri(path), Some(reporter())),
        )
        .await;
        assert_eq!(answer.code(), ErrorCode::NotFound.id(), "{path}");
        assert_eq!(script.calls(), i + 1, "{path}");
        answers.push(answer.without_request_id());
    }
    assert!(answers.iter().all(|a| a == &answers[0]));

    // A malformed parameter is FG-WEB-0006 (phase 8b MR6): the same 400
    // for every malformed value, decided without a lookup.
    let before = script.calls();
    let mut malformed = Vec::new();
    for query in [
        "",
        "path=",
        "path=a%2",
        "path=a&path=b",
        "path=a//b",
        "path=%E6%97%A5",
    ] {
        let uri = format!("/api/v1/namespaces/resolve?{query}");
        let answer = send(product(&script), request("GET", &uri, Some(reporter()))).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(answer.code(), ErrorCode::RequestInvalid.id(), "{query}");
        malformed.push(answer.without_request_id());
    }
    assert!(malformed.iter().all(|a| a == &malformed[0]));
    assert_eq!(script.calls(), before);

    // Authentication first; a wrong-scope token gets the same 403 for
    // every path.
    let anonymous = send(
        product(&script),
        request("GET", &resolve_uri("acme/visible"), None),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    let token = Principal::authenticated(
        UserId::new(5),
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
    );
    let a = send(
        product(&script),
        request("GET", &resolve_uri("acme/visible"), Some(token.clone())),
    )
    .await;
    let b = send(
        product(&script),
        request("GET", &resolve_uri("acme/nothing"), Some(token)),
    )
    .await;
    assert_eq!(a.code(), ErrorCode::InsufficientScope.id());
    assert_eq!(a.without_request_id(), b.without_request_id());
}

// ── logs ─────────────────────────────────────────────────────────────────

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

impl Captured {
    fn install(&self) {
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(|| ThreadSink)
                .finish();
            // ignore: another test of this process may have installed it
            let _ = tracing::subscriber::set_global_default(subscriber);
        });
        SINK.with(|s| *s.borrow_mut() = Some(self.clone()));
    }

    fn events(&self) -> Vec<Value> {
        let text = String::from_utf8(self.0.lock().expect("log buffer").clone()).expect("utf-8");
        text.lines()
            .map(|l| serde_json::from_str(l).expect("json log line"))
            .collect()
    }

    fn contexts(&self) -> Vec<String> {
        self.events()
            .iter()
            .filter_map(|e| e.get("context").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }
}

#[tokio::test]
async fn denials_differ_only_in_the_logged_reason() {
    let captured = Captured::default();
    captured.install();
    let script = Arc::new(script());
    for path in [
        "/acme/nothing/-/raw/f",
        "/acme/gone/-/raw/f",
        "/acme/-/raw/f",
        "/acme/hidden/-/raw/f",
        "/acme/old-hidden/-/raw/f",
    ] {
        let _ = send(content_app(&script), request("GET", path, Some(reporter()))).await;
    }
    let contexts = captured.contexts();
    for want in [
        "reason=absent",
        "reason=tombstone_deleted",
        "reason=kind_mismatch",
        "reason=not_visible",
        "via=moved",
    ] {
        assert!(
            contexts.iter().any(|c| c.contains(want)),
            "{want} not logged: {contexts:?}"
        );
    }
    assert!(
        contexts.iter().all(|c| c.contains("route=raw")),
        "{contexts:?}"
    );
}

#[tokio::test]
async fn an_unknown_path_is_logged_safely() {
    let captured = Captured::default();
    captured.install();
    let script = Arc::new(script());
    let long = format!("/api/{}", "a".repeat(400));
    let _ = send(product(&script), request("GET", &long, None)).await;
    let contexts = captured.contexts();
    let ctx = contexts
        .iter()
        .find(|c| c.contains("path=/api/aaa"))
        .expect("logged");
    assert!(ctx.contains("reason=path_rejected"), "{ctx}");
    assert!(ctx.contains("…(+"), "cut to 256 characters: {ctx}");
    assert!(!ctx.contains(&"a".repeat(300)), "{ctx}");
}

#[tokio::test]
async fn a_redirect_is_logged_as_namespace_redirected() {
    let captured = Captured::default();
    captured.install();
    let script = Arc::new(script());
    let _ = send(
        content_app(&script),
        request("GET", "/acme/old/-/raw/f", Some(reporter())),
    )
    .await;
    let events = captured.events();
    let event = events
        .iter()
        .find(|e| e["message"] == "namespace.redirected")
        .expect("namespace.redirected");
    assert_eq!(event["level"], "WARN");
    assert_eq!(event["namespace_id"], 10);
    assert_eq!(event["tombstone_id"], 11);
    assert_eq!(event["route"], "raw");
    assert!(event.get("request_id").is_some());
}

/// MR4 must e, M1 phase 9 MR3 (replaces the phase 8 tripwire that failed
/// when forge-web first depended on forge-git): forge-web runs git only
/// through forge-git's Smart HTTP API (`Git::upload_pack`,
/// `Git::check_wants`, the stream types; M1 phase 9 MR4 adds the push
/// API: `receive_request`, `receive_report`, `Git::fast_forwards`,
/// `Git::new_commits`; M2 MR7 the browse API, `forge_git::browse`) and
/// only in `routes/git.rs`, `routes/git_push.rs`, `git_stream.rs`,
/// `routes/browse.rs` / `routes/content.rs` — never the generic command builder, whose
/// options would be the place a user value could become an option. The git
/// routes carry no `TailSegments` (the git shape has no tail; their handlers
/// never read `tail()`), and a tail segment may still start with `-`
/// (`--output=x` is a legal file or ref name): when a content route passes
/// one to git, it goes through `revision()` / `path()` only.
#[test]
fn forge_web_runs_git_only_through_smart_http_api() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut dirs = vec![src.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    // Built at run time so this file does not match itself.
    let forbidden = [
        [".comm", "and("].concat(),
        ["Git::", "command"].concat(),
        ["Git", "Command"].concat(),
        ["arg", "_owned"].concat(),
        ["config", "_override"].concat(),
    ];
    let git_users = [
        "routes/browse.rs",
        "routes/content.rs",
        "routes/git.rs",
        "routes/git_push.rs",
        "routes/history.rs",
        "git_stream.rs",
    ];
    let mut used_in = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&src)
            .expect("under src")
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(file).expect("read");
        for needle in &forbidden {
            assert!(
                !text.contains(needle.as_str()),
                "{rel} uses {needle}: run git only through forge-git's smart_http API"
            );
        }
        if text.contains(&["forge", "_git"].concat()) && !rel.ends_with("_tests.rs") {
            used_in.push(rel.clone());
            assert!(
                git_users.contains(&rel.as_str()),
                "{rel} uses forge-git: only {git_users:?} may (ADR-0015 ref とパスの境目)"
            );
        }
        if rel == "routes/git.rs" || rel == "routes/git_push.rs" {
            assert!(!text.contains(".tail()"), "git routes take no tail");
        }
    }
    used_in.sort();
    let mut expected: Vec<String> = git_users.iter().map(|s| (*s).to_owned()).collect();
    expected.sort();
    assert_eq!(
        used_in, expected,
        "the Smart HTTP files are the forge-git users"
    );
    assert_eq!(
        used_in,
        git_users
            .iter()
            .rev()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    for def in crate::app::registry().defs() {
        if let Some(crate::registry::ScopedRoute::Git(_)) = def.route {
            assert_eq!(def.scope, crate::registry::RouteScope::ProjectGit);
            assert!(!def.path.contains("{*rest}"), "{}", def.path);
        }
    }
    let tail = forge_core::TailSegments::decode(&["--output=x", "-n"]).expect("legal names");
    assert_eq!(tail.as_slice(), ["--output=x", "-n"]);
}
