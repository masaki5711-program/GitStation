//! What the group and project write routes share (M1 phase 8b MR7a): the
//! hosting commands from the state, bounded loader reads, `{id}` between a
//! prefix and a suffix, the change's request and the uncached JSON answer.

use std::future::Future;

use axum::http::header::{CACHE_CONTROL, VARY};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::{ResourceCtx, UnresolvedCtx, UnresolvedReason};
use forge_core::parse_positive_id;
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::{Hosting, WriteRequest};
use serde::Serialize;
use serde_json::Value;

use crate::app::AppState;
use crate::registry::{Lookup, WriteParts};

/// The hosting commands; `FG-SYS-0001 reason=hosting_unconfigured` when the
/// server was built without them (`forge serve` always sets them).
pub(crate) fn hosting(state: &AppState) -> Result<&Hosting, ForgeError> {
    state.hosting.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "hosting_unconfigured")
    })
}

/// One read, bounded on the client side (forge-db bounds its statement
/// too): `FG-WEB-0002 operation=<operation>` when it does not answer.
pub(crate) async fn bounded<T>(
    state: &AppState,
    operation: &'static str,
    read: impl Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    let limit = state.lookup_timeout;
    match tokio::time::timeout(limit, read).await {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", operation)
            .with_context("timeout_ms", limit.as_millis())),
    }
}

/// The positive id between `prefix` and `suffix` of the raw path (never
/// decoded; the permalink's rule, [`parse_positive_id`]).
pub(crate) fn id_in(raw_path: &str, prefix: &str, suffix: &str) -> Option<i64> {
    let rest = raw_path.strip_prefix(prefix)?;
    let raw = rest.strip_suffix(suffix)?;
    if raw.contains('/') {
        return None;
    }
    parse_positive_id(raw)
}

/// Nothing the route can act on: decided by `authorize()` (`FG-REPO-0005`).
pub(crate) fn absent<R>() -> Lookup<R> {
    Lookup::Unresolved {
        ctx: UnresolvedCtx::new(UnresolvedReason::Absent),
        why: None,
    }
}

/// The request a write belongs to: its id and the token it came with.
pub(crate) fn write_request<R: ResourceCtx, B>(write: &WriteParts<R, B>) -> WriteRequest {
    WriteRequest {
        request_id: write.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: write.credential_id,
    }
}

/// A response body as JSON (`FG-SYS-0001` if it cannot be serialized).
pub(crate) fn json<T: Serialize>(body: T) -> Result<Value, ForgeError> {
    serde_json::to_value(body).map_err(|e| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "response_serialize")
            .with_source(e)
    })
}

/// `status` with the JSON body, never cached by a shared cache (the body
/// depends on who asks).
pub(crate) fn no_store(status: StatusCode, body: Value) -> Response {
    let mut res = (status, Json(body)).into_response();
    let headers = res.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(VARY, HeaderValue::from_static("Authorization, Cookie"));
    res
}
