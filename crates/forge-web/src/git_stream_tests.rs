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

/// MR3 review: with a zero per-user wait a user over the per-user cap is
/// refused at once; with every slot taken the next caller waits `wait`,
/// then FG-GIT-0004; dropping a slot frees both counts.
#[tokio::test]
async fn body_reads_are_capped_globally_and_per_user() {
    let slots = BodySlots::new(2, 1, Duration::from_millis(50), Duration::ZERO, 4);
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

/// Load test 2026-11-17 F4: a user's next push waits for one of its body
/// slots instead of being refused at once.
#[tokio::test(start_paused = true)]
async fn a_body_read_over_the_per_user_cap_waits_for_the_users_next_slot() {
    let slots = BodySlots::new(4, 1, Duration::from_secs(1), Duration::from_secs(10), 4);
    let first = slots.acquire("user:1").await.expect("first");
    let waiter = {
        let slots = slots.clone(); // clone: the task's handle
        tokio::spawn(async move { slots.acquire("user:1").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiter.is_finished(), "waits while the slot is held");
    drop(first);
    let second = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("served")
        .expect("join")
        .expect("the freed slot");
    assert_eq!(slots.held("user:1"), 1);
    drop(second);
}

/// The per-user wait is bounded and the refusal carries the retry hint
/// (`Retry-After` on the 503).
#[tokio::test(start_paused = true)]
async fn a_per_user_body_wait_that_runs_out_is_git_busy_with_a_retry_hint() {
    let slots = BodySlots::new(4, 1, Duration::from_secs(1), Duration::from_millis(100), 4);
    let _first = slots.acquire("user:1").await.expect("first");
    // tokio's paused clock: no machine-speed slack.
    let started = tokio::time::Instant::now();
    let err = slots.acquire("user:1").await.expect_err("busy");
    // tokio's timer rounds up to the next millisecond.
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(100) && waited <= Duration::from_millis(105),
        "it waited: {waited:?}"
    );
    assert_eq!(err.code(), ErrorCode::GitBusy);
    assert_eq!(
        ctx_value(&err, "reason").as_deref(),
        Some("principal_body_reads")
    );
    assert_eq!(ctx_value(&err, "queue").as_deref(), Some("timeout"));
    assert_eq!(
        ctx_value(&err, forge_git::RETRY_AFTER_CONTEXT).as_deref(),
        Some("1")
    );
    // The 503 answers it as `Retry-After` only under the same word.
    assert_eq!(
        crate::error::RETRY_AFTER_CONTEXT,
        forge_git::RETRY_AFTER_CONTEXT
    );
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
