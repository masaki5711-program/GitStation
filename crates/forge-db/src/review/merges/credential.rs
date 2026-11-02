//! The requester's credential as the merge job reads it again (ADR-0024 §5):
//! the account kind (`users.kind`), and the token's scopes, agent flag and
//! revocation / expiry or the session's revocation / expiry — from the
//! database at the time of the decision, never from a copy taken at the
//! request.

use forge_core::ids::UserId;
use forge_core::{Scope, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::CredentialRef;
use crate::query_error;
use crate::tokens::parse_scopes;

/// What the credential is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialState {
    /// A web session; `usable` = not revoked and not past its idle or
    /// absolute expiry.
    Session { usable: bool },
    /// A personal access token; `usable` = not revoked and not expired.
    Token {
        scopes: Vec<Scope>,
        is_agent: bool,
        usable: bool,
    },
    /// No such credential of this user (deleted, or another user's): the
    /// decision refuses it like a revoked one.
    Missing,
}

/// The requester's account kind and credential (the account's `active` /
/// `is_admin` come with the review facts, locked `FOR SHARE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialFacts {
    pub kind: UserKind,
    pub credential: CredentialState,
}

fn unknown_user(user: UserId) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "merge_requester_missing")
        .with_context("user_id", user.get())
}

/// Read `user`'s kind and `credential` (which must be `user`'s: another
/// user's or a missing one is [`CredentialState::Missing`] — never a
/// pass).
pub(crate) async fn credential_on(
    conn: &mut PgConnection,
    user: UserId,
    credential: CredentialRef,
) -> Result<CredentialFacts, ForgeError> {
    const QUERY: &str = "review.merge_credential";
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM forge.users WHERE id = $1")
        .bind(user.get())
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| query_error(QUERY, e))?;
    let kind = crate::users::parse_kind(user.get(), &kind.ok_or_else(|| unknown_user(user))?)?;
    let credential = match credential {
        CredentialRef::Token(id) => {
            let row: Option<(String, Vec<String>, bool, bool)> = sqlx::query_as(
                "SELECT public_id, scopes, is_agent, \
                 revoked_at IS NULL AND expires_at > now() \
                 FROM forge.tokens WHERE id = $1 AND user_id = $2",
            )
            .bind(id)
            .bind(user.get())
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            match row {
                Some((public_id, scopes, is_agent, usable)) => CredentialState::Token {
                    scopes: parse_scopes(id, &public_id, &scopes)?,
                    is_agent,
                    usable,
                },
                None => CredentialState::Missing,
            }
        }
        CredentialRef::Session(id) => {
            let usable: Option<bool> = sqlx::query_scalar(
                "SELECT revoked_at IS NULL AND idle_expires_at > now() \
                 AND absolute_expires_at > now() \
                 FROM forge.web_sessions WHERE id = $1 AND user_id = $2",
            )
            .bind(id)
            .bind(user.get())
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            match usable {
                Some(usable) => CredentialState::Session { usable },
                None => CredentialState::Missing,
            }
        }
    };
    Ok(CredentialFacts { kind, credential })
}
