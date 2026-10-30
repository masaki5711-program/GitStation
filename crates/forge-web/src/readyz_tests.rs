//! `GET /readyz` with fake probes (no database needed).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_core::readiness::{ProbeFuture, ReadinessProbe};
use forge_errors::{ErrorCode, ForgeError};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::app::{AppConfig, AppState};
use crate::context::REQUEST_ID_HEADER;

enum Behaviour {
    Ok,
    Fail,
    Hang,
}

struct Fake {
    name: &'static str,
    behaviour: Behaviour,
}

impl ReadinessProbe for Fake {
    fn name(&self) -> &'static str {
        self.name
    }

    fn check(&self) -> ProbeFuture<'_> {
        Box::pin(async move {
            match self.behaviour {
                Behaviour::Ok => Ok(()),
                Behaviour::Fail => Err(ForgeError::new(ErrorCode::DbUnavailable)
                    .with_context("target", "db-secret-host.internal:5432/forge")
                    .with_context("reason", "connect_refused")),
                Behaviour::Hang => {
                    std::future::pending::<()>().await;
                    Ok(())
                }
            }
        })
    }
}

fn app_with(probes: Vec<Fake>, timeout: Duration) -> axum::Router {
    let probes: Vec<Arc<dyn ReadinessProbe>> = probes
        .into_iter()
        .map(|p| Arc::new(p) as Arc<dyn ReadinessProbe>)
        .collect();
    let mut state = AppState::new(
        "test",
        probes,
        crate::tests::no_tokens(),
        crate::tests::no_lookups(),
    );
    state.probe_timeout = timeout;
    crate::app(state, AppConfig::default()).expect("app builds")
}

async fn get_readyz(router: axum::Router) -> (StatusCode, String, Option<String>) {
    let req = Request::builder()
        .uri("/readyz")
        .body(Body::empty())
        .expect("request");
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let id = res
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned(), id)
}

#[tokio::test]
async fn readyz_is_200_when_all_probes_pass() {
    let router = app_with(
        vec![
            Fake {
                name: "database",
                behaviour: Behaviour::Ok,
            },
            Fake {
                name: "storage",
                behaviour: Behaviour::Ok,
            },
        ],
        Duration::from_secs(1),
    );
    let (status, body, _) = get_readyz(router).await;
    assert_eq!(status, StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json, serde_json::json!({ "status": "ok" }));
}

#[tokio::test]
async fn readyz_is_503_with_error_code_and_request_id_when_a_probe_fails() {
    let router = app_with(
        vec![Fake {
            name: "database",
            behaviour: Behaviour::Fail,
        }],
        Duration::from_secs(1),
    );
    let (status, body, header_id) = get_readyz(router).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["error_code"], ErrorCode::DbUnavailable.id());
    assert_eq!(json["request_id"].as_str(), header_id.as_deref());
}

#[tokio::test]
async fn readyz_body_contains_no_host_or_internal_detail() {
    let router = app_with(
        vec![Fake {
            name: "database",
            behaviour: Behaviour::Fail,
        }],
        Duration::from_secs(1),
    );
    let (_, body, _) = get_readyz(router).await;
    for leak in [
        "db-secret-host",
        "5432",
        "connect_refused",
        "database",
        "target",
    ] {
        assert!(!body.contains(leak), "{leak} leaked: {body}");
    }
}

#[tokio::test]
async fn readyz_probe_is_bounded_by_its_timeout() {
    let router = app_with(
        vec![Fake {
            name: "database",
            behaviour: Behaviour::Hang,
        }],
        Duration::from_millis(100),
    );
    let started = std::time::Instant::now();
    let (status, body, _) = get_readyz(router).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["error_code"], ErrorCode::ReadinessTimeout.id());
}

#[tokio::test]
async fn readyz_without_probes_is_ready_and_needs_no_credentials() {
    let router = app_with(Vec::new(), Duration::from_secs(1));
    let (status, _, _) = get_readyz(router).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn readyz_returns_the_first_failing_probe_without_waiting_for_later_ones() {
    let router = app_with(
        vec![
            Fake {
                name: "database",
                behaviour: Behaviour::Ok,
            },
            Fake {
                name: "storage",
                behaviour: Behaviour::Fail,
            },
            Fake {
                name: "llm",
                behaviour: Behaviour::Hang,
            },
        ],
        Duration::from_secs(5),
    );
    let started = std::time::Instant::now();
    let (status, body, _) = get_readyz(router).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let json: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["error_code"], ErrorCode::DbUnavailable.id());
}

/// A probe that counts how often it runs and takes a little time.
struct Counting {
    runs: Arc<std::sync::atomic::AtomicUsize>,
}

impl ReadinessProbe for Counting {
    fn name(&self) -> &'static str {
        "database"
    }

    fn check(&self) -> ProbeFuture<'_> {
        Box::pin(async move {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(())
        })
    }
}

#[tokio::test]
async fn concurrent_and_repeated_readyz_calls_share_one_probe_run() {
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe: Arc<dyn ReadinessProbe> = Arc::new(Counting {
        runs: Arc::clone(&runs),
    });
    let state = AppState::new(
        "test",
        vec![probe],
        crate::tests::no_tokens(),
        crate::tests::no_lookups(),
    );
    let router = crate::app(state, AppConfig::default()).expect("app");
    let calls = (0..20).map(|_| get_readyz(router.clone()));
    let results = futures_util::future::join_all(calls).await;
    assert!(results.iter().all(|(s, _, _)| *s == StatusCode::OK));
    assert_eq!(
        runs.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "20 concurrent requests within the cache window run the probe once"
    );
}
