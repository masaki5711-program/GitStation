//! Web sessions (M2 MR3, ADR-0021 §5): issue, authenticate, touch, revoke,
//! revoke all of a user. The browser holds `__Host-gstn_session` =
//! `gss_<public_id>_<secret>` (`forge_core::session_token`); only
//! `sha256(secret)` is stored, and only the write pool reads it
//! (ADR-0018, like tokens).
//!
//! `issue` and `revoke` each write one audit row on the user
//! (`session.created` / `session.revoked`; `revoke_all` writes one
//! `session.revoked_all`), never a domain event (ADR-0021 §5). A disable
//! revokes the user's sessions in its own change (`users::disable`, ADR-0019
//! Amendment M2). Activity (`last_seen_at`, `idle_expires_at`) is recorded
//! at most once a minute, unaudited.

use std::time::Duration;

use forge_core::ids::UserId;
use forge_core::session_token::{self, SessionToken};
use forge_core::{AuditDetails, SessionRevokeReason, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::PgConnection;

use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::{query_error, Db};

/// An RFC 3339 UTC rendering of a timestamptz column, computed by the server.
macro_rules! utc {
    ($col:literal) => {
        concat!(
            "to_char(",
            $col,
            " AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
        )
    };
}

// Declared after `utc!` so the submodule can use it (textual macro scope).
mod auth;

pub use auth::{authenticate, SessionStore};

/// The audit resource type of session changes (tables.toml `audit_resource`).
const RESOURCE: &str = "user";
/// The shortest lifetime a session may be issued with (`web.*` refuses
/// less at load time).
pub const MIN_LIFETIME: Duration = Duration::from_secs(60);
/// The longest absolute lifetime (ADR-0021 §5; the table's CHECK too).
pub const MAX_ABSOLUTE: Duration = Duration::from_secs(24 * 3600);
/// The longest IdP value stored (issuer, subject, sid; the CHECKs).
const MAX_OIDC_CHARS: usize = 255;

/// What the OIDC login knows of the IdP session (MR4 / MR5 fill it).
#[derive(Debug, Clone)]
pub struct OidcSession {
    /// `iss` of the ID token.
    pub issuer: String,
    /// `sub` of the ID token (never logged, never audited).
    pub subject: String,
    /// `sid` of the ID token, when the IdP sends one.
    pub sid: Option<String>,
    /// The sealed refresh token (`forge_core::web_keys`, purpose
    /// `refresh-token`, the public id as associated data).
    pub refresh_token_enc: Option<Vec<u8>>,
    /// The sealed ID token (purpose `id-token`).
    pub id_token_enc: Option<Vec<u8>>,
}

/// The request to issue a session.
#[derive(Debug, Clone)]
pub struct NewSession {
    pub user: UserId,
    /// `web.session_idle_secs`.
    pub idle: Duration,
    /// `web.session_absolute_secs`.
    pub absolute: Duration,
    pub oidc: Option<OidcSession>,
}

/// What `issue` wrote. `token` is the only place the secret exists: set it
/// as the cookie and drop it.
#[derive(Debug)]
pub struct Issued {
    pub session_id: i64,
    pub token: SessionToken,
    /// RFC 3339, UTC.
    pub idle_expires_at: String,
    pub absolute_expires_at: String,
}

/// What `revoke` did.
#[must_use = "AlreadyEnded wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeOutcome {
    /// One `session.revoked` audit row.
    Revoked,
    /// Already revoked: nothing was written.
    AlreadyEnded,
}

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

fn not_found(resource: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", resource)
        .with_context("reason", "absent")
}

/// Whole seconds of a lifetime the caller validated (`web.*` at load).
fn lifetime_secs(new: &NewSession) -> Result<(f64, f64), ForgeError> {
    let in_range = new.idle >= MIN_LIFETIME && new.idle <= new.absolute;
    if !in_range || new.absolute > MAX_ABSOLUTE {
        return Err(internal("session_lifetime_out_of_range")
            .with_context("idle_s", new.idle.as_secs())
            .with_context("absolute_s", new.absolute.as_secs()));
    }
    // At most 86 400: exact as f64.
    Ok((new.idle.as_secs() as f64, new.absolute.as_secs() as f64))
}

fn check_oidc(oidc: &OidcSession) -> Result<(), ForgeError> {
    let fits = |s: &str| !s.is_empty() && s.chars().count() <= MAX_OIDC_CHARS && !s.contains('\0');
    let sid_ok = oidc.sid.as_deref().is_none_or(fits);
    if fits(&oidc.issuer) && fits(&oidc.subject) && sid_ok {
        Ok(())
    } else {
        // The ID token was verified (MR4) before this call: a caller bug.
        Err(internal("session_oidc_out_of_range"))
    }
}

/// Issue a session for `new.user`: one `session.created` audit row (ids and
/// the public id only).
///
/// The user's row is locked (`FOR UPDATE`), so a concurrent disable either
/// runs first (then this refuses) or after (then it revokes this session).
/// The caller decided admission first (`SessionCreate`, MR4); the rules
/// here are the backstop: an unknown user is `FG-REPO-0005`, a disabled
/// one `FG-AUTH-0004 reason=owner_disabled`, a bot or service account
/// `FG-SYS-0001 reason=session_for_non_human` (admission refuses them as
/// `NotHuman`); a lifetime outside 60 s ..= 24 h or idle above absolute is
/// `FG-SYS-0001 reason=session_lifetime_out_of_range`.
pub async fn issue(db: &Db, ctx: &ChangeContext, new: &NewSession) -> Result<Issued, ForgeError> {
    let user = new.user;
    let (idle_secs, absolute_secs) = lifetime_secs(new)?;
    if let Some(oidc) = &new.oidc {
        check_oidc(oidc)?;
    }
    let token = session_token::generate()
        .map_err(|e| internal("rng_unavailable").with_context("source", format!("{e:?}")))?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let owner: Option<(String, String)> = tx
            .step(|c| async move {
                sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR UPDATE")
                    .bind(user.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("sessions.issue.user", e))
            })
            .await?;
        let Some((kind, state)) = owner else {
            return Err(not_found("user"));
        };
        if state != "active" {
            return Err(ForgeError::new(ErrorCode::AccountDisabled)
                .with_context("reason", "owner_disabled")
                .with_context("user", user.get()));
        }
        if kind != UserKind::Human.as_str() {
            return Err(internal("session_for_non_human").with_context("user", user.get()));
        }
        let hash = forge_core::token::sha256(token.secret());
        let public_id = token.public_id().as_str();
        let oidc = new.oidc.as_ref();
        let (session_id, idle_at, absolute_at): (i64, String, String) = tx
            .step(|c| async move {
                sqlx::query_as(concat!(
                    "INSERT INTO forge.web_sessions (public_id, secret_sha256, user_id, \
                     idle_expires_at, absolute_expires_at, oidc_issuer, oidc_sid, oidc_sub, \
                     refresh_token_enc, id_token_enc, refresh_checked_at, access_role_seen_at) \
                     VALUES ($1, $2, $3, now() + make_interval(secs => $4), \
                     now() + make_interval(secs => $5), $6, $7, $8, $9, $10, \
                     CASE WHEN $6::text IS NULL THEN NULL ELSE now() END, \
                     CASE WHEN $6::text IS NULL THEN NULL ELSE now() END) \
                     RETURNING id, ",
                    utc!("idle_expires_at"),
                    ", ",
                    utc!("absolute_expires_at")
                ))
                .bind(public_id)
                .bind(hash.as_slice())
                .bind(user.get())
                .bind(idle_secs)
                .bind(absolute_secs)
                .bind(oidc.map(|o| o.issuer.as_str()))
                .bind(oidc.and_then(|o| o.sid.as_deref()))
                .bind(oidc.map(|o| o.subject.as_str()))
                .bind(oidc.and_then(|o| o.refresh_token_enc.as_deref()))
                .bind(oidc.and_then(|o| o.id_token_enc.as_deref()))
                .fetch_one(c)
                .await
                .map_err(|e| query_error("sessions.issue.insert", e))
            })
            .await?;
        let details = AuditDetails::SessionCreated {
            session_id,
            user,
            public_id: public_id.to_owned(),
        };
        Ok(Settle::Commit(
            ChangeRecord::audit_only(&details, RESOURCE, user.get()),
            (session_id, idle_at, absolute_at),
        ))
    }
    .await;
    let (session_id, idle_expires_at, absolute_expires_at) =
        tx.settle("session.created", outcome).await?;
    Ok(Issued {
        session_id,
        token,
        idle_expires_at,
        absolute_expires_at,
    })
}

/// Revoke one session with `reason`: one `session.revoked` audit row.
/// Revoking a revoked session is `AlreadyEnded` and writes nothing (an
/// expired one is still revoked: the reason is recorded). An unknown
/// session is `FG-REPO-0005` (`resource=session`, `reason=absent`).
///
/// It does not check who owns the session: an HTTP route decides with
/// `authorize()` first (`SessionRevoke`, MR6) and answers another user's
/// session as not found.
pub async fn revoke(
    db: &Db,
    ctx: &ChangeContext,
    session_id: i64,
    reason: SessionRevokeReason,
) -> Result<RevokeOutcome, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let row: Option<(i64, String, bool)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT user_id, public_id, revoked_at IS NOT NULL FROM forge.web_sessions \
                     WHERE id = $1 FOR UPDATE",
                )
                .bind(session_id)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("sessions.revoke.lock", e))
            })
            .await?;
        let Some((user_id, public_id, revoked)) = row else {
            return Err(not_found("session"));
        };
        if revoked {
            return Ok(Settle::Discard(RevokeOutcome::AlreadyEnded));
        }
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.web_sessions SET revoked_at = now(), revoked_reason = $2 \
                 WHERE id = $1",
            )
            .bind(session_id)
            .bind(reason.as_str())
            .execute(c)
            .await
            .map_err(|e| query_error("sessions.revoke.update", e))
        })
        .await?;
        let details = AuditDetails::SessionRevoked {
            session_id,
            user: UserId::new(user_id),
            public_id,
            reason,
        };
        Ok(Settle::Commit(
            ChangeRecord::audit_only(&details, RESOURCE, user_id),
            RevokeOutcome::Revoked,
        ))
    }
    .await;
    tx.settle("session.revoked", outcome).await
}

/// Revoke every active session of `user` with `reason`: one
/// `session.revoked_all` audit row with the count, or nothing at all when
/// the user has none (returns 0). An unknown user is `FG-REPO-0005`.
pub async fn revoke_all(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
    reason: SessionRevokeReason,
) -> Result<i64, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let known: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar("SELECT id FROM forge.users WHERE id = $1 FOR UPDATE")
                    .bind(user.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("sessions.revoke_all.user", e))
            })
            .await?;
        if known.is_none() {
            return Err(not_found("user"));
        }
        let revoked = tx
            .step(|c| revoke_active(c, user, reason, "sessions.revoke_all.update"))
            .await?;
        if revoked == 0 {
            return Ok(Settle::Discard(0));
        }
        let details = AuditDetails::SessionsRevoked {
            user,
            reason,
            revoked_sessions: revoked,
        };
        Ok(Settle::Commit(
            ChangeRecord::audit_only(&details, RESOURCE, user.get()),
            revoked,
        ))
    }
    .await;
    tx.settle("session.revoked_all", outcome).await
}

/// Revoke the user's active sessions inside the caller's change (the
/// caller writes the audit row: `revoke_all`, `users::disable`). Returns
/// how many. Expired sessions are left as they are (they no longer
/// authenticate).
pub(crate) async fn revoke_active(
    conn: &mut PgConnection,
    user: UserId,
    reason: SessionRevokeReason,
    query: &'static str,
) -> Result<i64, ForgeError> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "UPDATE forge.web_sessions SET revoked_at = now(), revoked_reason = $2 \
         WHERE user_id = $1 AND revoked_at IS NULL AND absolute_expires_at > now() \
         RETURNING id",
    )
    .bind(user.get())
    .bind(reason.as_str())
    .fetch_all(conn)
    .await
    .map_err(|e| query_error(query, e))?;
    Ok(i64::try_from(ids.len()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests;
