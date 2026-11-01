//! Web sessions as their owner and the administration see them (M2 MR6,
//! ADR-0021 §17), all on `Resource::Session`.
//!
//! - `GET /api/v1/user/sessions` (`SessionList`): the caller's active
//!   sessions, with the one this request came with marked `current`.
//!   Sessions only (a token is `FG-AUTH-0002 reason=session_required`,
//!   decided before the lookup, so a token learns nothing of a session).
//! - `DELETE /api/v1/user/sessions/{public_id}` (`SessionRevoke`, sessions
//!   only): end one of them
//!   (`reason=logout`). The loader reads the session's owner; another
//!   user's session is the same `FG-REPO-0005` as an absent one —
//!   administrators included (they use the administration's route).
//! - `GET /api/v1/admin/users/{id}/sessions` (`AdminSessionList`): a user's active sessions
//!   (administrator rights; a token needs the `admin` scope).
//! - `DELETE /api/v1/admin/users/{id}/sessions` (`AdminSessionRevoke`): end every session of the
//!   user (`reason=admin`, one `session.revoked_all` audit row; never an
//!   agent). An unknown user is `FG-REPO-0005`.
//!
//! Nothing here shows a secret, a hash, the IdP's tokens or subject: the
//! public id, the times, whether it came through the IdP.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::SessionCtx;
use forge_core::{parse_positive_id, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::accounts::{is_session_public_id, SessionInfo};
use serde::Serialize;
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup};
use crate::routes::write::{absent, bounded, hosting, json, no_store, write_request};

/// `GET` the caller's sessions.
pub const OWN: &str = "/api/v1/user/sessions";
/// `DELETE` one of them.
pub const OWN_ONE: &str = "/api/v1/user/sessions/{public_id}";
const OWN_PREFIX: &str = "/api/v1/user/sessions/";
/// `GET` / `DELETE` a user's sessions (administrators).
pub const ADMIN: &str = "/api/v1/admin/users/{id}/sessions";
const ADMIN_PREFIX: &str = "/api/v1/admin/users/";
const ADMIN_SUFFIX: &str = "/sessions";

/// One active session.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionItem {
    /// The session's public id (16 hex; the `{public_id}` to end it).
    pub public_id: String,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub last_seen_at: String,
    pub idle_expires_at: String,
    pub absolute_expires_at: String,
    /// Signed in through the identity provider.
    pub oidc: bool,
    /// The session this request came with.
    pub current: bool,
}

/// A user's active sessions, newest first.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionItems {
    pub items: Vec<SessionItem>,
    /// More active sessions exist than are shown (at most 100).
    pub truncated: bool,
}

/// What ending a user's sessions did.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionsRevoked {
    #[schema(minimum = 1)]
    pub user_id: i64,
    /// How many sessions ended (0: there were none).
    pub revoked_sessions: i64,
}

fn found<R>(ctx: R) -> Lookup<R> {
    Lookup::Found {
        ctx,
        canonical: None,
        tombstone: None,
        project: None,
        group: None,
    }
}

fn me(viewer: Option<UserId>) -> Result<UserId, ForgeError> {
    viewer.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "session_route_without_user")
    })
}

/// The `{id}` of `/api/v1/admin/users/{id}/sessions`.
fn admin_target(parts: &Parts) -> Result<UserId, ForgeError> {
    parts
        .uri
        .path()
        .strip_prefix(ADMIN_PREFIX)
        .and_then(|rest| rest.strip_suffix(ADMIN_SUFFIX))
        .filter(|raw| !raw.contains('/'))
        .and_then(parse_positive_id)
        .map(UserId::new)
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))
}

/// `GET /api/v1/user/sessions`.
pub struct OwnSessions;

impl Loader<AppState> for OwnSessions {
    type Ctx = SessionCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, SessionCtx> {
        Box::pin(async move { Ok(found(SessionCtx::new(me(viewer)?, None))) })
    }
}

/// `DELETE /api/v1/user/sessions/{public_id}`: the session and its owner.
pub struct OwnSessionToRevoke;

impl Loader<AppState> for OwnSessionToRevoke {
    type Ctx = SessionCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, SessionCtx> {
        Box::pin(async move {
            let public_id = parts
                .uri
                .path()
                .strip_prefix(OWN_PREFIX)
                .filter(|raw| is_session_public_id(raw))
                .ok_or_else(|| param_invalid("public_id", "not_a_session_id"))?;
            let hosting = hosting(state)?;
            Ok(
                match bounded(
                    state,
                    "session_owner_load",
                    hosting.session_owner(public_id),
                )
                .await?
                {
                    Some((session, owner)) => found(SessionCtx::new(owner, Some(session))),
                    None => absent(),
                },
            )
        })
    }
}

/// `GET|DELETE /api/v1/admin/users/{id}/sessions`: the user's sessions.
pub struct UserSessions;

impl Loader<AppState> for UserSessions {
    type Ctx = SessionCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, SessionCtx> {
        Box::pin(async move { Ok(found(SessionCtx::new(admin_target(parts)?, None))) })
    }
}

fn item(s: &SessionInfo, current: Option<&str>) -> SessionItem {
    SessionItem {
        public_id: s.public_id.clone(), // clone: the row is borrowed
        created_at: s.created_at.clone(),
        last_seen_at: s.last_seen_at.clone(),
        idle_expires_at: s.idle_expires_at.clone(),
        absolute_expires_at: s.absolute_expires_at.clone(),
        oidc: s.oidc,
        current: current == Some(s.public_id.as_str()),
    }
}

fn respond(ctx: &crate::Ctx, status: StatusCode, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET /api/v1/user/sessions` and `GET /api/v1/admin/users/{id}/sessions`
/// → `200`.
pub async fn list(state: AppState, request: Dispatched<SessionCtx>) -> Response {
    // The caller's own session marks `current` (the administration's list
    // of the administrator's own sessions too).
    let current = request.session().map(|s| s.public_id.clone()); // clone: the request is consumed below
    let caller = request.principal().user();
    // The request (its body is not Sync) stays out of the await.
    let w = request.into_write();
    let built = async {
        let hosting = hosting(&state)?;
        let listed = bounded(&state, "session_list", hosting.sessions(&w.proof)).await?;
        let mine = caller == Some(w.proof.id().0);
        let current = if mine { current.as_deref() } else { None };
        json(SessionItems {
            items: listed.sessions.iter().map(|s| item(s, current)).collect(),
            truncated: listed.truncated,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `DELETE /api/v1/user/sessions/{public_id}` → `204` (also when it had
/// ended already).
pub async fn revoke(state: AppState, request: Dispatched<SessionCtx>) -> Response {
    let w = request.into_write();
    let done = async {
        hosting(&state)?
            .revoke_session(&w.proof, &write_request(&w))
            .await
            .map(|_| ())
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `DELETE /api/v1/admin/users/{id}/sessions` → `200` with the count.
pub async fn revoke_all(state: AppState, request: Dispatched<SessionCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let revoked = hosting(&state)?
            .revoke_all_sessions(&w.proof, &write_request(&w))
            .await?;
        json(SessionsRevoked {
            user_id: w.proof.id().0.get(),
            revoked_sessions: revoked,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}
