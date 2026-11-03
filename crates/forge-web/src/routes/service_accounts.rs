//! Service accounts and their tokens (M3 S0, plan 11 §5 S0, ADR-0019
//! Addendum S0), administrators only:
//!
//! - `POST /api/v1/admin/service-accounts` (`ServiceAccountCreate`): a
//!   service account with an owner who is a person and a purpose. **An
//!   administrator's web session only** (a token, an `admin`-scoped one
//!   included, is `FG-AUTH-0002 reason=session_required`). `201`.
//! - `GET /api/v1/admin/service-accounts` (`ServiceAccountList`, `read_api`
//!   + administrator rights): every service account, at most 500.
//! - `GET /api/v1/admin/service-accounts/{id}` (`ServiceAccountRead`) and
//!   `PATCH` (`ServiceAccountUpdate`, people only): display name, purpose,
//!   owner. A user of another kind is `FG-REPO-0005` like an absent one.
//! - `GET /api/v1/admin/service-accounts/{id}/tokens`
//!   (`ServiceAccountTokenList`) and `POST` (`ServiceAccountTokenIssue`,
//!   **a web session only**, never `admin`, never for a disabled account):
//!   the secret is in the `201` answer once; the caller is recorded as the
//!   token's issuer.
//! - `DELETE /api/v1/admin/service-accounts/{id}/tokens/{token_id}`
//!   (`ServiceAccountTokenRevoke`, an `admin`-scoped token included: the
//!   leak response). A token of another user is `FG-REPO-0005`. `204`, also
//!   when it was revoked already.
//!
//! Disabling a service account is `POST /api/v1/admin/users/{id}/disable`
//! (its tokens are revoked in the same change).

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{
    InstanceCtx, ServiceAccountCtx, ServiceAccountTokenCtx, ServiceAccountTokenRequestCtx,
    ServiceAccountView,
};
use forge_core::{parse_positive_id, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::accounts::TokenFields;
use forge_hosting::service_accounts::{
    ServiceAccount, ServiceAccountChanges, ServiceAccountFields, ServiceAccountTarget,
    ServiceAccountUpdate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::{param_invalid, request_invalid};
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup};
use crate::routes::tokens::{expiry, scopes as parse_scopes, token_body};
use crate::routes::write::{absent, bounded, hosting, json, no_store, write_request};

/// `GET` / `POST` the service accounts.
pub const COLLECTION: &str = "/api/v1/admin/service-accounts";
/// `GET` / `PATCH` one.
pub const ONE: &str = "/api/v1/admin/service-accounts/{id}";
/// `GET` / `POST` its tokens.
pub const TOKENS: &str = "/api/v1/admin/service-accounts/{id}/tokens";
/// `DELETE` one of its tokens.
pub const TOKEN: &str = "/api/v1/admin/service-accounts/{id}/tokens/{token_id}";
const PREFIX: &str = "/api/v1/admin/service-accounts/";
const TOKENS_SUFFIX: &str = "/tokens";
/// The largest body.
pub const MAX_BODY: usize = 4096;

/// `POST /api/v1/admin/service-accounts`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateServiceAccount {
    /// Also its personal namespace's path (a top-level segment).
    pub username: String,
    /// 1..=255 characters (default: the username).
    pub display_name: Option<String>,
    /// The person who answers for it (an active human user's id).
    #[schema(minimum = 1)]
    pub owner_id: i64,
    /// Why it exists, 1..=500 characters, one line (one account per
    /// purpose).
    pub purpose: String,
}

/// `PATCH /api/v1/admin/service-accounts/{id}`: the fields to change.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateServiceAccount {
    pub display_name: Option<String>,
    pub purpose: Option<String>,
    /// A new owner (an active human user's id): offboarding hands it on.
    #[schema(minimum = 1)]
    pub owner_id: Option<i64>,
}

/// `POST /api/v1/admin/service-accounts/{id}/tokens`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueServiceAccountToken {
    /// 1..=100 characters, no control or invisible format characters.
    pub name: String,
    /// `read_repository`, `write_repository`, `read_api`, `api`,
    /// `merge_execute`, `merge_approve`, `user_email`; never `admin`.
    pub scopes: Vec<String>,
    /// Days until it expires, 1..=365 (default 90).
    #[schema(minimum = 1, maximum = 365)]
    pub expires_in_days: Option<u32>,
    /// The token is for an AI agent (ADR-0017 §4.0); default false.
    pub is_agent: Option<bool>,
}

/// The person a service account belongs to.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnerBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub username: String,
    pub active: bool,
}

/// One service account (no personal data: it has none).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceAccountBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub username: String,
    pub display_name: String,
    /// `active` or `disabled`.
    pub state: String,
    /// `null` only for an account created before M3 S0.
    #[schema(required = true)]
    pub owner: Option<OwnerBody>,
    #[schema(required = true)]
    pub purpose: Option<String>,
    #[schema(required = true)]
    pub created_by: Option<i64>,
    /// UTC, RFC 3339.
    pub created_at: String,
    /// Active (not revoked, not expired) tokens.
    pub active_tokens: i64,
    /// The latest expiry of an active token (UTC, RFC 3339).
    #[schema(required = true)]
    pub last_token_expiry: Option<String>,
}

/// The service accounts.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceAccountList {
    pub items: Vec<ServiceAccountBody>,
    /// More than 500 exist (only the first 500 by id are listed).
    pub truncated: bool,
}

/// What an update changed.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatedServiceAccount {
    /// `updated`, or `unchanged` (nothing was written).
    pub outcome: String,
    /// The fields that changed: `display_name`, `purpose`, `owner`.
    pub fields: Vec<String>,
    pub service_account: ServiceAccountBody,
}

/// One token of a service account (never the secret or its hash).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceAccountTokenBody {
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
    /// The administrator who issued it (`null`: the operator CLI).
    #[schema(required = true)]
    pub issued_by_user_id: Option<i64>,
}

/// A service account's tokens.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceAccountTokenList {
    pub items: Vec<ServiceAccountTokenBody>,
}

/// An issued token: the secret, shown this once. `Debug` is written by
/// hand and never prints the secret (S0 review).
#[derive(Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssuedServiceAccountToken {
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(minimum = 1)]
    pub service_account_id: i64,
    pub public_id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub expires_at: String,
    pub is_agent: bool,
    #[schema(minimum = 1)]
    pub issued_by_user_id: i64,
    /// The whole token. It is never shown again: hand it to the owner now.
    pub token: String,
}

impl std::fmt::Debug for IssuedServiceAccountToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedServiceAccountToken")
            .field("id", &self.id)
            .field("service_account_id", &self.service_account_id)
            .field("public_id", &self.public_id)
            .field("name", &self.name)
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .field("is_agent", &self.is_agent)
            .field("issued_by_user_id", &self.issued_by_user_id)
            .field("token", &crate::routes::tokens::REDACTED)
            .finish()
    }
}

fn body(a: ServiceAccount) -> ServiceAccountBody {
    ServiceAccountBody {
        id: a.id.get(),
        username: a.username,
        display_name: a.display_name,
        state: if a.active { "active" } else { "disabled" }.to_owned(),
        owner: a.owner.map(|o| OwnerBody {
            id: o.id.get(),
            username: o.username,
            active: o.active,
        }),
        purpose: a.purpose,
        created_by: a.created_by.map(UserId::get),
        created_at: a.created_at,
        active_tokens: a.active_tokens,
        last_token_expiry: a.last_token_expiry,
    }
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

fn view(target: ServiceAccountTarget) -> ServiceAccountView {
    ServiceAccountView {
        kind: target.kind,
        active: target.active,
    }
}

fn owner_id(raw: i64) -> Result<UserId, ForgeError> {
    if raw <= 0 {
        return Err(request_invalid("field_invalid")
            .with_context("field", "owner_id")
            .with_context("rule", "not_positive"));
    }
    Ok(UserId::new(raw))
}

/// The `{id}` (and `{token_id}`) of a path under [`PREFIX`]: `rest` is
/// what follows the id (`""`, `/tokens`, `/tokens/{token_id}`).
fn ids(path: &str) -> Result<(UserId, &str), ForgeError> {
    let rest = path
        .strip_prefix(PREFIX)
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let (raw, tail) = rest.split_once('/').map_or((rest, ""), |(id, t)| (id, t));
    let id = parse_positive_id(raw).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    Ok((UserId::new(id), tail))
}

/// Load the account of the path: its kind and state (`absent` when there
/// is no such user; another kind is decided by `authorize()`).
async fn account(state: &AppState, id: UserId) -> Result<Option<ServiceAccountView>, ForgeError> {
    let hosting = hosting(state)?;
    Ok(bounded(
        state,
        "service_account_load",
        hosting.service_account_target(id),
    )
    .await?
    .map(view))
}

/// `GET|PATCH /api/v1/admin/service-accounts/{id}` and `GET …/tokens`.
pub struct OneAccount {
    /// The path's tail after the id: `""` or `tokens`.
    tail: &'static str,
}

/// The loader of `GET|PATCH …/{id}`.
pub const ACCOUNT: OneAccount = OneAccount { tail: "" };
/// The loader of `GET …/{id}/tokens`.
pub const ACCOUNT_TOKENS: OneAccount = OneAccount { tail: "tokens" };

impl OneAccount {
    fn load_ctx<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
    ) -> LoadFuture<'a, ServiceAccountCtx> {
        Box::pin(async move {
            let (id, tail) = ids(parts.uri.path())?;
            if tail != self.tail {
                return Err(param_invalid("id", "not_positive_decimal"));
            }
            Ok(match account(state, id).await? {
                Some(view) => found(ServiceAccountCtx::new(id, view)),
                None => absent(),
            })
        })
    }
}

impl Loader<AppState> for OneAccount {
    type Ctx = ServiceAccountCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, ServiceAccountCtx> {
        self.load_ctx(state, parts)
    }
}

impl BodyLoader<AppState, UpdateServiceAccount> for OneAccount {
    type Ctx = ServiceAccountCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a UpdateServiceAccount,
        _: Option<UserId>,
    ) -> LoadFuture<'a, ServiceAccountCtx> {
        self.load_ctx(state, parts)
    }
}

/// `POST /api/v1/admin/service-accounts`: decided on the instance (the
/// owner named in the body is checked by the command, under its lock).
pub struct NewAccount;

impl BodyLoader<AppState, CreateServiceAccount> for NewAccount {
    type Ctx = InstanceCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: &'a CreateServiceAccount,
        _: Option<UserId>,
    ) -> LoadFuture<'a, InstanceCtx> {
        Box::pin(async { Ok(found(InstanceCtx)) })
    }
}

/// `POST …/{id}/tokens`: the account and the body's scopes.
pub struct TokenRequest;

impl BodyLoader<AppState, IssueServiceAccountToken> for TokenRequest {
    type Ctx = ServiceAccountTokenRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a IssueServiceAccountToken,
        _: Option<UserId>,
    ) -> LoadFuture<'a, ServiceAccountTokenRequestCtx> {
        Box::pin(async move {
            let (id, tail) = ids(parts.uri.path())?;
            if tail != TOKENS_SUFFIX.trim_start_matches('/') {
                return Err(param_invalid("id", "not_positive_decimal"));
            }
            let scopes = parse_scopes(&body.scopes)?;
            Ok(match account(state, id).await? {
                Some(view) => found(ServiceAccountTokenRequestCtx::new(id, view, scopes)),
                None => absent(),
            })
        })
    }
}

/// `DELETE …/{id}/tokens/{token_id}`: the account, and the token only when
/// it is that account's (another user's token is not found).
pub struct TokenToRevoke;

impl Loader<AppState> for TokenToRevoke {
    type Ctx = ServiceAccountTokenCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, ServiceAccountTokenCtx> {
        Box::pin(async move {
            let (id, tail) = ids(parts.uri.path())?;
            let token_id = tail
                .strip_prefix("tokens/")
                .filter(|raw| !raw.contains('/'))
                .and_then(parse_positive_id)
                .ok_or_else(|| param_invalid("token_id", "not_positive_decimal"))?;
            let Some(view) = account(state, id).await? else {
                return Ok(absent());
            };
            let hosting = hosting(state)?;
            let owner = bounded(state, "token_owner_load", hosting.token_owner(token_id)).await?;
            Ok(if owner == Some(id) {
                found(ServiceAccountTokenCtx::new(id, token_id, view))
            } else {
                absent()
            })
        })
    }
}

fn respond(ctx: &crate::Ctx, status: StatusCode, built: Result<Value, ForgeError>) -> Response {
    match built {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// The account vanished between the loader and the read (it is never
/// deleted, so this is a defect): `FG-SYS-0001`.
fn vanished(id: UserId) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "service_account_vanished")
        .with_context("user", id.get())
}

/// `POST /api/v1/admin/service-accounts` → `201`.
pub async fn create(
    state: AppState,
    request: Dispatched<InstanceCtx, CreateServiceAccount>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let input = &w.input;
        let fields = ServiceAccountFields {
            username: input.username.clone(), // clone: the body stays with the request
            display_name: input
                .display_name
                .clone() // clone: as above
                .unwrap_or_else(|| input.username.clone()), // clone: as above
            owner: owner_id(input.owner_id)?,
            purpose: input.purpose.clone(), // clone: as above
        };
        let hosting = hosting(&state)?;
        let created = hosting
            .create_service_account(&w.proof, &write_request(&w), &fields)
            .await?;
        let stored = bounded(
            &state,
            "service_account_read",
            hosting.created_service_account(&w.proof, created.user_id),
        )
        .await?
        .ok_or_else(|| vanished(created.user_id))?;
        json(body(stored))
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `GET /api/v1/admin/service-accounts` → `200`.
pub async fn list(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let listed = bounded(
            &state,
            "service_account_list",
            hosting(&state)?.service_accounts(&w.proof),
        )
        .await?;
        json(ServiceAccountList {
            items: listed.items.into_iter().map(body).collect(),
            truncated: listed.truncated,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET /api/v1/admin/service-accounts/{id}` → `200`.
pub async fn read(state: AppState, request: Dispatched<ServiceAccountCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let id = *w.proof.id();
        let got = bounded(
            &state,
            "service_account_read",
            hosting(&state)?.service_account(&w.proof),
        )
        .await?
        .ok_or_else(|| vanished(id))?;
        json(body(got))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `PATCH /api/v1/admin/service-accounts/{id}` → `200`.
pub async fn update(
    state: AppState,
    request: Dispatched<ServiceAccountCtx, UpdateServiceAccount>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let input = &w.input;
        let changes = ServiceAccountChanges {
            display_name: input.display_name.clone(), // clone: the body stays with the request
            purpose: input.purpose.clone(),           // clone: as above
            owner: input.owner_id.map(owner_id).transpose()?,
        };
        let hosting = hosting(&state)?;
        let outcome = hosting
            .update_service_account(&w.proof, &write_request(&w), &changes)
            .await?;
        let id = *w.proof.id();
        let stored = bounded(
            &state,
            "service_account_read",
            hosting.service_account(&w.proof),
        )
        .await?
        .ok_or_else(|| vanished(id))?;
        let (outcome, fields) = match outcome {
            ServiceAccountUpdate::Updated(fields) => (
                "updated",
                fields.iter().map(|f| f.as_str().to_owned()).collect(),
            ),
            ServiceAccountUpdate::Unchanged => ("unchanged", Vec::new()),
        };
        json(UpdatedServiceAccount {
            outcome: outcome.to_owned(),
            fields,
            service_account: body(stored),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET /api/v1/admin/service-accounts/{id}/tokens` → `200`.
pub async fn tokens(state: AppState, request: Dispatched<ServiceAccountCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let listed = bounded(
            &state,
            "service_account_tokens",
            hosting(&state)?.service_account_tokens(&w.proof),
        )
        .await?;
        json(ServiceAccountTokenList {
            items: listed
                .iter()
                .map(|t| {
                    let b = token_body(t);
                    ServiceAccountTokenBody {
                        id: b.id,
                        public_id: b.public_id,
                        name: b.name,
                        scopes: b.scopes,
                        created_at: b.created_at,
                        expires_at: b.expires_at,
                        revoked_at: b.revoked_at,
                        last_used_at: b.last_used_at,
                        issued_by_user_id: t.issued_by.map(UserId::get),
                    }
                })
                .collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST /api/v1/admin/service-accounts/{id}/tokens` → `201` with the
/// secret (once).
pub async fn issue(
    state: AppState,
    request: Dispatched<ServiceAccountTokenRequestCtx, IssueServiceAccountToken>,
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
        let issuer = w.proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "issue_without_user")
        })?;
        let issued = hosting(&state)?
            .issue_service_account_token(&w.proof, &write_request(&w), fields)
            .await?;
        json(IssuedServiceAccountToken {
            id: issued.token_id,
            service_account_id: w.proof.id().get(),
            public_id: issued.token.public_id().as_str().to_owned(),
            name,
            scopes: w
                .proof
                .ctx()
                .scopes()
                .iter()
                .map(|s| s.as_str().to_owned())
                .collect(),
            expires_at: issued.expires_at.clone(), // clone: the issued value is borrowed
            is_agent,
            issued_by_user_id: issuer.get(),
            token: issued.token.reveal(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `DELETE /api/v1/admin/service-accounts/{id}/tokens/{token_id}` → `204`.
pub async fn revoke(state: AppState, request: Dispatched<ServiceAccountTokenCtx>) -> Response {
    let w = request.into_write();
    let done = async {
        hosting(&state)?
            .revoke_service_account_token(&w.proof, &write_request(&w))
            .await
            .map(|_| ())
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_issued_tokens_debug_never_shows_the_secret() {
        let issued = IssuedServiceAccountToken {
            id: 31,
            service_account_id: 42,
            public_id: "00000000000000a1".to_owned(),
            name: "feed".to_owned(),
            scopes: vec!["read_api".to_owned()],
            expires_at: "2027-01-01T00:00:00Z".to_owned(),
            is_agent: false,
            issued_by_user_id: 1,
            token: "fgp_00000000000000a1_c2VjcmV0LXNlY3JldC1zZWNyZXQtc2VjcmV0".to_owned(),
        };
        let printed = format!("{issued:?} {issued:#?}");
        assert!(!printed.contains("c2VjcmV0"), "{printed}");
        assert!(
            printed.contains(crate::routes::tokens::REDACTED),
            "{printed}"
        );
        assert!(printed.contains("00000000000000a1"), "{printed}");
    }
}
