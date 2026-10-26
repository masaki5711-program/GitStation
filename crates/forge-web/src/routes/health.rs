//! `GET /healthz` — liveness (public). Readiness (`/readyz`: DB, storage)
//! arrives with the database (M1 phase 2).

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::app::AppState;

pub async fn healthz(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "status": "ok", "version": state.version }))
}
