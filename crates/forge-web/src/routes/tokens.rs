//! The signed-in user's personal access tokens (M2 MR6, ADR-0019, plan 10
//! §5): `GET|POST /api/v1/user/tokens` and `DELETE
//! /api/v1/user/tokens/{id}`.
//!
//! - `GET` (`TokenList`, `read_api`): the caller's own tokens, newest
//!   first (revoked and expired ones included); never a secret or hash.
//! - `POST` (`TokenCreate`, `api`): a token for the caller with the
//!   requested scopes. **Sessions only: a token never creates a token**
//!   (`authorize()`: `FG-AUTH-0002 reason=session_required`, ADR-0019
//!   Amendment M2 4), so this is the browser's route; the `admin` scope only for an
//!   administrator. The secret is in the `201` answer once, never again.
//!   forge-db checks the owner again under the owner's row lock
//!   (`FG-AUTH-0008`: disabled, not an administrator any more, the active
//!   token limit, the name and expiry rules).
//! - `DELETE` (`TokenRevoke`, `api`): the loader reads the token's owner;
//!   another user's token is the same `FG-REPO-0005` as an absent one
//!   (administrators may revoke any, as ADR-0019's matrix says). `204`,
//!   also when it was revoked already.

use std::time::Duration;

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{TokenCtx, TokenOwnerCtx, TokenRequestCtx};
use forge_core::{parse_positive_id, Scope, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::accounts::{TokenFields, TokenInfo, MAX_EXPIRY, MIN_EXPIRY};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::{param_invalid, request_invalid};
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup};
use crate::routes::write::{absent, bounded, hosting, json, no_store, write_request};

/// `GET` / `POST` the caller's tokens.
pub const COLLECTION: &str = "/api/v1/user/tokens";
/// `DELETE` one of them.
pub const ONE: &str = "/api/v1/user/tokens/{id}";
const PREFIX: &str = "/api/v1/user/tokens/";
/// The largest create body.
pub const MAX_BODY: usize = 2048;
const SECONDS_PER_DAY: u64 = 86_400;

/// `POST /api/v1/user/tokens`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateToken {
    /// 1..=100 characters, no control or invisible format characters.
    pub name: String,
    /// `read_repository`, `write_repository`, `read_api`, `api`, `admin`
    /// (administrators only); at least one.
    pub scopes: Vec<String>,
    /// Days until it expires, 1..=365 (default 90).
    #[schema(minimum = 1, maximum = 365)]
    pub expires_in_days: Option<u32>,
    /// The token is for an AI agent (ADR-0017 §4.0); default false.
    pub is_agent: Option<bool>,
}

/// One token as the API shows it (never the secret or its hash).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenBody {
    #[schema(minimum = 1)]
    pub id: i64,
    /// The token's public part (`fgp_<public_id>_…`).
    pub public_id: String,
    pub name: String,
    pub scopes: Vec<String>,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub expires_at: String,
    #[schema(required = true)]
    pub revoked_at: Option<String>,
    #[schema(required = true)]
    pub last_used_at: Option<String>,
}

/// The caller's tokens.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenList {
    pub items: Vec<TokenBody>,
}

/// A created token: the secret, shown this once.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatedToken {
    #[schema(minimum = 1)]
    pub id: i64,
    pub public_id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub expires_at: String,
    pub is_agent: bool,
    /// The whole token. It is never shown again: store it now.
    pub token: String,
}

fn token_body(t: &TokenInfo) -> TokenBody {
    TokenBody {
        id: t.token_id,
        public_id: t.public_id.clone(), // clone: the row is borrowed
        name: t.name.clone(),
        scopes: t.scopes.iter().map(|s| s.as_str().to_owned()).collect(),
        created_at: t.created_at.clone(),
        expires_at: t.expires_at.clone(),
        revoked_at: t.revoked_at.clone(),
        last_used_at: t.last_used_at.clone(),
    }
}

/// The caller (the registry's guard admitted an authenticated one).
fn me(viewer: Option<UserId>) -> Result<UserId, ForgeError> {
    viewer.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "token_route_without_user")
    })
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

/// The scope names of a create body; an unknown or empty list is
/// `FG-WEB-0006 reason=field_invalid field=scopes`.
fn scopes(names: &[String]) -> Result<Vec<Scope>, ForgeError> {
    let invalid = |rule| {
        request_invalid("field_invalid")
            .with_context("field", "scopes")
            .with_context("rule", rule)
    };
    if names.is_empty() {
        return Err(invalid("empty"));
    }
    if names.len() > 8 {
        return Err(invalid("too_many"));
    }
    names
        .iter()
        .map(|n| Scope::parse(n).ok_or_else(|| invalid("unknown_scope")))
        .collect()
}

fn expiry(days: Option<u32>) -> Result<Option<Duration>, ForgeError> {
    let Some(days) = days else {
        return Ok(None);
    };
    let expires = Duration::from_secs(u64::from(days) * SECONDS_PER_DAY);
    if expires < MIN_EXPIRY || expires > MAX_EXPIRY {
        return Err(request_invalid("field_invalid")
            .with_context("field", "expires_in_days")
            .with_context("rule", "out_of_range"));
    }
    Ok(Some(expires))
}

/// `GET /api/v1/user/tokens`: the caller's tokens.
pub struct OwnTokens;

impl Loader<AppState> for OwnTokens {
    type Ctx = TokenOwnerCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, TokenOwnerCtx> {
        Box::pin(async move { Ok(found(TokenOwnerCtx::new(me(viewer)?))) })
    }
}

/// `POST /api/v1/user/tokens`: a token for the caller with the body's scopes.
pub struct NewToken;

impl BodyLoader<AppState, CreateToken> for NewToken {
    type Ctx = TokenRequestCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        body: &'a CreateToken,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, TokenRequestCtx> {
        Box::pin(async move {
            let scopes = scopes(&body.scopes)?;
            Ok(found(TokenRequestCtx::new(me(viewer)?, scopes)))
        })
    }
}

/// `DELETE /api/v1/user/tokens/{id}`: the token and its owner.
pub struct TokenToRevoke;

impl Loader<AppState> for TokenToRevoke {
    type Ctx = TokenCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, TokenCtx> {
        Box::pin(async move {
            let id = parts
                .uri
                .path()
                .strip_prefix(PREFIX)
                .filter(|rest| !rest.contains('/'))
                .and_then(parse_positive_id)
                .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
            let hosting = hosting(state)?;
            Ok(
                match bounded(state, "token_owner_load", hosting.token_owner(id)).await? {
                    Some(owner) => found(TokenCtx::new(id, owner)),
                    None => absent(),
                },
            )
        })
    }
}

fn respond(ctx: &crate::Ctx, status: StatusCode, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET /api/v1/user/tokens` → `200`.
pub async fn list(state: AppState, request: Dispatched<TokenOwnerCtx>) -> Response {
    // The request (its body is not Sync) stays out of the await.
    let w = request.into_write();
    let built = async {
        let hosting = hosting(&state)?;
        let tokens = bounded(&state, "token_list", hosting.tokens(&w.proof)).await?;
        json(TokenList {
            items: tokens.iter().map(token_body).collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST /api/v1/user/tokens` → `201` with the secret (once).
pub async fn create(
    state: AppState,
    request: Dispatched<TokenRequestCtx, CreateToken>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let fields = TokenFields {
            name: w.input.name.clone(), // clone: the body stays with the request
            expires_in: expiry(w.input.expires_in_days)?,
            is_agent: w.input.is_agent.unwrap_or(false),
        };
        let is_agent = fields.is_agent;
        let name = fields.name.clone(); // clone: the answer repeats it
        let issued = hosting(&state)?
            .create_token(&w.proof, &write_request(&w), fields)
            .await?;
        json(CreatedToken {
            id: issued.token_id,
            public_id: issued.token.public_id().as_str().to_owned(),
            name,
            scopes: w
                .proof
                .ctx()
                .scopes()
                .iter()
                .map(|s| s.as_str().to_owned())
                .collect(),
            expires_at: issued.expires_at.clone(),
            is_agent,
            token: issued.token.reveal(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `DELETE /api/v1/user/tokens/{id}` → `204` (also when already revoked).
pub async fn revoke(state: AppState, request: Dispatched<TokenCtx>) -> Response {
    let w = request.into_write();
    let done = async {
        hosting(&state)?
            .revoke_token(&w.proof, &write_request(&w))
            .await
            .map(|_| ())
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
