//! The authentication guard every non-public route gets from the registry.
//!
//! The authentication middleware (M1 phase 7/8: session or personal access
//! token) inserts the caller's `forge_auth::Principal` into the request
//! extensions. Before the handler runs, a non-public route answers
//! `FG-AUTH-0001` without an authenticated principal and `FG-AUTH-0004` for a
//! disabled account — the same preamble as `forge_auth::authorize()`.
//!
//! This is authentication only: the per-resource decision is the
//! registry's `forge_auth::prove(principal, def.action, ctx)` on what the
//! route's loader (or the dispatcher's resolution) found; the handler only
//! receives the admitted proof and never calls `authorize()` itself.

use axum::extract::{Request, State};
use axum::http::header::ACCEPT_LANGUAGE;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use forge_auth::{Action, Principal};
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};

use crate::context;
use crate::error::ApiError;

pub(crate) async fn require_principal(
    State(action): State<Action>,
    req: Request,
    next: Next,
) -> Response {
    let err = match active_principal(&req, action) {
        Ok(_) => return next.run(req).await,
        Err(err) => err,
    };
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let lang = context::language(req.headers().get(ACCEPT_LANGUAGE));
    ApiError::new(err, request_id, lang).into_response()
}

/// The request's authenticated, active principal, or `FG-AUTH-0001` /
/// `FG-AUTH-0004` (the dispatcher's step 4 uses the same check).
pub(crate) fn active_principal(req: &Request, action: Action) -> Result<Principal, ForgeError> {
    let code = match req.extensions().get::<Principal>() {
        Some(p) if p.user().is_some() && p.active() => return Ok(p.clone()),
        Some(p) if p.user().is_some() => ErrorCode::AccountDisabled,
        _ => ErrorCode::Unauthenticated,
    };
    Err(ForgeError::new(code).with_context("action", action.name()))
}
