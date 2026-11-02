#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

use std::time::Duration;

use axum::body::{Body, Bytes};
use forge_errors::ErrorCode;

use super::*;

fn ctx_value(err: &ForgeError, key: &str) -> Option<String> {
    err.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.clone())
}

/// MR3 review: a user over the per-user cap is refused at once; with every
/// slot taken the next caller waits `wait`, then FG-GIT-0004; dropping a
/// slot frees both counts.
#[tokio::test]
async fn body_reads_are_capped_globally_and_per_user() {
    let slots = BodySlots::new(2, 1, Duration::from_millis(50));
    let a = slots.acquire("user:1").await.expect("first");
    let err = slots.acquire("user:1").await.expect_err("per user");
    assert_eq!(err.code(), ErrorCode::GitBusy);
    assert_eq!(
        ctx_value(&err, "reason").as_deref(),
        Some("principal_body_reads")
    );
    let b = slots.acquire("user:2").await.expect("second");
    let err = slots.acquire("user:3").await.expect_err("global");
    assert_eq!(err.code(), ErrorCode::GitBusy);
    assert_eq!(ctx_value(&err, "reason").as_deref(), Some("body_reads"));
    assert_eq!(slots.held("user:3"), 0, "a refused wait returns its claim");
    drop(a);
    assert_eq!(slots.held("user:1"), 0);
    let again = slots.acquire("user:1").await.expect("freed");
    drop((again, b));
    assert_eq!(slots.held("user:2"), 0);
}

fn limits(body: Duration, idle: Duration) -> GitHttpLimits {
    GitHttpLimits {
        body_timeout: body,
        idle_timeout: idle,
        ..GitHttpLimits::default()
    }
}

/// A client that keeps sending a byte now and then (never idle) is cut at
/// `body_timeout`, not at the 1 h git timeout.
#[tokio::test]
async fn a_slow_body_ends_at_the_body_timeout() {
    let trickle = futures_util::stream::unfold(0u32, |n| async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        Some((Ok::<_, std::io::Error>(Bytes::from_static(b"0")), n + 1))
    });
    let err = read_request_body(
        Body::from_stream(trickle),
        false,
        &limits(Duration::from_millis(200), Duration::from_secs(60)),
    )
    .await
    .expect_err("too slow");
    assert_eq!(err.code(), ErrorCode::RequestTimeout);
    assert_eq!(
        ctx_value(&err, "operation").as_deref(),
        Some("git_request_body")
    );
    assert_eq!(ctx_value(&err, "kind").as_deref(), Some("total"));
    assert_eq!(ctx_value(&err, "timeout_ms").as_deref(), Some("200"));
}

#[tokio::test]
async fn a_stalled_body_ends_at_the_idle_timeout() {
    let stalled =
        futures_util::stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"0")) })
            .chain(futures_util::stream::pending());
    let err = read_request_body(
        Body::from_stream(stalled),
        false,
        &limits(Duration::from_secs(60), Duration::from_millis(50)),
    )
    .await
    .expect_err("stalled");
    assert_eq!(ctx_value(&err, "kind").as_deref(), Some("idle"));
    assert_eq!(ctx_value(&err, "bytes_in").as_deref(), Some("1"));
}
