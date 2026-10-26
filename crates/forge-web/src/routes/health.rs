//! `GET /healthz` — liveness, and `GET /readyz` — readiness (both public).
//! Neither answer carries the version, host names or SQL: a failure is the
//! standard error body (code, user message, request id), the detail is in
//! the log.

use axum::extract::State;
use axum::Json;
use forge_errors::{ErrorCode, ForgeError};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::context::Ctx;
use crate::error::ApiError;

/// No version in the public answer (it would help pick known-vulnerable
/// releases); the version is in `forge --version` and the admin diagnostics.
pub async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Every probe in order, each bounded by `probe_timeout`; the first failure
/// answers with its own code (e.g. FG-DB-0001, 503) or FG-WEB-0005.
pub async fn readyz(State(state): State<AppState>, ctx: Ctx) -> Result<Json<Value>, ApiError> {
    for probe in state.probes.iter() {
        match tokio::time::timeout(state.probe_timeout, probe.check()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(ctx.fail(e.with_context("probe", probe.name()))),
            Err(_) => {
                return Err(ctx.fail(
                    ForgeError::new(ErrorCode::ReadinessTimeout)
                        .with_context("probe", probe.name())
                        .with_context("timeout_ms", state.probe_timeout.as_millis()),
                ))
            }
        }
    }
    Ok(Json(json!({ "status": "ok" })))
}
