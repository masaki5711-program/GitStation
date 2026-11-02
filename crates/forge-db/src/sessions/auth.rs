//! `sessions::authenticate`: resolve a presented session cookie on the
//! write pool (the read pool cannot see hashes, ADR-0018), compare in
//! constant time, and record activity at most once a minute without an
//! audit row (ADR-0021 §5).

use std::time::Duration;

use forge_core::authenticator::{SessionAuthFuture, SessionIdentity};
use forge_core::ids::UserId;
use forge_core::session_token;
use forge_core::token::{self, TokenSecret};
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::PgConnection;

use crate::{query_error, Db};

/// Compared against when the public id is unknown, so a miss costs the
/// same hash and comparison as a hit (no timing signal for existence).
pub(super) const DUMMY_HASH: [u8; 32] = [0u8; 32];

/// The session and its user, as one statement reads them.
#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    user_id: i64,
    secret_sha256: Vec<u8>,
    revoked: bool,
    revoked_reason: Option<String>,
    expired: bool,
    stale: bool,
    refresh_due: bool,
    is_admin: bool,
    active: bool,
    username: String,
    display_name: String,
    idle_expires_at: String,
    absolute_expires_at: String,
}

fn rejected(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", reason)
}

/// Resolve a presented session cookie value; `idle` is
/// `web.session_idle_secs` (the touch moves `idle_expires_at` to now +
/// idle, never past the absolute expiry).
///
/// Checked in this order, so a session's state is told only to its secret's
/// holder: `FG-AUTH-0001` `session_malformed`, `session_unknown`,
/// `session_mismatch`; a disabled user is `FG-AUTH-0004
/// reason=owner_disabled` (ADR-0019 Amendment M2: "the next request after a
/// disable"; its sessions are revoked too); then `session_revoked` (with
/// `revoked_reason`) and `session_expired` (idle or absolute).
/// `FG-DB-0001` when the database cannot be used (never a 401);
/// `FG-DB-0019 reason=bad_hash` for a stored hash that is not 32 bytes.
/// A failed touch is logged once (`FG-DB-0002`, warn) and authentication
/// proceeds.
pub async fn authenticate(
    db: &Db,
    request_id: &RequestId,
    presented: &str,
    idle: Duration,
) -> Result<SessionIdentity, ForgeError> {
    authenticate_with(db, request_id, presented, idle, TouchPolicy::Always).await
}

/// When [`authenticate_with`] records activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPolicy {
    /// Whenever the last record is older than a minute.
    Always,
    /// The same, except for a session whose refresh check is due
    /// (`refresh_due`): the check decides first (`SessionContinue`, M2 MR5)
    /// and its caller records the activity with [`touch_session`] only when
    /// the session goes on — a session about to be revoked is never
    /// extended.
    AfterRefreshCheck,
}

/// [`authenticate`] with an explicit [`TouchPolicy`].
pub async fn authenticate_with(
    db: &Db,
    request_id: &RequestId,
    presented: &str,
    idle: Duration,
    policy: TouchPolicy,
) -> Result<SessionIdentity, ForgeError> {
    let plain = session_token::parse(presented).map_err(|_| rejected("session_malformed"))?;
    let public_id = plain.public_id().as_str();
    db.within("sessions.authenticate", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("sessions.authenticate.acquire", e))?;
        let row: Option<Row> = sqlx::query_as(concat!(
            "SELECT s.id, s.user_id, s.secret_sha256, s.revoked_at IS NOT NULL AS revoked, \
             s.revoked_reason, \
             (s.absolute_expires_at <= now() OR s.idle_expires_at <= now()) AS expired, \
             s.last_seen_at < now() - interval '1 minute' AS stale, \
             (s.oidc_issuer IS NOT NULL AND s.refresh_token_enc IS NOT NULL \
              AND (s.refresh_checked_at IS NULL \
                   OR s.refresh_checked_at < now() - interval '300 seconds')) AS refresh_due, \
             u.is_admin, u.state = 'active' AS active, u.username, u.display_name, ",
            utc!("s.idle_expires_at"),
            " AS idle_expires_at, ",
            utc!("s.absolute_expires_at"),
            " AS absolute_expires_at \
             FROM forge.web_sessions s JOIN forge.users u ON u.id = s.user_id \
             WHERE s.public_id = $1"
        ))
        .bind(public_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| query_error("sessions.authenticate.lookup", e))?;
        let Some(row) = row else {
            // Equal work for a miss; black_box keeps the comparison from
            // being optimised away, since its result is deliberately unused.
            std::hint::black_box(token::verify(&DUMMY_HASH, plain.secret()));
            return Err(rejected("session_unknown"));
        };
        let matched = matches_hash(row.id, &row.secret_sha256, plain.secret())
            .map_err(|e| e.with_context("public_id", public_id))?;
        if !matched {
            return Err(rejected("session_mismatch").with_context("public_id", public_id));
        }
        if !row.active {
            return Err(ForgeError::new(ErrorCode::AccountDisabled)
                .with_context("reason", "owner_disabled")
                .with_context("user", row.user_id)
                .with_context("public_id", public_id));
        }
        if row.revoked {
            return Err(rejected("session_revoked")
                .with_context("public_id", public_id)
                .with_context(
                    "revoked_reason",
                    row.revoked_reason.as_deref().unwrap_or("unknown"),
                ));
        }
        if row.expired {
            return Err(rejected("session_expired").with_context("public_id", public_id));
        }
        let mut idle_expires_at = row.idle_expires_at;
        let deferred = policy == TouchPolicy::AfterRefreshCheck && row.refresh_due;
        if row.stale && !deferred {
            if let Some(moved) = touch(&mut conn, request_id, row.id, idle).await {
                idle_expires_at = moved;
            }
        }
        Ok(SessionIdentity {
            session_id: row.id,
            public_id: public_id.to_owned(),
            user_id: UserId::new(row.user_id),
            username: row.username,
            display_name: row.display_name,
            is_admin: row.is_admin,
            idle_expires_at,
            absolute_expires_at: row.absolute_expires_at,
            refresh_due: row.refresh_due,
        })
    })
    .await
}

/// Constant-time comparison with the stored hash; a hash that is not 32
/// bytes is `FG-DB-0019 reason=bad_hash` (the CHECK was bypassed).
pub(super) fn matches_hash(
    session_id: i64,
    stored: &[u8],
    secret: &TokenSecret,
) -> Result<bool, ForgeError> {
    let stored: [u8; 32] = stored.try_into().map_err(|_| {
        ForgeError::new(ErrorCode::CredentialDataInvalid)
            .with_context("reason", "bad_hash")
            .with_context("session_id", session_id)
    })?;
    Ok(token::verify(&stored, secret))
}

/// Record the activity: `last_seen_at = now()` and `idle_expires_at = now()
/// + idle` (never past the absolute expiry), at most once a minute
///   (conditional: a concurrent request's touch wins, this one changes
///   nothing). Returns the new idle expiry. Losing it only ends the session
///   earlier, so a failure is logged once here (the boundary of this side
///   effect) and the request proceeds; it is never silent.
async fn touch(
    conn: &mut PgConnection,
    request_id: &RequestId,
    session_id: i64,
    idle: Duration,
) -> Option<String> {
    // At most 86 400 (validated at load): exact as f64.
    let idle_secs = idle.as_secs() as f64;
    let result: Result<Option<String>, sqlx::Error> = sqlx::query_scalar(concat!(
        "UPDATE forge.web_sessions SET last_seen_at = now(), \
         idle_expires_at = LEAST(now() + make_interval(secs => $2), absolute_expires_at) \
         WHERE id = $1 AND revoked_at IS NULL AND last_seen_at < now() - interval '1 minute' \
         RETURNING ",
        utc!("idle_expires_at")
    ))
    .bind(session_id)
    .bind(idle_secs)
    .fetch_optional(conn)
    .await;
    match result {
        Ok(moved) => moved,
        Err(e) => {
            let e = query_error("sessions.authenticate.touch", e);
            tracing::warn!(
                target: "forge_db",
                request_id = %request_id,
                error_code = e.code().id(),
                session_id,
                error = %e,
                "recording the session's activity failed; authentication proceeds"
            );
            None
        }
    }
}

/// Record the activity of a session whose touch [`authenticate_with`]
/// deferred ([`TouchPolicy::AfterRefreshCheck`]), after its refresh check
/// let it go on: at most once a minute, never past the absolute expiry.
/// Returns the new idle expiry, `None` when nothing moved. A failure is
/// logged once here (`FG-DB-0002` / `FG-DB-0001`, warn) and the request
/// proceeds, as for the touch inside authentication.
pub async fn touch_session(
    db: &Db,
    request_id: &RequestId,
    session_id: i64,
    idle: Duration,
) -> Option<String> {
    let acquired = db
        .within("sessions.touch_session", async {
            db.write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("sessions.touch_session.acquire", e))
        })
        .await;
    match acquired {
        Ok(mut conn) => touch(&mut conn, request_id, session_id, idle).await,
        Err(e) => {
            tracing::warn!(
                target: "forge_db",
                request_id = %request_id,
                error_code = e.code().id(),
                session_id,
                error = %e,
                "recording the session's activity failed; authentication proceeds"
            );
            None
        }
    }
}

/// The HTTP layer's session authenticator (`forge serve` injects it as
/// `forge_web::Sessions`): [`authenticate`] with the configured idle
/// lifetime.
#[derive(Clone)]
pub struct SessionStore {
    db: Db,
    idle: Duration,
    policy: TouchPolicy,
}

impl SessionStore {
    pub fn new(db: Db, idle: Duration) -> Self {
        Self {
            db,
            idle,
            policy: TouchPolicy::Always,
        }
    }

    /// Leave the touch of a session whose refresh check is due to the
    /// check (`forge_oidc::RefreshingSessions` wraps this store; M2 MR5).
    #[must_use]
    pub fn deferring_touch_to_refresh_check(mut self) -> Self {
        self.policy = TouchPolicy::AfterRefreshCheck;
        self
    }
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("idle", &self.idle)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl forge_core::SessionAuthenticator for SessionStore {
    fn authenticate_session<'a>(
        &'a self,
        request_id: &'a RequestId,
        presented: &'a str,
    ) -> SessionAuthFuture<'a> {
        Box::pin(authenticate_with(
            &self.db,
            request_id,
            presented,
            self.idle,
            self.policy,
        ))
    }
}
