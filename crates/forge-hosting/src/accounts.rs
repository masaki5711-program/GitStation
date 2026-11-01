//! The account routes' commands and reads (M2 MR6): personal access
//! tokens, web sessions, the user search and the user administration, on
//! the proofs the route registry made.
//!
//! Each command takes the proof of its own action (another action is
//! `FG-SYS-0001 reason=proof_action_mismatch`) and acts on the id the
//! proof carries, never on the request again. The forge-db commands are
//! the backstops under their own locks: `tokens::issue` re-reads the
//! owner's state and administrator flag (a disable or demotion in between
//! refuses the token, `FG-AUTH-0008`), `users::disable` always enforces the
//! administration guard (`Guard::Enforce`: no HTTP caller can force a
//! disable past it, ADR-0019 §10), and a token's or session's owner never
//! changes, so the proof's owner is the row's.

use std::time::Duration;

use forge_auth::{
    Action, Authorized, SessionCtx, SessionRoute, TokenCtx, TokenOwnerCtx, TokenRequestCtx,
    UserTargetCtx,
};
use forge_core::{SessionRevokeReason, UserId};
use forge_db::sessions;
use forge_db::tokens::{self, NewToken};
use forge_db::users;
use forge_errors::{ErrorCode, ForgeError};

use crate::{caller, change_context, require, Hosting, WriteRequest};

pub use forge_db::sessions::{
    is_public_id as is_session_public_id, RevokeOutcome as SessionRevokeOutcome, SessionInfo,
    SessionList, MAX_LISTED as MAX_LISTED_SESSIONS,
};
pub use forge_db::tokens::{
    Issued as IssuedToken, RevokeOutcome as TokenRevokeOutcome, TokenInfo, DEFAULT_EXPIRY,
    MAX_EXPIRY, MIN_EXPIRY,
};
pub use forge_db::users::{
    AdminUserRow, DisableOutcome, StateFilter as UserStateFilter, UserMatch, MAX_ADMIN_PAGE,
    MAX_SEARCH_CHARS, MAX_SEARCH_RESULTS, MIN_SEARCH_CHARS,
};

/// A token request as the route parsed it (the scopes are the proof's).
#[derive(Debug, Clone)]
pub struct TokenFields {
    pub name: String,
    /// `None` = the default expiry (90 days).
    pub expires_in: Option<Duration>,
    pub is_agent: bool,
}

impl Hosting {
    // ── tokens ──

    /// The tokens of the proof's owner (`TokenList`), newest first.
    pub async fn tokens(
        &self,
        proof: &Authorized<TokenOwnerCtx>,
    ) -> Result<Vec<TokenInfo>, ForgeError> {
        require(proof.action(), Action::TokenList)?;
        tokens::list(&self.db, *proof.id()).await
    }

    /// The owner of token `id` for the revoke route's loader (`None`:
    /// absent).
    pub async fn token_owner(&self, id: i64) -> Result<Option<UserId>, ForgeError> {
        tokens::owner(&self.db, id).await
    }

    /// Issue a token for the proof's owner with the proof's scopes
    /// (`TokenCreate`; the secret is in the answer only).
    pub async fn create_token(
        &self,
        proof: &Authorized<TokenRequestCtx>,
        request: &WriteRequest,
        fields: TokenFields,
    ) -> Result<IssuedToken, ForgeError> {
        require(proof.action(), Action::TokenCreate)?;
        let user = caller(proof)?;
        let new = NewToken {
            user: *proof.id(),
            name: fields.name,
            scopes: proof.ctx().scopes().to_vec(),
            expires_in: fields.expires_in,
            is_agent: fields.is_agent,
        };
        tokens::issue(
            &self.db,
            &change_context(request, proof.principal(), user),
            &new,
        )
        .await
    }

    /// Revoke the proof's token (`TokenRevoke`).
    pub async fn revoke_token(
        &self,
        proof: &Authorized<TokenCtx>,
        request: &WriteRequest,
    ) -> Result<TokenRevokeOutcome, ForgeError> {
        require(proof.action(), Action::TokenRevoke)?;
        let user = caller(proof)?;
        tokens::revoke(
            &self.db,
            &change_context(request, proof.principal(), user),
            *proof.id(),
        )
        .await
    }

    // ── sessions ──

    /// The active sessions of the proof's owner (`SessionList`).
    pub async fn sessions(
        &self,
        proof: &Authorized<SessionCtx>,
    ) -> Result<SessionList, ForgeError> {
        require(proof.action(), Action::SessionList)?;
        sessions::list(&self.db, proof.id().0).await
    }

    /// The row id and owner of the session `public_id` names, for the
    /// revoke route's loader (`None`: absent). `public_id` must have the
    /// shape [`is_session_public_id`] accepts.
    pub async fn session_owner(
        &self,
        public_id: &str,
    ) -> Result<Option<(i64, UserId)>, ForgeError> {
        sessions::owner(&self.db, public_id).await
    }

    /// End the proof's one session (`SessionRevoke` with a session):
    /// `reason=logout` on the user's own route, `admin` on the
    /// administration's.
    pub async fn revoke_session(
        &self,
        proof: &Authorized<SessionCtx>,
        request: &WriteRequest,
    ) -> Result<SessionRevokeOutcome, ForgeError> {
        require(proof.action(), Action::SessionRevoke)?;
        let user = caller(proof)?;
        let Some(session) = proof.id().1 else {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "session_revoke_without_session"));
        };
        let reason = match proof.ctx().route() {
            SessionRoute::Own => SessionRevokeReason::Logout,
            SessionRoute::Admin => SessionRevokeReason::Admin,
        };
        sessions::revoke(
            &self.db,
            &change_context(request, proof.principal(), user),
            session,
            reason,
        )
        .await
    }

    /// End every session of the proof's owner (`SessionRevoke` without a
    /// session, the administration's route): `reason=admin`; how many.
    pub async fn revoke_all_sessions(
        &self,
        proof: &Authorized<SessionCtx>,
        request: &WriteRequest,
    ) -> Result<i64, ForgeError> {
        require(proof.action(), Action::SessionRevoke)?;
        let user = caller(proof)?;
        let (owner, session) = *proof.id();
        if session.is_some() || proof.ctx().route() != SessionRoute::Admin {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "revoke_all_needs_the_admin_route"));
        }
        sessions::revoke_all(
            &self.db,
            &change_context(request, proof.principal(), user),
            owner,
            SessionRevokeReason::Admin,
        )
        .await
    }

    // ── users ──

    /// Active users matching `term` (`UserSearch` was proved on the
    /// instance; names only).
    pub async fn search_users(&self, term: &str) -> Result<Vec<UserMatch>, ForgeError> {
        users::search(&self.db, term).await
    }

    /// One administration page (`UserAdminList` was proved on the
    /// instance).
    pub async fn admin_users(
        &self,
        after: Option<UserId>,
        state: UserStateFilter,
        max_rows: u32,
    ) -> Result<Vec<AdminUserRow>, ForgeError> {
        users::admin_page(&self.db, after, state, max_rows).await
    }

    /// Disable the proof's user (`UserDisable`), the administration guard
    /// enforced: `FG-AUTH-0009` `last_admin` / `last_owner` refuse it and
    /// nothing is written. There is no way to force it from here.
    pub async fn disable_user(
        &self,
        proof: &Authorized<UserTargetCtx>,
        request: &WriteRequest,
    ) -> Result<DisableOutcome, ForgeError> {
        require(proof.action(), Action::UserDisable)?;
        let user = caller(proof)?;
        users::disable(
            &self.db,
            &change_context(request, proof.principal(), user),
            *proof.id(),
        )
        .await
    }
}
