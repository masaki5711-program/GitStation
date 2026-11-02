//! Body routes end to end through the product middleware (M1 phase 8b
//! MR6): the body is read, bounded and typed after authentication and
//! before any lookup; the loader sees the parsed body; the handler gets it
//! with the admitted proof. Failures: `FG-WEB-0006` (shape) and
//! `FG-WEB-0009` (size), never with a body value in the log.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use forge_auth::{Credential, InstanceCtx, Principal, Scope};
use forge_core::UserId;
use forge_errors::{ErrorCode, ForgeError};
use http_body_util::BodyExt;
use serde::Deserialize;
use serde_json::Value;
use tower::ServiceExt;

use crate::app::{build, AppConfig, AppState};
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Lookup, Registry, Verb};

const PATH: &str = "/api/v1/test/things";
const CAP: usize = 64 * 1024;

/// A synthetic write body (strict).
#[derive(Debug, Deserialize, utoipa::ToSchema, PartialEq)]
#[serde(deny_unknown_fields)]
struct Change {
    name: String,
    level: u8,
}

/// Records what it was asked to load (the test's stand-in for the DB).
#[derive(Default)]
struct Counting {
    calls: AtomicUsize,
    seen: Mutex<Vec<String>>,
}

#[derive(Clone)]
struct Shared(Arc<Counting>);

impl BodyLoader<AppState, Change> for Shared {
    type Ctx = InstanceCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a axum::http::request::Parts,
        body: &'a Change,
        _: Option<UserId>,
    ) -> LoadFuture<'a, InstanceCtx> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut seen) = self.0.seen.lock() {
            seen.push(body.name.clone());
        }
        Box::pin(async {
            Ok(Lookup::Found {
                ctx: InstanceCtx,
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

async fn handler(_: AppState, request: Dispatched<InstanceCtx, Change>) -> Response {
    format!("{}:{}", request.input().name, request.input().level).into_response()
}

fn app(counting: &Arc<Counting>, config: AppConfig) -> axum::Router {
    let registry = Registry::new().route_with_body(
        Verb::Post,
        PATH,
        forge_auth::Action::UserSelfRead,
        "synthetic body route",
        CAP,
        Shared(Arc::clone(counting)),
        handler,
    );
    let state = AppState::new(
        "test",
        Vec::new(),
        crate::tests::no_tokens(),
        crate::tests::no_lookups(),
    );
    build(registry, state, config).expect("router")
}

fn member() -> Principal {
    Principal::authenticated(UserId::new(5), false, true, Credential::Session)
}

struct Answer {
    status: StatusCode,
    bytes: Vec<u8>,
}

impl Answer {
    fn code(&self) -> String {
        serde_json::from_slice::<Value>(&self.bytes).unwrap_or(Value::Null)["error_code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

async fn send(
    app: axum::Router,
    who: Option<Principal>,
    content_type: Option<&str>,
    body: Body,
    length: Option<usize>,
) -> Answer {
    let mut req = Request::builder().method("POST").uri(PATH);
    if let Some(ct) = content_type {
        req = req.header("content-type", ct);
    }
    if let Some(n) = length {
        req = req.header("content-length", n);
    }
    let mut req = req.body(body).expect("request");
    if let Some(p) = who {
        req.extensions_mut().insert(p);
    }
    let res = app.oneshot(req).await.expect("infallible");
    let status = res.status();
    let bytes = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    Answer { status, bytes }
}

async fn post(counting: &Arc<Counting>, content_type: &str, json: &str) -> Answer {
    send(
        app(counting, AppConfig::default()),
        Some(member()),
        Some(content_type),
        Body::from(json.to_owned()),
        Some(json.len()),
    )
    .await
}

#[tokio::test]
async fn a_valid_body_reaches_the_loader_and_the_handler() {
    let counting = Arc::new(Counting::default());
    for ct in [
        "application/json",
        "application/json; charset=utf-8",
        "Application/JSON;charset=\"UTF-8\"",
    ] {
        let answer = post(&counting, ct, r#"{"name":"alpha","level":3}"#).await;
        assert_eq!(answer.status, StatusCode::OK, "{ct}");
        assert_eq!(answer.bytes, b"alpha:3");
    }
    assert_eq!(counting.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        counting.seen.lock().map(|s| s.clone()).unwrap_or_default(),
        vec!["alpha"; 3]
    );
}

/// The `test` of FG-WEB-0006: every shape error is a 400 before any
/// lookup, and its log names fields and positions, never values.
#[tokio::test]
async fn request_body_validation_is_request_invalid() {
    let counting = Arc::new(Counting::default());
    let json = "application/json";
    let cases: &[(&str, &str, &str)] = &[
        (
            json,
            r#"{"name":"secret-value","level":3,"extra":1}"#,
            "unknown_field",
        ),
        (json, r#"{"name":"secret-value"}"#, "missing_field"),
        (
            json,
            r#"{"name":"secret-value","level":"secret-level"}"#,
            "type",
        ),
        (json, r#"{"name":"secret-value","level":300}"#, "type"),
        (json, r#"["secret-value"]"#, "type"),
        (json, r#"{"name":"secret-value","level":3"#, "json_syntax"),
        (json, "", "json_syntax"),
        (json, r#"{"name":"a","name":"b","level":1}"#, "json_syntax"),
        ("text/plain", r#"{"name":"a","level":1}"#, "content_type"),
        (
            "application/json; charset=latin1",
            r#"{"name":"a","level":1}"#,
            "content_type",
        ),
        (
            "application/jsonx",
            r#"{"name":"a","level":1}"#,
            "content_type",
        ),
    ];
    for (ct, body, _) in cases {
        let answer = post(&counting, ct, body).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(answer.code(), ErrorCode::RequestInvalid.id(), "{body}");
        let text = String::from_utf8_lossy(&answer.bytes);
        assert!(!text.contains("secret"), "{text}");
    }
    let missing = send(
        app(&counting, AppConfig::default()),
        Some(member()),
        None,
        Body::from(r#"{"name":"a","level":1}"#),
        None,
    )
    .await;
    assert_eq!(missing.code(), ErrorCode::RequestInvalid.id());
    assert_eq!(
        counting.calls.load(Ordering::SeqCst),
        0,
        "no lookup for a bad shape"
    );

    // The logged error: the reason and the field names, never a value.
    for (ct, body, reason) in cases {
        let parts = parts_with(ct);
        let err = super::read_json::<Change>(&parts, Body::from(body.to_owned()), CAP)
            .await
            .expect_err(body);
        let shown = format!("{err} {err:?}");
        assert!(shown.contains(&format!("reason={reason}")), "{shown}");
        assert!(!shown.contains("secret"), "{shown}");
    }
    let parts = parts_with(json);
    let err = super::read_json::<Change>(&parts, Body::from(r#"{"level":1,"zz9":2}"#), CAP)
        .await
        .expect_err("unknown");
    assert!(err.to_string().contains("field=zz9"), "{err}");
    let err = super::read_json::<Change>(&parts, Body::from(r#"{"level":1,"x y":2}"#), CAP)
        .await
        .expect_err("odd name");
    assert!(err.to_string().contains("field=<not_shown>"), "{err}");
    let err = super::read_json::<Change>(&parts, Body::from(r#"{"level":1}"#), CAP)
        .await
        .expect_err("missing");
    assert!(err.to_string().contains("field=name"), "{err}");
}

fn parts_with(content_type: &str) -> axum::http::request::Parts {
    Request::builder()
        .header("content-type", content_type)
        .body(())
        .expect("request")
        .into_parts()
        .0
}

/// The `test` of FG-WEB-0009: 65 KiB is refused, declared or streamed;
/// nothing is loaded; the server-wide limit caps every route.
#[tokio::test]
async fn oversized_request_body_is_request_too_large() {
    let counting = Arc::new(Counting::default());
    let filler = "a".repeat(65 * 1024);
    let big = format!(r#"{{"name":"{filler}","level":1}}"#);
    let declared = post(&counting, "application/json", &big).await;
    assert_eq!(declared.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(declared.code(), ErrorCode::RequestTooLarge.id());

    // No Content-Length: the body is cut as soon as it passes the cap.
    let chunks: Vec<Result<Bytes, std::io::Error>> = big
        .as_bytes()
        .chunks(4096)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    let streamed = send(
        app(&counting, AppConfig::default()),
        Some(member()),
        Some("application/json"),
        Body::from_stream(futures_util::stream::iter(chunks)),
        None,
    )
    .await;
    assert_eq!(streamed.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(streamed.code(), ErrorCode::RequestTooLarge.id());

    // The server's body_limit_bytes below the route's cap wins.
    let small = AppConfig {
        body_limit_bytes: 16,
        ..AppConfig::default()
    };
    let json = r#"{"name":"alpha","level":3}"#;
    let limited = send(
        app(&counting, small),
        Some(member()),
        Some("application/json"),
        Body::from(json),
        Some(json.len()),
    )
    .await;
    assert_eq!(limited.code(), ErrorCode::RequestTooLarge.id());
    assert_eq!(counting.calls.load(Ordering::SeqCst), 0);

    let parts = parts_with("application/json");
    let err = super::read_json::<Change>(&parts, Body::from(big), CAP)
        .await
        .expect_err("too large");
    assert!(err.to_string().contains("declared=false"), "{err}");
    assert!(err.to_string().contains("limit_bytes=65536"), "{err}");
}

/// Authentication comes first: no body is read and nothing loaded for an
/// anonymous, disabled or wrong-scope caller with a bad body.
#[tokio::test]
async fn authentication_comes_before_the_body() {
    let counting = Arc::new(Counting::default());
    let disabled = Principal::authenticated(UserId::new(5), false, false, Credential::Session);
    for (who, code) in [
        (None, ErrorCode::Unauthenticated),
        (Some(disabled), ErrorCode::AccountDisabled),
    ] {
        let answer = send(
            app(&counting, AppConfig::default()),
            who,
            Some("text/plain"),
            Body::from("not json"),
            None,
        )
        .await;
        assert_eq!(answer.code(), code.id());
    }
    // A wrong-scope token passes authentication; the body is checked, then
    // authorize() refuses after the (counted) load.
    let repo = Principal::authenticated(
        UserId::new(5),
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
    );
    let answer = send(
        app(&counting, AppConfig::default()),
        Some(repo),
        Some("application/json"),
        Body::from(r#"{"name":"alpha","level":3}"#),
        None,
    )
    .await;
    assert_eq!(answer.code(), ErrorCode::InsufficientScope.id());
    assert_eq!(counting.calls.load(Ordering::SeqCst), 1);
}

/// A body that fails mid-stream is a read failure, never a parse of what
/// arrived.
#[tokio::test]
async fn a_body_that_breaks_off_is_a_read_failure() {
    let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
        Ok(Bytes::from_static(b"{\"name\":")),
        Err(std::io::Error::other("connection reset")),
    ];
    let parts = parts_with("application/json");
    let err: ForgeError = super::read_json::<Change>(
        &parts,
        Body::from_stream(futures_util::stream::iter(chunks)),
        CAP,
    )
    .await
    .expect_err("broken");
    assert_eq!(err.code(), ErrorCode::RequestTimeout);
    assert!(err.to_string().contains("operation=request_read"), "{err}");
}

fn parts_with_total(content_type: &str, total: std::time::Duration) -> axum::http::request::Parts {
    let mut parts = parts_with(content_type);
    parts.extensions.insert(super::BodyReadTimeout(total));
    parts
}

/// A body that keeps trickling (never idle, never ending) is cut by the
/// total deadline: `FG-WEB-0002 operation=request_read kind=total`.
#[tokio::test]
async fn a_trickling_json_body_is_cut_by_the_total_deadline() {
    let parts = parts_with_total("application/json", std::time::Duration::from_millis(150));
    let trickle = futures_util::stream::unfold(0u8, |n| async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        Some((Ok::<_, std::io::Error>(Bytes::from_static(b" ")), n))
    });
    let started = std::time::Instant::now();
    let err = super::read_json::<Change>(&parts, Body::from_stream(trickle), CAP)
        .await
        .expect_err("cut");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(err.code(), ErrorCode::RequestTimeout);
    let text = err.to_string();
    assert!(text.contains("operation=request_read"), "{text}");
    assert!(text.contains("kind=total"), "{text}");
    assert!(text.contains("timeout_ms=150"), "{text}");
}

/// A body stream that fails (client abort, protocol error) is `kind=aborted`,
/// not `idle`; the idle timeout's own error stays `kind=idle`.
#[tokio::test]
async fn a_failed_body_stream_is_classified_aborted_or_idle() {
    let parts = parts_with("application/json");
    let aborted = futures_util::stream::iter(vec![
        Ok(Bytes::from_static(b"{\"name\":")),
        Err(std::io::Error::other("connection reset")),
    ]);
    let err = super::read_json::<Change>(&parts, Body::from_stream(aborted), CAP)
        .await
        .expect_err("aborted");
    assert_eq!(err.code(), ErrorCode::RequestTimeout);
    let text = err.to_string();
    assert!(text.contains("kind=aborted"), "{text}");
    assert!(text.contains("bytes_in=8"), "{text}");

    let idle = futures_util::stream::iter(vec![Err::<Bytes, _>(crate::server::ReadError::Idle)]);
    let err = super::read_json::<Change>(&parts, Body::from_stream(idle), CAP)
        .await
        .expect_err("idle");
    assert!(err.to_string().contains("kind=idle"), "{err}");
}

/// Only the media type is logged, never parameters or a longer header.
#[tokio::test]
async fn a_refused_content_type_logs_only_the_media_type() {
    let parts = parts_with("text/plain; boundary=secret-value");
    let err = super::read_json::<Change>(&parts, Body::empty(), CAP)
        .await
        .expect_err("refused");
    let text = err.to_string();
    assert!(text.contains("text/plain"), "{text}");
    assert!(!text.contains("secret-value"), "{text}");
}
