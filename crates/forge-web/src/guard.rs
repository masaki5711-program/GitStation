//! The authentication guard every non-public route gets from the registry.
//!
//! The authentication middleware (M1 phase 7/8: session or personal access
//! token) inserts the caller's `forge_auth::Principal` into the request
//! extensions. Without an authenticated principal a non-public route answers
//! `FG-AUTH-0001` before its handler runs — so a route whose handler forgets
//! to authorize still fails closed.

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
    let authenticated = req
        .extensions()
        .get::<Principal>()
        .is_some_and(|p| p.user().is_some());
    if authenticated {
        return next.run(req).await;
    }
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let lang = context::language(req.headers().get(ACCEPT_LANGUAGE));
    ApiError::new(
        ForgeError::new(ErrorCode::Unauthenticated).with_context("action", action.name()),
        request_id,
        lang,
    )
    .into_response()
}
