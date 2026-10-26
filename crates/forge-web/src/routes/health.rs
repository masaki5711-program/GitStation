//! `GET /healthz` — liveness (public). Readiness (`/readyz`: DB, storage)
//! arrives with the database (M1 phase 2).

use axum::Json;
use serde_json::{json, Value};

/// No version in the public answer (it would help pick known-vulnerable
/// releases); the version is in `forge --version` and the admin diagnostics.
pub async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}
