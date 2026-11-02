//! `GET /api/v1/session` (`SessionRead`, M2 MR3, ADR-0021 §6, §17): the
//! caller's own web session — who it stands for, when it ends, and its CSRF
//! token, which the page sends as `X-GitStation-CSRF` on every
//! state-changing request.
//!
//! Sessions only: `authorize()` refuses a token (`FG-AUTH-0002
//! reason=session_required`). Decided on the instance (`InstanceCtx`): the
//! data is the request's own session and what the session authenticator
//! read with it (no second lookup). Never cached (`no-store`).

use axum::http::header::{CACHE_CONTROL, VARY};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::InstanceCtx;
use forge_errors::{ErrorCode, ForgeError};
use serde::Serialize;

use crate::app::AppState;
use crate::registry::Dispatched;

/// The route's path.
pub const PATH: &str = "/api/v1/session";

/// The signed-in user, as the session shows it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUser {
    #[schema(minimum = 1)]
    pub id: i64,
    pub username: String,
    pub display_name: String,
    /// The account's administrator flag (a session carries administrator
    /// rights when it is set).
    pub is_admin: bool,
}

/// The caller's web session.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionBody {
    pub user: SessionUser,
    /// Send as `X-GitStation-CSRF` on every state-changing request (64
    /// lowercase hex; derived from the cookie, the same for the session's
    /// lifetime).
    pub csrf_token: String,
    /// The session's public id (never the secret).
    pub public_id: String,
    /// When the session ends without activity (RFC 3339, UTC).
    pub idle_expires_at: String,
    /// When the session ends at the latest (RFC 3339, UTC).
    pub absolute_expires_at: String,
}

/// `GET|HEAD /api/v1/session`.
pub async fn read(_state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let built = match (
        request.principal().user(),
        request.caller(),
        request.session(),
    ) {
        // clone: the profile and session stay in the request; the body owns its copy.
        (Some(id), Some(caller), Some(session)) => Ok(SessionBody {
            user: SessionUser {
                id: id.get(),
                username: caller.username.clone(),
                display_name: caller.display_name.clone(),
                is_admin: caller.is_admin,
            },
            csrf_token: session.csrf_token.clone(),
            public_id: session.public_id.clone(),
            idle_expires_at: session.idle_expires_at.clone(),
            absolute_expires_at: session.absolute_expires_at.clone(),
        }),
        // `prove(SessionRead)` allowed, so this is an active session
        // principal; without the session's extension it would be served half
        // an answer: fail loudly instead.
        (user, _, _) => Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "session_credential_missing")
            .with_context("user", user.map_or(0, |u| u.get()))),
    };
    let body = match built {
        Ok(body) => body,
        Err(e) => return request.ctx().fail(e).into_response(),
    };
    let mut res = Json(body).into_response();
    let headers = res.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(VARY, HeaderValue::from_static("Cookie"));
    res
}
