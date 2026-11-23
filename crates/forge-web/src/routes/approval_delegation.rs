//! A person's delegation of approvals (ADR-0038 §3.5, step C):
//! `GET|PUT|DELETE /api/v1/user/approval-delegation`.
//!
//! - `GET` (`ApprovalDelegationRead`, `read_api`): the caller's own
//!   delegation (`mine`, `null` when none) and the people who delegate to
//!   the caller and whose period has not ended (`to_me`).
//! - `PUT` (`ApprovalDelegationManage`: a person in a web session):
//!   `{"delegate", "starts_at", "ends_at"}` — at most 30 days, not oneself,
//!   an active person (`400 FG-WEB-0006` otherwise); replaces any.
//! - `DELETE` (`ApprovalDelegationManage`): `204`, also when there was none.
//!
//! The proof names the caller (`TokenOwnerCtx`); nobody reads or changes
//! another person's delegation, administrators included.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::TokenOwnerCtx;
use forge_core::UserId;
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{Review, ReviewRequest, StoredDelegation};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, WriteParts};
use crate::routes::write::{bounded, json, no_store, own};

/// The route.
pub const PATH: &str = "/api/v1/user/approval-delegation";
/// The body cap.
pub const MAX_BODY: usize = 4 * 1024;

/// Why a caller without a user finds no delegation.
const WITHOUT_USER: &str = "delegation_route_without_user";

/// `PUT /api/v1/user/approval-delegation`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutDelegation {
    /// The delegate's username (an active person, not the caller).
    pub delegate: String,
    /// A timestamp with a time zone (RFC 3339).
    pub starts_at: String,
    /// After `starts_at` and now, at most 30 days after `starts_at`.
    pub ends_at: String,
}

/// One delegation.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegationBody {
    #[schema(minimum = 1)]
    pub delegator_id: i64,
    pub delegator_username: String,
    pub delegator_display_name: String,
    #[schema(minimum = 1)]
    pub delegate_id: i64,
    pub delegate_username: String,
    pub delegate_display_name: String,
    /// UTC, RFC 3339.
    pub starts_at: String,
    pub ends_at: String,
    /// The period covers now.
    pub active: bool,
}

/// `GET /api/v1/user/approval-delegation`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegationState {
    #[schema(required = true)]
    pub mine: Option<DelegationBody>,
    pub to_me: Vec<DelegationBody>,
}

fn body(d: StoredDelegation) -> DelegationBody {
    DelegationBody {
        delegator_id: d.delegator.get(),
        delegator_username: d.delegator_username,
        delegator_display_name: d.delegator_display_name,
        delegate_id: d.delegate.get(),
        delegate_username: d.delegate_username,
        delegate_display_name: d.delegate_display_name,
        starts_at: d.starts_at,
        ends_at: d.ends_at,
        active: d.active,
    }
}

fn review(state: &AppState) -> Result<&Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

fn request_of<B>(w: &WriteParts<TokenOwnerCtx, B>) -> ReviewRequest {
    ReviewRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

/// The caller's own delegation.
pub struct OwnDelegation;

impl Loader<AppState> for OwnDelegation {
    type Ctx = TokenOwnerCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, TokenOwnerCtx> {
        Box::pin(async move { Ok(own(viewer, WITHOUT_USER, TokenOwnerCtx::new)) })
    }
}

impl BodyLoader<AppState, PutDelegation> for OwnDelegation {
    type Ctx = TokenOwnerCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: &'a PutDelegation,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, TokenOwnerCtx> {
        Box::pin(async move { Ok(own(viewer, WITHOUT_USER, TokenOwnerCtx::new)) })
    }
}

/// `GET` → `200`.
pub async fn get(state: AppState, request: Dispatched<TokenOwnerCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let found = bounded(&state, "delegations", review(&state)?.delegations(&w.proof)).await?;
        json(DelegationState {
            mine: found.mine.map(body),
            to_me: found.to_me.into_iter().map(body).collect(),
        })
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `PUT` → `200` the delegation.
pub async fn put(state: AppState, request: Dispatched<TokenOwnerCtx, PutDelegation>) -> Response {
    let w = request.into_write();
    let built = async {
        let (_, stored) = review(&state)?
            .set_delegation(
                &request_of(&w),
                &w.proof,
                &w.input.delegate,
                &w.input.starts_at,
                &w.input.ends_at,
            )
            .await?;
        json(body(stored))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `DELETE` → `204`.
pub async fn delete(state: AppState, request: Dispatched<TokenOwnerCtx>) -> Response {
    let w = request.into_write();
    let done = async {
        review(&state)?
            .remove_delegation(&request_of(&w), &w.proof)
            .await
    }
    .await;
    match done {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
