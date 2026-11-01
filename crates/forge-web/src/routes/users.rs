//! `GET /api/v1/user` (`UserSelfRead`, M1 phase 8b MR6): the caller's own
//! account — what a client (`gstn auth status`, an agent) needs to know who
//! its credential stands for.
//!
//! Decided on the instance (`InstanceCtx`): every active signed-in user may
//! read their own account, a token needs `read_api` (or `api`). The data is
//! the principal's and what the authenticator read with the token in the
//! same statement (no second lookup): `id`, `username`, `display_name`,
//! `is_admin` (the account flag; rights also need a session or the token's
//! `admin` scope), `is_agent` (the token was issued for an AI agent) and
//! `token_scopes` (`null` for a session). Never cached by a shared cache.

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
pub const PATH: &str = "/api/v1/user";

/// The caller's account as the API shows it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserSelf {
    #[schema(minimum = 1)]
    pub id: i64,
    pub username: String,
    pub display_name: String,
    /// The account's administrator flag.
    pub is_admin: bool,
    /// The credential is a token issued for an AI agent.
    pub is_agent: bool,
    /// The token's scopes; `null` for a session.
    #[schema(required = true)]
    pub token_scopes: Option<Vec<String>>,
}

/// `GET|HEAD /api/v1/user`.
pub async fn me(_state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let built = match (request.principal().user(), request.caller()) {
        // clone: the profile stays in the request; the body owns its copy.
        (Some(id), Some(caller)) => Ok(UserSelf {
            id: id.get(),
            username: caller.username.clone(),
            display_name: caller.display_name.clone(),
            is_admin: caller.is_admin,
            is_agent: caller.is_agent,
            token_scopes: caller
                .token_scopes
                .as_ref()
                .map(|s| s.iter().map(|s| s.as_str().to_owned()).collect()),
        }),
        // `prove(UserSelfRead)` allowed, so there is a user; a principal
        // without the authenticator's profile (a session, M2) is not served
        // yet: fail loudly rather than answer with half an account.
        (user, _) => Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "caller_profile_missing")
            .with_context("user", user.map_or(0, |u| u.get()))),
    };
    let body = match built {
        Ok(body) => body,
        Err(e) => return request.ctx().fail(e).into_response(),
    };
    let mut res = Json(body).into_response();
    let headers = res.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(VARY, HeaderValue::from_static("Authorization, Cookie"));
    res
}
