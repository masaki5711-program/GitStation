//! `sessions::list` and `sessions::owner` (M2 MR6, ADR-0021 §17): the
//! reads behind `GET /api/v1/user/sessions`, `GET
//! /api/v1/admin/users/{id}/sessions` and the loader of `DELETE
//! /api/v1/user/sessions/{public_id}`. Both on the read pool, which never
//! sees a hash, a ciphertext or the IdP's subject (ADR-0018, tables.toml).

use forge_core::ids::UserId;
use forge_errors::{ErrorCode, ForgeError};

use crate::{read_query_error, Db};

/// The most sessions one list shows (a person rarely holds more than a
/// few; past it the oldest are left out and `truncated` says so).
pub const MAX_LISTED: i64 = 100;

/// One active web session as the list shows it: never the secret, its
/// hash, the IdP's tokens or subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// The session's public id (the `{public_id}` of its revoke route).
    pub public_id: String,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub last_seen_at: String,
    pub idle_expires_at: String,
    pub absolute_expires_at: String,
    /// Signed in through the OIDC IdP (`oidc_issuer` set).
    pub oidc: bool,
}

/// What [`list`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionList {
    /// Newest first, at most [`MAX_LISTED`].
    pub sessions: Vec<SessionInfo>,
    /// More active sessions exist than are shown.
    pub truncated: bool,
}

type ListRow = (String, String, String, String, String, bool);

/// The user's **active** sessions (not revoked, neither lifetime over),
/// newest first: one statement on the read pool.
pub async fn list(db: &Db, user: UserId) -> Result<SessionList, ForgeError> {
    let rows: Vec<ListRow> = db
        .within("sessions.list", async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(concat!(
                "SELECT public_id, ",
                utc!("created_at"),
                ", ",
                utc!("last_seen_at"),
                ", ",
                utc!("idle_expires_at"),
                ", ",
                utc!("absolute_expires_at"),
                ", oidc_issuer IS NOT NULL FROM forge.web_sessions \
                 WHERE user_id = $1 AND revoked_at IS NULL AND idle_expires_at > now() \
                 AND absolute_expires_at > now() \
                 ORDER BY created_at DESC, id DESC LIMIT $2"
            ))
            .bind(user.get())
            .bind(MAX_LISTED + 1)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error("sessions.list", e))
        })
        .await?;
    let shown = usize::try_from(MAX_LISTED).unwrap_or(usize::MAX);
    let truncated = rows.len() > shown;
    let sessions = rows
        .into_iter()
        .take(shown)
        .map(
            |(public_id, created_at, last_seen_at, idle_expires_at, absolute_expires_at, oidc)| {
                SessionInfo {
                    public_id,
                    created_at,
                    last_seen_at,
                    idle_expires_at,
                    absolute_expires_at,
                    oidc,
                }
            },
        )
        .collect();
    Ok(SessionList {
        sessions,
        truncated,
    })
}

/// The session `public_id` names: its row id and owner (`None`: no such
/// session, or not a well-formed public id — the same answer). Read pool,
/// one statement whatever the value (a malformed one is never sent: the
/// caller validated its shape).
pub async fn owner(db: &Db, public_id: &str) -> Result<Option<(i64, UserId)>, ForgeError> {
    if !is_public_id(public_id) {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "session_public_id_unchecked"));
    }
    let row: Option<(i64, i64)> = db
        .within("sessions.owner", async {
            let mut conn = db.acquire().await?;
            sqlx::query_as("SELECT id, user_id FROM forge.web_sessions WHERE public_id = $1")
                .bind(public_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| read_query_error("sessions.owner", e))
        })
        .await?;
    Ok(row.map(|(id, user)| (id, UserId::new(user))))
}

/// A session public id: 16 lowercase hex digits (`web_sessions_public_id_hex`).
pub fn is_public_id(raw: &str) -> bool {
    raw.len() == 16 && raw.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
