//! Authentication of a presented personal access token (step D3,
//! ADR-0019 §7). forge-db implements [`Authenticator`] (`tokens::authenticate`
//! on the write pool); forge-web's authentication middleware calls it through
//! `AppState.authenticator`. The trait lives here so forge-web does not depend
//! on forge-db (CLAUDE.md §3-9, the same shape as
//! [`crate::readiness::ReadinessProbe`]).

use std::future::Future;
use std::pin::Pin;

use forge_errors::ForgeError;

use crate::{RequestId, Scope, UserId};

/// Who a valid token stands for. The owner is active: a disabled owner's
/// token is refused by the authenticator itself (`FG-AUTH-0004`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenIdentity {
    pub token_id: i64,
    pub user_id: UserId,
    /// The owner's username and display name (`GET /api/v1/user`; never an
    /// authorization input).
    pub username: String,
    pub display_name: String,
    /// The owner is an administrator. Administrator **rights** also need the
    /// token's `admin` scope (`forge_auth::Principal::admin_rights`).
    pub is_admin: bool,
    pub scopes: Vec<Scope>,
    /// Issued for an AI agent (ADR-0017 §4.0; reaches `Credential` in W1).
    pub is_agent: bool,
}

/// The future an authenticator returns (boxed: stored as a trait object).
pub type AuthFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TokenIdentity, ForgeError>> + Send + 'a>>;

/// Resolves a presented token string to its owner.
pub trait Authenticator: Send + Sync {
    /// `presented` is the whole token as the client sent it (a secret: the
    /// implementation and its errors never log or quote it).
    ///
    /// Errors (ADR-0019 §3): `FG-AUTH-0001` with a `reason` in the context
    /// (`malformed`, `unknown`, `mismatch`, `revoked`, `expired`);
    /// `FG-AUTH-0004` (`reason=owner_disabled`) for a disabled owner;
    /// `FG-DB-0001` when the store cannot be reached (never a 401);
    /// `FG-DB-0019` for a stored row that cannot be read.
    fn authenticate<'a>(&'a self, request_id: &'a RequestId, presented: &'a str) -> AuthFuture<'a>;
}

/// Who a valid web session stands for (M2 MR3, ADR-0021 §5). The user is
/// active: a disabled user's session is refused by the authenticator
/// itself (`FG-AUTH-0004`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    /// `forge.web_sessions.id`.
    pub session_id: i64,
    /// The session's public id (safe to log).
    pub public_id: String,
    pub user_id: UserId,
    /// The user's username and display name (`GET /api/v1/session`; never
    /// an authorization input).
    pub username: String,
    pub display_name: String,
    /// The user is an administrator (a session carries every scope, so
    /// this is administrator rights: `forge_auth::Principal::admin_rights`).
    pub is_admin: bool,
    /// When the session ends without activity / at the latest (RFC 3339, UTC).
    pub idle_expires_at: String,
    pub absolute_expires_at: String,
}

/// The future a session authenticator returns.
pub type SessionAuthFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SessionIdentity, ForgeError>> + Send + 'a>>;

/// Resolves a presented `__Host-gstn_session` cookie value to its user
/// (M2 MR3, ADR-0021 §5). forge-db implements it (`forge_db::sessions`, on
/// the write pool); forge-web's cookie authentication calls it — the same
/// shape as [`Authenticator`], so forge-web does not depend on forge-db.
pub trait SessionAuthenticator: Send + Sync {
    /// `presented` is the whole cookie value (a secret: never logged or
    /// quoted by the implementation or its errors).
    ///
    /// Errors: `FG-AUTH-0001` with `reason` (`session_malformed`,
    /// `session_unknown`, `session_mismatch`, `session_revoked`,
    /// `session_expired`); `FG-AUTH-0004` (`reason=owner_disabled`) for a
    /// disabled user; `FG-DB-0001` when the store cannot be reached (never a
    /// 401); `FG-DB-0019` for a stored row that cannot be read.
    fn authenticate_session<'a>(
        &'a self,
        request_id: &'a RequestId,
        presented: &'a str,
    ) -> SessionAuthFuture<'a>;
}
