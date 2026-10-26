//! `GET /healthz` — liveness, and `GET /readyz` — readiness (both public).
//! Neither answer carries the version, host names or SQL: a failure is the
//! standard error body (code, user message, request id), the detail is in
//! the log.
//!
//! `/readyz` is unauthenticated, so it must not let callers drive load on
//! the database: probes are single-flight (concurrent requests wait for the
//! one running check) and the outcome is cached for [`CACHE_FOR`]. An
//! ongoing failure is logged when it starts or changes and again every
//! [`RELOG_EVERY`] while it lasts (alerts and log rotation must still see
//! it), not on every poll; recovery is logged too. Answers whose failure
//! is not logged again leave a debug breadcrumb with their request id.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::Json;
use forge_errors::{ErrorCode, ForgeError};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::app::AppState;
use crate::context::Ctx;
use crate::error::ApiError;

/// How long a readiness outcome is reused.
pub const CACHE_FOR: Duration = Duration::from_secs(1);

/// How often a failure that is still ongoing is logged again.
pub const RELOG_EVERY: Duration = Duration::from_secs(60);

/// The last readiness outcome: ready, or the failing code and probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ready,
    NotReady(ErrorCode, &'static str),
}

#[derive(Debug, Default)]
struct Last {
    outcome: Option<Outcome>,
    at: Option<Instant>,
    /// When the current failure was last logged.
    logged_at: Option<Instant>,
}

/// Shared by every `/readyz` request (single-flight + cache).
#[derive(Debug, Default)]
pub struct ReadinessCache {
    last: Mutex<Last>,
}

impl ReadinessCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

/// No version in the public answer (it would help pick known-vulnerable
/// releases); the version is in `forge --version` and the admin diagnostics.
pub async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

pub async fn readyz(State(state): State<AppState>, ctx: Ctx) -> Result<Json<Value>, ApiError> {
    // Holding the lock while probing makes concurrent requests share one
    // check; the request-timeout middleware bounds the wait.
    let mut last = state.readiness.last.lock().await;
    if let (Some(outcome), Some(at)) = (last.outcome, last.at) {
        if at.elapsed() < CACHE_FOR {
            return answer_cached(outcome, &ctx);
        }
    }
    let fresh = run_probes(&state).await;
    let outcome = match &fresh {
        Ok(()) => Outcome::Ready,
        Err((e, probe)) => Outcome::NotReady(e.code(), probe),
    };
    let now = Instant::now();
    let changed = last.outcome != Some(outcome);
    if changed && outcome == Outcome::Ready && last.outcome.is_some() {
        tracing::info!(target: "forge_web::readyz", "forge.readiness.recovered");
    }
    let relog = !changed
        && last
            .logged_at
            .is_none_or(|t| now.duration_since(t) >= RELOG_EVERY);
    let log_now = outcome != Outcome::Ready && (changed || relog);
    if log_now {
        last.logged_at = Some(now);
    }
    last.outcome = Some(outcome);
    last.at = Some(now);
    drop(last);
    match fresh {
        Ok(()) => Ok(Json(json!({ "status": "ok" }))),
        Err((e, _)) if log_now => Err(ctx.fail(e)),
        // The same failure, logged within the last RELOG_EVERY.
        Err((e, _)) => {
            breadcrumb(&ctx, e.code());
            Err(ctx.fail(e).already_logged())
        }
    }
}

fn answer_cached(outcome: Outcome, ctx: &Ctx) -> Result<Json<Value>, ApiError> {
    match outcome {
        Outcome::Ready => Ok(Json(json!({ "status": "ok" }))),
        Outcome::NotReady(code, probe) => {
            breadcrumb(ctx, code);
            Err(ctx
                .fail(ForgeError::new(code).with_context("probe", probe))
                .already_logged())
        }
    }
}

/// A request id handed to a user (問い合わせ番号) stays findable even when
/// its failure was not logged again.
fn breadcrumb(ctx: &Ctx, code: ErrorCode) {
    tracing::debug!(
        target: "forge_web::readyz",
        request_id = %ctx.request_id,
        error_code = code.id(),
        "readiness failure answered from the ongoing outage"
    );
}

/// Every probe in order, each bounded by `probe_timeout`; the first failure
/// with its own code (e.g. FG-DB-0001, 503) or FG-WEB-0005.
async fn run_probes(state: &AppState) -> Result<(), (ForgeError, &'static str)> {
    for probe in state.probes.iter() {
        let name = probe.name();
        match tokio::time::timeout(state.probe_timeout, probe.check()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err((e.with_context("probe", name), name)),
            Err(_) => {
                return Err((
                    ForgeError::new(ErrorCode::ReadinessTimeout)
                        .with_context("probe", name)
                        .with_context("timeout_ms", state.probe_timeout.as_millis()),
                    name,
                ))
            }
        }
    }
    Ok(())
}
