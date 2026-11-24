//! A person's delegation of approvals (ADR-0038 §3.5, step C): read
//! (`ApprovalDelegationRead`), set and remove (`ApprovalDelegationManage`,
//! a person in a web session). The proof names the person
//! (`TokenOwnerCtx`: the caller themself).

use forge_auth::{Action, Authorized, TokenOwnerCtx};
use forge_db::review::{self, DelegationOutcome, StoredDelegation};
use forge_errors::ForgeError;

use crate::{change_context, require, Review, ReviewRequest};

/// One's delegation and the delegations naming oneself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegations {
    /// The caller's own (`None`: none set).
    pub mine: Option<StoredDelegation>,
    /// People who delegate to the caller and whose period has not ended.
    pub to_me: Vec<StoredDelegation>,
}

impl Review {
    /// `GET /api/v1/user/approval-delegation`.
    pub async fn delegations(
        &self,
        proof: &Authorized<TokenOwnerCtx>,
    ) -> Result<Delegations, ForgeError> {
        require(proof.action(), Action::ApprovalDelegationRead)?;
        let me = *proof.id();
        Ok(Delegations {
            mine: review::delegation_of(&self.db, me).await?,
            to_me: review::delegations_to(&self.db, me).await?,
        })
    }

    /// `PUT /api/v1/user/approval-delegation`: `delegate` (a username)
    /// approves for the caller from `starts_at` until `ends_at` (at most 30
    /// days; `FG-WEB-0006` otherwise).
    pub async fn set_delegation(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<TokenOwnerCtx>,
        delegate: &str,
        starts_at: &str,
        ends_at: &str,
    ) -> Result<(DelegationOutcome, StoredDelegation), ForgeError> {
        require(proof.action(), Action::ApprovalDelegationManage)?;
        let me = *proof.id();
        let ctx = change_context(request, proof.principal(), me)?;
        let (outcome, stored) =
            review::set_delegation_decided(&self.db, &ctx, me, delegate, starts_at, ends_at)
                .await?;
        tracing::info!(
            target: "forge_review",
            event = "forge.review.delegation_set",
            request_id = %request.request_id,
            user_id = me.get(),
            delegate_id = stored.delegate.get(),
            starts_at = %stored.starts_at,
            ends_at = %stored.ends_at,
            changed = outcome == DelegationOutcome::Set,
            "approval delegation set"
        );
        Ok((outcome, stored))
    }

    /// `DELETE /api/v1/user/approval-delegation`.
    pub async fn remove_delegation(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<TokenOwnerCtx>,
    ) -> Result<DelegationOutcome, ForgeError> {
        require(proof.action(), Action::ApprovalDelegationManage)?;
        let me = *proof.id();
        let ctx = change_context(request, proof.principal(), me)?;
        review::remove_delegation_decided(&self.db, &ctx, me).await
    }
}
