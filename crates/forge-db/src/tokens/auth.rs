//! `tokens::authenticate`: resolve a presented token on the write pool (the
//! read pool cannot see hashes, ADR-0018), compare in constant time, and
//! record `last_used_at` at most every 10 minutes without an audit row.

use forge_core::ids::UserId;
use forge_core::token::{self, TokenSecret};
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::PgConnection;

use super::{parse_scopes, TokenIdentity};
use crate::{query_error, Db};

/// Compared against when the public id is unknown, so a miss costs the
/// same hash and comparison as a hit (no timing signal for existence).
const DUMMY_HASH: [u8; 32] = [0u8; 32];

/// id, user_id, secret_sha256, scopes, is_agent, revoked, expired,
/// is_admin, owner active, last_used_at stale.
type Row = (
    i64,
    i64,
    Vec<u8>,
    Vec<String>,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
);

fn rejected(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", reason)
}

/// Resolve a presented token string.
///
/// `FG-AUTH-0001` (reason in the context, never in the user message):
/// `malformed`, `unknown`, `mismatch`, `revoked`, `expired` — checked in
/// that order, so the state of a token is told only to its secret's holder.
/// A valid token of a disabled owner is `FG-AUTH-0004`
/// (`reason=owner_disabled`) here, so no caller can forget that check, and
/// its use is not recorded.
/// `FG-DB-0001` when the database cannot be used (never a 401);
/// `FG-DB-0019` for a stored row this build cannot read. Updates
/// `last_used_at` at most every 10 minutes, unaudited; a failure of that
/// update is logged once (`FG-DB-0002`, warn) and authentication proceeds.
pub async fn authenticate(
    db: &Db,
    request_id: &RequestId,
    presented: &str,
) -> Result<TokenIdentity, ForgeError> {
    let plain = token::parse(presented).map_err(|_| rejected("malformed"))?;
    let public_id = plain.public_id().as_str();
    db.within("tokens.authenticate", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("tokens.authenticate.acquire", e))?;
        let row: Option<Row> = sqlx::query_as(
            "SELECT t.id, t.user_id, t.secret_sha256, t.scopes, t.is_agent, \
             t.revoked_at IS NOT NULL, t.expires_at <= now(), u.is_admin, u.state = 'active', \
             t.last_used_at IS NULL OR t.last_used_at < now() - interval '10 minutes' \
             FROM forge.tokens t JOIN forge.users u ON u.id = t.user_id \
             WHERE t.public_id = $1",
        )
        .bind(public_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| query_error("tokens.authenticate.lookup", e))?;
        let Some((id, user_id, hash, scopes, is_agent, revoked, expired, is_admin, active, stale)) =
            row
        else {
            // Equal work for a miss; black_box keeps the comparison from
            // being optimised away, since its result is deliberately unused.
            std::hint::black_box(token::verify(&DUMMY_HASH, plain.secret()));
            return Err(rejected("unknown"));
        };
        let matched = matches_hash(id, &hash, plain.secret())
            .map_err(|e| e.with_context("public_id", public_id))?;
        if !matched {
            return Err(rejected("mismatch").with_context("public_id", public_id));
        }
        if revoked {
            return Err(rejected("revoked").with_context("public_id", public_id));
        }
        if expired {
            return Err(rejected("expired").with_context("public_id", public_id));
        }
        if !active {
            return Err(ForgeError::new(ErrorCode::AccountDisabled)
                .with_context("reason", "owner_disabled")
                .with_context("user", user_id)
                .with_context("public_id", public_id));
        }
        let scopes = parse_scopes(id, public_id, &scopes)?;
        if stale {
            touch(&mut conn, request_id, id).await;
        }
        Ok(TokenIdentity {
            token_id: id,
            user_id: UserId::new(user_id),
            is_admin,
            scopes,
            is_agent,
        })
    })
    .await
}

/// Constant-time comparison with the stored hash; a hash that is not 32
/// bytes is `FG-DB-0019 reason=bad_hash` (the CHECK was bypassed).
fn matches_hash(token_id: i64, stored: &[u8], secret: &TokenSecret) -> Result<bool, ForgeError> {
    let stored: [u8; 32] = stored.try_into().map_err(|_| {
        ForgeError::new(ErrorCode::CredentialDataInvalid)
            .with_context("reason", "bad_hash")
            .with_context("token_id", token_id)
    })?;
    Ok(token::verify(&stored, secret))
}

/// Record the use. Losing it only makes `last_used_at` older, so a failure
/// is logged once here (the boundary of this side effect) and the request
/// proceeds; it is never silent.
async fn touch(conn: &mut PgConnection, request_id: &RequestId, token_id: i64) {
    let result = sqlx::query(
        "UPDATE forge.tokens SET last_used_at = now() WHERE id = $1 \
         AND (last_used_at IS NULL OR last_used_at < now() - interval '10 minutes')",
    )
    .bind(token_id)
    .execute(conn)
    .await;
    if let Err(e) = result {
        let e = query_error("tokens.authenticate.touch", e);
        tracing::warn!(
            target: "forge_db",
            request_id = %request_id,
            error_code = e.code().id(),
            token_id,
            error = %e,
            "recording the token's last use failed; authentication proceeds"
        );
    }
}
