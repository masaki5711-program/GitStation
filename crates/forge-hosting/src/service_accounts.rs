//! The service account routes' commands and reads (M3 S0, plan 11 §5 S0,
//! ADR-0019 Addendum S0), on the proofs the route registry made.
//!
//! Each command takes the proof of its own action (another action is
//! `FG-SYS-0001 reason=proof_action_mismatch`) and acts on the id the proof
//! carries. The forge-db commands are the backstops under their own locks:
//! `service_accounts::create` / `update` re-check the owner (an active
//! person) under its row lock, and `tokens::issue_for_service_account`
//! re-reads the account's kind and state under its row lock (a disable in
//! between refuses the token, `FG-AUTH-0008 owner_disabled`). A user's kind
//! never changes, so the loaders' kind is the row's.

use forge_auth::{
    Action, Authorized, InstanceCtx, ServiceAccountCtx, ServiceAccountTokenCtx,
    ServiceAccountTokenRequestCtx,
};
use forge_core::UserId;
use forge_db::service_accounts::{self, Changes, NewServiceAccount, UpdateOutcome};
use forge_db::tokens::{self, NewToken};
use forge_errors::ForgeError;

use crate::accounts::TokenFields;
use crate::{caller, change_context, require, Hosting, WriteRequest};

pub use forge_db::service_accounts::{
    Changes as ServiceAccountChanges, Created as CreatedServiceAccount, Listed as ServiceAccounts,
    NewServiceAccount as ServiceAccountFields, Owner as ServiceAccountOwner, ServiceAccount,
    Target as ServiceAccountTarget, UpdateOutcome as ServiceAccountUpdate, EXPIRY_WARNING,
    MAX_LISTED as MAX_LISTED_SERVICE_ACCOUNTS, MAX_PURPOSE_CHARS,
};

impl Hosting {
    /// The kind and state of user `id` for the routes' loaders (`None`:
    /// absent). A user of another kind is decided by `authorize()`
    /// (`FG-REPO-0005 not_service_account`).
    pub async fn service_account_target(
        &self,
        id: UserId,
    ) -> Result<Option<ServiceAccountTarget>, ForgeError> {
        service_accounts::target(&self.db, id).await
    }

    /// Create a service account (`ServiceAccountCreate`, an administrator's
    /// session): the proof's caller is its creator.
    pub async fn create_service_account(
        &self,
        proof: &Authorized<InstanceCtx>,
        request: &WriteRequest,
        new: &NewServiceAccount,
    ) -> Result<CreatedServiceAccount, ForgeError> {
        require(proof.action(), Action::ServiceAccountCreate)?;
        let user = caller(proof)?;
        service_accounts::create(
            &self.db,
            &change_context(request, proof.principal(), user),
            new,
            user,
        )
        .await
    }

    /// The account [`Hosting::create_service_account`] just created, for its
    /// `201` answer (the proof is the create's).
    pub async fn created_service_account(
        &self,
        proof: &Authorized<InstanceCtx>,
        id: UserId,
    ) -> Result<Option<ServiceAccount>, ForgeError> {
        require(proof.action(), Action::ServiceAccountCreate)?;
        service_accounts::get(&self.db, id).await
    }

    /// Every service account (`ServiceAccountList`).
    pub async fn service_accounts(
        &self,
        proof: &Authorized<InstanceCtx>,
    ) -> Result<ServiceAccounts, ForgeError> {
        require(proof.action(), Action::ServiceAccountList)?;
        service_accounts::list(&self.db).await
    }

    /// The proof's service account (`ServiceAccountRead`, or after
    /// `ServiceAccountUpdate` to answer with the stored state); `None` when
    /// it vanished after the loader read it.
    pub async fn service_account(
        &self,
        proof: &Authorized<ServiceAccountCtx>,
    ) -> Result<Option<ServiceAccount>, ForgeError> {
        if proof.action() != Action::ServiceAccountUpdate {
            require(proof.action(), Action::ServiceAccountRead)?;
        }
        service_accounts::get(&self.db, *proof.id()).await
    }

    /// Change the proof's service account (`ServiceAccountUpdate`).
    pub async fn update_service_account(
        &self,
        proof: &Authorized<ServiceAccountCtx>,
        request: &WriteRequest,
        changes: &Changes,
    ) -> Result<UpdateOutcome, ForgeError> {
        require(proof.action(), Action::ServiceAccountUpdate)?;
        let user = caller(proof)?;
        service_accounts::update(
            &self.db,
            &change_context(request, proof.principal(), user),
            *proof.id(),
            changes,
        )
        .await
    }

    /// Issue a token for the proof's service account with the proof's
    /// scopes (`ServiceAccountTokenIssue`); the caller is recorded as its
    /// issuer. The secret is in the answer only.
    pub async fn issue_service_account_token(
        &self,
        proof: &Authorized<ServiceAccountTokenRequestCtx>,
        request: &WriteRequest,
        fields: TokenFields,
    ) -> Result<tokens::Issued, ForgeError> {
        require(proof.action(), Action::ServiceAccountTokenIssue)?;
        let user = caller(proof)?;
        let new = NewToken {
            user: *proof.id(),
            name: fields.name,
            scopes: proof.ctx().scopes().to_vec(),
            expires_in: fields.expires_in,
            is_agent: fields.is_agent,
        };
        tokens::issue_for_service_account(
            &self.db,
            &change_context(request, proof.principal(), user),
            &new,
            user,
        )
        .await
    }

    /// The proof's service account's tokens (`ServiceAccountTokenList`),
    /// newest first; never a secret.
    pub async fn service_account_tokens(
        &self,
        proof: &Authorized<ServiceAccountCtx>,
    ) -> Result<Vec<tokens::TokenInfo>, ForgeError> {
        require(proof.action(), Action::ServiceAccountTokenList)?;
        tokens::list(&self.db, *proof.id()).await
    }

    /// Revoke the proof's token of the proof's service account
    /// (`ServiceAccountTokenRevoke`; the loader checked the token is the
    /// account's).
    pub async fn revoke_service_account_token(
        &self,
        proof: &Authorized<ServiceAccountTokenCtx>,
        request: &WriteRequest,
    ) -> Result<tokens::RevokeOutcome, ForgeError> {
        require(proof.action(), Action::ServiceAccountTokenRevoke)?;
        let user = caller(proof)?;
        let (_, token_id) = *proof.id();
        tokens::revoke(
            &self.db,
            &change_context(request, proof.principal(), user),
            token_id,
        )
        .await
    }
}
