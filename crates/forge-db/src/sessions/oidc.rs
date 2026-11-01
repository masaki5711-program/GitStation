//! The identity provider's side of a web session (M2 MR5, ADR-0021 §7,
//! §8): the 5-minute refresh check's claim and result, the RP logout's ID
//! token, and the back-channel logout's sessions. Every write here touches
//! the table's unaudited columns only (`refresh_*`, `id_token_enc`,
//! `access_role_seen_at`); ending a session is always [`super::revoke`]
//! (one `session.revoked` audit row each).

use forge_core::ids::UserId;
use forge_core::UserKind;
use forge_errors::ForgeError;

use super::{internal, not_found};
use crate::identities::LoginAccount;
use crate::{query_error, Db};

/// The right to refresh one session now, taken by [`claim_refresh`]. No
/// `Debug` of the sealed token's bytes.
pub struct RefreshClaim {
    pub session_id: i64,
    pub public_id: String,
    pub user: UserId,
    /// `oidc_issuer` / `oidc_sub` of the row (the refreshed ID token must
    /// name the same, ADR-0021 §7).
    pub issuer: String,
    pub subject: String,
    /// The sealed refresh token (purpose `refresh-token`, the public id as
    /// associated data).
    pub refresh_token_enc: Vec<u8>,
    /// Refreshes in a row whose answer had no ID token (0 after one with).
    pub without_id_token: i16,
}

impl std::fmt::Debug for RefreshClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshClaim")
            .field("session_id", &self.session_id)
            .field("public_id", &self.public_id)
            .field("user", &self.user)
            .field("without_id_token", &self.without_id_token)
            .finish_non_exhaustive()
    }
}

#[derive(sqlx::FromRow)]
struct ClaimRow {
    public_id: String,
    user_id: i64,
    oidc_issuer: Option<String>,
    oidc_sub: Option<String>,
    refresh_token_enc: Option<Vec<u8>>,
    refresh_without_id_token: i16,
}

/// Take the right to refresh `session_id` (ADR-0021 §7, "once per session
/// at a time"): one conditional `UPDATE … SET refresh_checked_at = now()`
/// that only the first request after 5 minutes wins. `None`: another
/// request holds it, the check is not due, or the session is revoked or has
/// no refresh token — the request goes on without refreshing.
///
/// `FG-DB-0001` / `FG-DB-0002` when the database fails; `FG-DB-0019
/// reason=oidc_columns_missing` for a claimed row without its issuer or
/// subject (the table's CHECKs make that impossible).
pub async fn claim_refresh(db: &Db, session_id: i64) -> Result<Option<RefreshClaim>, ForgeError> {
    let row: Option<ClaimRow> = db
        .within("sessions.claim_refresh", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("sessions.claim_refresh.acquire", e))?;
            sqlx::query_as(
                "UPDATE forge.web_sessions SET refresh_checked_at = now() \
                 WHERE id = $1 AND revoked_at IS NULL AND refresh_token_enc IS NOT NULL \
                 AND absolute_expires_at > now() \
                 AND (refresh_checked_at IS NULL \
                      OR refresh_checked_at < now() - interval '300 seconds') \
                 RETURNING public_id, user_id, oidc_issuer, oidc_sub, refresh_token_enc, \
                 refresh_without_id_token",
            )
            .bind(session_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| query_error("sessions.claim_refresh.update", e))
        })
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    match (row.oidc_issuer, row.oidc_sub, row.refresh_token_enc) {
        (Some(issuer), Some(subject), Some(refresh_token_enc)) => Ok(Some(RefreshClaim {
            session_id,
            public_id: row.public_id,
            user: UserId::new(row.user_id),
            issuer,
            subject,
            refresh_token_enc,
            without_id_token: row.refresh_without_id_token,
        })),
        _ => Err(
            ForgeError::new(forge_errors::ErrorCode::CredentialDataInvalid)
                .with_context("reason", "oidc_columns_missing")
                .with_context("session_id", session_id),
        ),
    }
}

/// The account of `user` as the refresh check's `SessionContinue` reads it
/// again (ADR-0021 §7): `Active`, `Disabled` or `NotHuman` (never `New`:
/// the session's user exists). An unknown user is `FG-REPO-0005`.
pub async fn account_of(db: &Db, user: UserId) -> Result<LoginAccount, ForgeError> {
    let row: Option<(String, String)> = db
        .within("sessions.account_of", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("sessions.account_of.acquire", e))?;
            sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1")
                .bind(user.get())
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| query_error("sessions.account_of.select", e))
        })
        .await?;
    let Some((kind, state)) = row else {
        return Err(not_found("user"));
    };
    Ok(if state != "active" {
        LoginAccount::Disabled
    } else if kind != UserKind::Human.as_str() {
        LoginAccount::NotHuman
    } else {
        LoginAccount::Active
    })
}

/// What a successful refresh stores.
#[derive(Clone, Copy)]
pub struct Refreshed<'a> {
    /// The sealed new refresh token, when the provider rotated it (`None`
    /// keeps the stored one).
    pub refresh_token_enc: Option<&'a [u8]>,
    /// The sealed new ID token (`None`: the answer had none; the stored
    /// one stays for the RP logout's `id_token_hint`).
    pub id_token_enc: Option<&'a [u8]>,
}

/// Store a refresh the check allowed: the new sealed tokens, and either
/// `access_role_seen_at = now()` with the count of refreshes without an ID
/// token reset to 0 (an ID token confirmed the role), or that count raised
/// by one. `false`: the session was revoked meanwhile (nothing written).
/// Unaudited columns only.
pub async fn store_refreshed(
    db: &Db,
    session_id: i64,
    refreshed: Refreshed<'_>,
) -> Result<bool, ForgeError> {
    let with_id_token = refreshed.id_token_enc.is_some();
    let stored = db
        .within("sessions.store_refreshed", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("sessions.store_refreshed.acquire", e))?;
            sqlx::query(
                "UPDATE forge.web_sessions SET \
                 refresh_token_enc = COALESCE($2, refresh_token_enc), \
                 id_token_enc = COALESCE($3, id_token_enc), \
                 access_role_seen_at = CASE WHEN $4 THEN now() ELSE access_role_seen_at END, \
                 refresh_without_id_token = CASE WHEN $4 THEN 0 \
                     ELSE LEAST(refresh_without_id_token + 1, 1000) END \
                 WHERE id = $1 AND revoked_at IS NULL",
            )
            .bind(session_id)
            .bind(refreshed.refresh_token_enc)
            .bind(refreshed.id_token_enc)
            .bind(with_id_token)
            .execute(&mut *conn)
            .await
            .map_err(|e| query_error("sessions.store_refreshed.update", e))
        })
        .await?;
    Ok(stored.rows_affected() == 1)
}

/// The RP logout's view of a live session: its public id (the sealed ID
/// token's associated data) and the sealed ID token, if any. `None`: the
/// session is revoked or gone.
pub async fn logout_view(
    db: &Db,
    session_id: i64,
) -> Result<Option<(String, Option<Vec<u8>>)>, ForgeError> {
    db.within("sessions.logout_view", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("sessions.logout_view.acquire", e))?;
        sqlx::query_as(
            "SELECT public_id, id_token_enc FROM forge.web_sessions \
             WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(session_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| query_error("sessions.logout_view.select", e))
    })
    .await
}

/// Which IdP session a back-channel logout names (ADR-0021 §8).
#[derive(Debug, Clone, Copy)]
pub enum LogoutTarget<'a> {
    /// The token's `sid`: the rows with this `oidc_sid`.
    Sid(&'a str),
    /// No `sid`: every row of this `oidc_sub`.
    Subject(&'a str),
}

/// The unrevoked sessions of `issuer` the target names (never a row of
/// another identity provider with the same `sid` / `sub`). The caller
/// revokes each with [`super::revoke`]; at most 1 000 (a user has a few).
pub async fn backchannel_targets(
    db: &Db,
    issuer: &str,
    target: LogoutTarget<'_>,
) -> Result<Vec<i64>, ForgeError> {
    let (sid, sub) = match target {
        LogoutTarget::Sid(sid) => (Some(sid), None),
        LogoutTarget::Subject(sub) => (None, Some(sub)),
    };
    if sid.or(sub).is_none_or(str::is_empty) || issuer.is_empty() {
        return Err(internal("backchannel_target_empty"));
    }
    db.within("sessions.backchannel_targets", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("sessions.backchannel_targets.acquire", e))?;
        sqlx::query_scalar(
            "SELECT id FROM forge.web_sessions \
             WHERE oidc_issuer = $1 AND revoked_at IS NULL \
             AND (($2::text IS NOT NULL AND oidc_sid = $2) \
                  OR ($2::text IS NULL AND oidc_sub = $3)) \
             ORDER BY id LIMIT 1000",
        )
        .bind(issuer)
        .bind(sid)
        .bind(sub)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| query_error("sessions.backchannel_targets.select", e))
    })
    .await
}
