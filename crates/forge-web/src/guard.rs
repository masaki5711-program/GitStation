//! The authentication guard every non-public route gets from the registry.
//!
//! The authentication middleware (M1 phase 7/8: session or personal access
//! token) inserts the caller's `forge_auth::Principal` into the request
//! extensions. Before the handler runs, a non-public route answers
//! `FG-AUTH-0001` without an authenticated principal and `FG-AUTH-0004` for a
//! disabled account — the same preamble as `forge_auth::authorize()`.
//!
//! This is authentication only: the per-resource decision is still the
//! handler's `forge_auth::authorize(.., def.action, ..)` call.

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
    let code = match req.extensions().get::<Principal>() {
        Some(p) if p.user().is_some() && p.active() => return next.run(req).await,
        Some(p) if p.user().is_some() => ErrorCode::AccountDisabled,
        _ => ErrorCode::Unauthenticated,
    };
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let lang = context::language(req.headers().get(ACCEPT_LANGUAGE));
    ApiError::new(
        ForgeError::new(code).with_context("action", action.name()),
        request_id,
        lang,
    )
    .into_response()
}
