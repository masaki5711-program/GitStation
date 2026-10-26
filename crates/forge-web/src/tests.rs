use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::Action;
use forge_errors::ErrorCode;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::app::{build, AppConfig, AppState};
use crate::context::REQUEST_ID_HEADER;
use crate::registry::{Registry, Verb};

fn state() -> AppState {
    AppState { version: "test" }
}

async fn call(
    router: axum::Router,
    req: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, json)
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).expect("request")
}

fn app() -> axum::Router {
    crate::app(state(), AppConfig::default()).expect("app builds")
}

#[tokio::test]
async fn healthz_is_public_and_carries_a_request_id() {
    let (status, headers, body) = call(app(), get("/healthz")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    let id = headers.get(&REQUEST_ID_HEADER).expect("request id header");
    assert_eq!(id.len(), 32);
}

#[tokio::test]
async fn unknown_route_is_route_not_found() {
    let (status, _, body) = call(app(), get("/api/v1/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error_code"], ErrorCode::RouteNotFound.id());
}

#[tokio::test]
async fn error_body_contains_only_code_message_request_id() {
    let (_, headers, body) = call(app(), get("/does/not/exist")).await;
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
async fn request_id_is_echoed_and_replaced_when_invalid() {
    let valid = Request::get("/healthz")
        .header("x-request-id", "abc-123_DEF")
        .body(Body::empty())
        .expect("request");
    let (_, headers, _) = call(app(), valid).await;
    assert_eq!(
        headers.get(&REQUEST_ID_HEADER).expect("header"),
        "abc-123_DEF"
    );

    let invalid = Request::get("/healthz")
        .header("x-request-id", "<script>alert(1)</script>")
        .body(Body::empty())
        .expect("request");
    let (_, headers, _) = call(app(), invalid).await;
    let echoed = headers
        .get(&REQUEST_ID_HEADER)
        .expect("header")
        .to_str()
        .expect("ascii");
    assert_ne!(echoed, "<script>alert(1)</script>");
    assert_eq!(echoed.len(), 32, "replaced by a generated id");
}

#[tokio::test]
async fn accept_language_selects_the_message_language() {
    for (tag, expected) in [("id-ID,id;q=0.9", "id"), ("en-US", "en"), ("fr", "ja")] {
        let req = Request::get("/missing")
            .header("accept-language", tag)
            .body(Body::empty())
            .expect("request");
        let (_, headers, body) = call(app(), req).await;
        assert_eq!(
            headers.get("content-language").expect("lang"),
            expected,
            "{tag}"
        );
        let lang = forge_errors::Lang::parse(expected).expect("lang");
        assert_eq!(
            body["user_message"],
            ErrorCode::RouteNotFound.user_message(lang)
        );
    }
}

async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_secs(5)).await;
    "late"
}

#[tokio::test]
async fn slow_handler_is_request_timeout() {
    let registry = Registry::new().route(Verb::Get, "/slow", Action::Health, "slow", slow);
    let router = build(
        registry,
        state(),
        AppConfig {
            request_timeout: Duration::from_millis(50),
        },
    )
    .expect("router");
    let (status, _, body) = call(router, get("/slow")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error_code"], ErrorCode::RequestTimeout.id());
}

#[test]
fn duplicate_route_is_a_startup_error_not_a_panic() {
    let registry: Registry<AppState> = Registry::new()
        .route(Verb::Get, "/x", Action::Health, "a", slow)
        .route(Verb::Get, "/x", Action::Health, "b", slow);
    let err = registry.into_router().expect_err("duplicate");
    assert_eq!(err.code(), ErrorCode::Internal);
}

#[test]
fn every_registered_route_has_matrix_rows() {
    let summary = forge_auth::matrix::summary();
    for def in crate::app::registry().defs() {
        let s = summary
            .iter()
            .find(|s| s.action == def.action.name())
            .unwrap_or_else(|| panic!("{} has no matrix rows", def.action.name()));
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
fn generated_documents_on_disk_match_the_code() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (rel, body) in crate::generated_documents(env!("CARGO_PKG_VERSION")) {
        let on_disk = std::fs::read_to_string(root.join(rel)).unwrap_or_default();
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
}

#[test]
fn every_api_action_is_routed_or_planned_but_not_both() {
    let routed: std::collections::BTreeSet<Action> = crate::app::registry()
        .defs()
        .iter()
        .map(|d| d.action)
        .collect();
    let planned: std::collections::BTreeSet<Action> = crate::app::PLANNED.iter().copied().collect();
    assert!(
        routed.is_disjoint(&planned),
        "routed and planned: {:?}",
        routed.intersection(&planned).collect::<Vec<_>>()
    );
    for &action in Action::ALL.iter().filter(|a| !a.is_git_operation()) {
        assert!(
            routed.contains(&action) || planned.contains(&action),
            "{action:?} is neither routed nor planned"
        );
    }
}
