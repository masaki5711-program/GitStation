use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use forge_auth::Action;
use forge_errors::{ErrorCode, Lang};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::app::{build, AppConfig, AppState};
use crate::context::REQUEST_ID_HEADER;
use crate::registry::{openapi_json, routes_json, Registry, Verb};

fn state() -> AppState {
    AppState { version: "test" }
}

async fn call(router: axum::Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, json)
}

fn req(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

fn app() -> axum::Router {
    crate::app(state(), AppConfig::default()).expect("app builds")
}

fn with(registry: Registry<AppState>, config: AppConfig) -> axum::Router {
    build(registry, state(), config).expect("router")
}

fn fast_timeout() -> AppConfig {
    AppConfig {
        request_timeout: Duration::from_millis(50),
        ..AppConfig::default()
    }
}

async fn ok() -> &'static str {
    "ok"
}

async fn never() -> &'static str {
    std::future::pending::<()>().await;
    "unreachable"
}

#[tokio::test]
async fn healthz_is_public_and_carries_a_request_id() {
    let (status, headers, body) = call(app(), req("GET", "/healthz")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({ "status": "ok" }),
        "no version in public"
    );
    let id = headers.get(&REQUEST_ID_HEADER).expect("request id header");
    assert_eq!(id.len(), 32);
}

#[tokio::test]
async fn unknown_route_is_route_not_found() {
    let (status, _, body) = call(app(), req("GET", "/api/v1/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error_code"], ErrorCode::RouteNotFound.id());
}

#[tokio::test]
async fn wrong_method_on_a_registered_path_is_method_not_allowed() {
    let res = app()
        .oneshot(req("POST", "/healthz"))
        .await
        .expect("infallible");
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    let allow = res
        .headers()
        .get("allow")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(body["error_code"], ErrorCode::MethodNotAllowed.id());
    assert!(allow.contains("GET"), "Allow header kept: {allow:?}");
}

#[tokio::test]
async fn error_body_contains_only_code_message_request_id() {
    let (_, headers, body) = call(app(), req("GET", "/does/not/exist")).await;
    let keys: Vec<&str> = body
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["error_code", "request_id", "user_message"]);
    let id = headers.get(&REQUEST_ID_HEADER).expect("header");
    assert_eq!(body["request_id"], id.to_str().expect("ascii"));
    assert!(
        !body.to_string().contains("/does/not/exist"),
        "no path in the body"
    );
}

#[tokio::test]
async fn request_id_is_always_generated_by_the_server() {
    // A client-supplied id is never adopted (it is only logged), so nobody
    // can make their requests look like someone else's in the logs.
    for supplied in ["abc-123_DEF", "<script>alert(1)</script>"] {
        let r = Request::get("/healthz")
            .header("x-request-id", supplied)
            .body(Body::empty())
            .expect("request");
        let (_, headers, _) = call(app(), r).await;
        let echoed = headers
            .get(&REQUEST_ID_HEADER)
            .expect("header")
            .to_str()
            .expect("ascii");
        assert_ne!(echoed, supplied);
        assert_eq!(echoed.len(), 32);
    }
}

#[tokio::test]
async fn accept_language_selects_the_message_language() {
    for (tag, expected) in [("id-ID,id;q=0.9", "id"), ("en-US", "en"), ("fr", "ja")] {
        let r = Request::get("/missing")
            .header("accept-language", tag)
            .body(Body::empty())
            .expect("request");
        let (_, headers, body) = call(app(), r).await;
        assert_eq!(
            headers.get("content-language").expect("lang"),
            expected,
            "{tag}"
        );
        let lang = Lang::parse(expected).expect("lang");
        assert_eq!(
            body["user_message"],
            ErrorCode::RouteNotFound.user_message(lang)
        );
    }
}

#[test]
fn language_ignores_unparseable_header_values() {
    use axum::http::HeaderValue;
    assert_eq!(crate::context::language(None), Lang::Ja);
    let garbage = HeaderValue::from_bytes(b"\xff\xfe").expect("opaque header");
    assert_eq!(crate::context::language(Some(&garbage)), Lang::Ja);
    let later = HeaderValue::from_static("fr, en");
    assert_eq!(crate::context::language(Some(&later)), Lang::En);
}

#[tokio::test]
async fn slow_handler_is_request_timeout() {
    let router = with(
        Registry::new().route(Verb::Get, "/slow", Action::Health, "slow", never),
        fast_timeout(),
    );
    let (status, _, body) = call(router, req("GET", "/slow")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error_code"], ErrorCode::RequestTimeout.id());
}

#[tokio::test]
async fn timeout_response_carries_request_id_and_language() {
    let router = with(
        Registry::new().route(Verb::Get, "/slow", Action::Health, "slow", never),
        fast_timeout(),
    );
    let r = Request::get("/slow")
        .header("accept-language", "en")
        .body(Body::empty())
        .expect("request");
    let (_, headers, body) = call(router, r).await;
    let id = headers
        .get(&REQUEST_ID_HEADER)
        .expect("header")
        .to_str()
        .expect("ascii");
    assert_eq!(body["request_id"], id);
    assert_eq!(headers.get("content-language").expect("lang"), "en");
    assert_eq!(
        body["user_message"],
        ErrorCode::RequestTimeout.user_message(Lang::En)
    );
}

#[allow(clippy::panic)]
async fn boom() -> &'static str {
    panic!("boom")
}

#[tokio::test]
async fn a_panicking_handler_becomes_an_internal_error_with_a_request_id() {
    let router = with(
        Registry::new().route(Verb::Get, "/boom", Action::Health, "boom", boom),
        AppConfig::default(),
    );
    let (status, headers, body) = call(router, req("GET", "/boom")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error_code"], ErrorCode::Internal.id());
    let id = headers
        .get(&REQUEST_ID_HEADER)
        .expect("id")
        .to_str()
        .expect("ascii");
    assert_eq!(body["request_id"], id);
}

#[tokio::test]
async fn non_public_route_without_principal_is_unauthenticated_and_handler_not_entered() {
    let entered = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&entered);
    let handler = move || {
        let flag = Arc::clone(&flag);
        async move {
            flag.store(true, Ordering::SeqCst);
            "secret"
        }
    };
    let router = with(
        Registry::new().route(Verb::Get, "/admin/users", Action::UserCreate, "x", handler),
        AppConfig::default(),
    );
    let (status, _, body) = call(router, req("GET", "/admin/users")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error_code"], ErrorCode::Unauthenticated.id());
    assert!(!entered.load(Ordering::SeqCst), "handler must not run");
}

async fn created() -> (StatusCode, &'static str) {
    (StatusCode::CREATED, "created")
}

#[tokio::test]
async fn two_verbs_on_one_path_both_route() {
    let router = with(
        Registry::new()
            .route(Verb::Get, "/x", Action::Health, "get", ok)
            .route(Verb::Post, "/x", Action::Health, "post", created),
        AppConfig::default(),
    );
    let (get, _, _) = call(router.clone(), req("GET", "/x")).await;
    let (post, _, _) = call(router, req("POST", "/x")).await;
    assert_eq!(get, StatusCode::OK);
    assert_eq!(post, StatusCode::CREATED);
}

#[test]
fn invalid_or_duplicate_routes_are_a_startup_error() {
    let cases: [Registry<AppState>; 4] = [
        Registry::new()
            .route(Verb::Get, "/x", Action::Health, "a", ok)
            .route(Verb::Get, "/x", Action::Health, "b", ok),
        Registry::new().route(Verb::Get, "no-slash", Action::Health, "a", ok),
        Registry::new().route(Verb::Get, "/a//b", Action::Health, "a", ok),
        Registry::new().route(Verb::Get, "", Action::Health, "a", ok),
    ];
    for registry in cases {
        let err = registry.into_router().expect_err("invalid registry");
        assert_eq!(err.code(), ErrorCode::RouteRegistryInvalid);
    }
}

#[test]
fn every_registered_route_has_matrix_rows() {
    let summary = forge_auth::matrix::summary();
    for def in crate::app::registry().defs() {
        let s = summary
            .iter()
            .find(|s| s.action == def.action.name())
            .unwrap_or_else(|| panic!("{} has no matrix rows", def.action.name()));
        assert_eq!(def.action.is_public(), s.public, "{}", def.action.name());
        assert!(
            s.allow > 0 && (s.public || s.deny > 0),
            "{} {} ({}) needs allowed and denied rows",
            def.verb.as_str(),
            def.path,
            def.action.name()
        );
    }
}

#[test]
fn every_action_is_routed_or_planned_but_not_both() {
    use std::collections::BTreeSet;
    let routed: BTreeSet<Action> = crate::app::registry()
        .defs()
        .iter()
        .map(|d| d.action)
        .collect();
    let planned: BTreeSet<Action> = crate::app::PLANNED.iter().copied().collect();
    assert!(
        routed.is_disjoint(&planned),
        "{:?}",
        routed.intersection(&planned).collect::<Vec<_>>()
    );
    for action in Action::ALL {
        assert!(
            routed.contains(action) || planned.contains(action),
            "{action:?} is neither routed nor planned"
        );
    }
}

#[test]
fn no_router_bypass_outside_the_registry() {
    // Routes are added only through Registry::route (ADR-0010). Patterns are
    // assembled at run time so this file does not match itself.
    let forbidden = [
        format!("{}{}", "Router::", "new("),
        format!("{}{}", ".route(", "\""),
        format!("{}{}", ".route_", "service("),
        format!("{}{}", ".ne", "st("),
        format!("{}{}", ".nest_", "service("),
    ];
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut offenders = Vec::new();
    let mut stack = vec![crates];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n != "target") {
                    stack.push(p);
                }
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if !name.ends_with(".rs") || name == "registry.rs" || name == "tests.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            for pat in &forbidden {
                if text.contains(pat.as_str()) {
                    offenders.push(format!("{}: {pat}", p.display()));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "router bypass: {offenders:?}");
}

#[test]
fn openapi_documents_params_security_and_multiple_verbs() {
    let registry: Registry<AppState> = Registry::new()
        .route(Verb::Get, "/x/{id}", Action::Health, "public get", ok)
        .route(Verb::Delete, "/x/{id}", Action::ProjectDelete, "delete", ok);
    let doc: Value = serde_json::from_str(&openapi_json(registry.defs(), "1")).expect("json");
    let path = &doc["paths"]["/x/{id}"];
    assert_eq!(path["get"]["operationId"], "Health");
    assert_eq!(path["delete"]["operationId"], "ProjectDelete");
    assert_eq!(path["get"]["security"], serde_json::json!([]));
    assert!(path["delete"].get("security").is_none(), "inherits bearer");
    assert_eq!(path["delete"]["parameters"][0]["name"], "id");
}

#[test]
fn routes_json_sorts_and_appends_planned() {
    let registry: Registry<AppState> = Registry::new()
        .route(Verb::Post, "/b", Action::Health, "b", ok)
        .route(Verb::Get, "/a", Action::Health, "a", ok);
    let list: Value =
        serde_json::from_str(&routes_json(registry.defs(), &[Action::ProjectRead])).expect("json");
    assert_eq!(list[0]["path"], "/a");
    assert_eq!(list[1]["path"], "/b");
    assert_eq!(
        list[2],
        serde_json::json!({ "action": "ProjectRead", "planned": true })
    );
}

#[test]
fn generated_documents_on_disk_match_the_code() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (rel, body) in crate::generated_documents(env!("CARGO_PKG_VERSION")) {
        let on_disk = std::fs::read_to_string(root.join(rel))
            .unwrap_or_else(|e| panic!("Git/{rel} missing ({e}) — run gen-authz"));
        assert!(
            on_disk == body,
            "Git/{rel} is stale — run `cargo run -p forge-web --bin gen-authz` in Git/"
        );
    }
}

#[tokio::test]
async fn bind_to_an_occupied_port_is_server_bind_failed() {
    let first = crate::server::bind("127.0.0.1:0")
        .await
        .expect("ephemeral port");
    let addr = first.local_addr().expect("addr").to_string();
    let err = crate::server::bind(&addr).await.expect_err("port in use");
    assert_eq!(err.code(), ErrorCode::ServerBindFailed);
    assert!(err.to_string().contains("bind="), "{err}");
    let invalid = crate::server::bind("not-an-address")
        .await
        .expect_err("invalid");
    assert_eq!(invalid.code(), ErrorCode::ServerBindFailed);
}

async fn http_get(addr: &str, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.expect("write");
    let mut out = String::new();
    s.read_to_string(&mut out).await.expect("read");
    out
}

#[tokio::test]
async fn serve_answers_healthz_and_stops_on_shutdown() {
    let listener = crate::server::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(crate::server::serve(
        listener,
        app(),
        crate::ServerLimits::default(),
        async {
            let _ = rx.await; // ignore: a dropped sender also means "stop"
        },
    ));
    let res = http_get(&addr, "/healthz").await;
    assert!(res.starts_with("HTTP/1.1 200"), "{res}");
    tx.send(()).expect("server running");
    let outcome = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("stops")
        .expect("task");
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[tokio::test]
async fn serve_failures_have_their_own_code() {
    // A request still running at shutdown, with a tiny grace period: the
    // forced stop is reported as FG-SYS-0005 (shutdown_deadline).
    let listener = crate::server::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let router = with(
        Registry::new().route(Verb::Get, "/hang", Action::Health, "hang", never),
        AppConfig {
            request_timeout: Duration::from_secs(60),
            ..AppConfig::default()
        },
    );
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let limits = crate::ServerLimits {
        shutdown_grace: Duration::from_millis(100),
        ..crate::ServerLimits::default()
    };
    let server = tokio::spawn(crate::server::serve(listener, router, limits, async {
        let _ = rx.await; // ignore: a dropped sender also means "stop"
    }));
    let client = tokio::spawn(async move { http_get(&addr, "/hang").await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    tx.send(()).expect("server running");
    let err = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("stops")
        .expect("task")
        .expect_err("deadline");
    assert_eq!(err.code(), ErrorCode::ServeFailed);
    assert!(
        err.to_string().contains("operation=shutdown_deadline"),
        "{err}"
    );
    client.abort();
}

#[tokio::test]
async fn slow_header_senders_are_disconnected() {
    // slowloris: a client that never finishes its headers is dropped after
    // the header-read timeout instead of holding a connection forever.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = crate::server::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let limits = crate::ServerLimits {
        header_read_timeout: Duration::from_millis(200),
        ..crate::ServerLimits::default()
    };
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(crate::server::serve(listener, app(), limits, async {
        let _ = rx.await; // ignore: a dropped sender also means "stop"
    }));
    let mut s = tokio::net::TcpStream::connect(&addr)
        .await
        .expect("connect");
    s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n")
        .await
        .expect("partial headers");
    let mut buf = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
    assert!(closed.is_ok(), "server must close the stalled connection");
    let _ = tx.send(()); // ignore: the server may already be stopping
    let _ = tokio::time::timeout(Duration::from_secs(10), server).await; // ignore: cleanup
}

#[test]
fn signal_handler_failure_is_logged_with_a_code() {
    let err = crate::server::signal_failure("SIGTERM", std::io::Error::other("denied"));
    assert_eq!(err.code(), ErrorCode::SignalHandlerUnavailable);
    assert!(err.to_string().contains("signal=SIGTERM"), "{err}");
}

#[test]
fn accept_failure_has_its_own_code() {
    // The accept loop logs FG-SYS-0007 and keeps serving; the code is warn
    // severity (the server does not stop).
    assert_eq!(
        ErrorCode::AcceptFailed.severity(),
        forge_errors::Severity::Warn
    );
    assert_eq!(ErrorCode::AcceptFailed.http_status(), 503);
}
