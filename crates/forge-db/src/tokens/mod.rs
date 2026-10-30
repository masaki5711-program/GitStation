//! Personal access tokens (step D3, ADR-0019): issue, revoke, list and
//! authenticate. `issue` / `revoke` each write one audit row (attached to the
//! owner, resource `user`) and one domain event (`token.created` /
//! `token.revoked`, aggregate `token`, ADR-0016 §10). The secret is shown
//! once by the caller; only `sha256(secret)` is stored, and only the write
//! pool reads it (ADR-0018).

use std::time::Duration;

use forge_core::ids::UserId;
use forge_core::token::{self, PlainToken};
use forge_core::{AuditDetails, Scope, UserKind};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{ChangeContext, ChangeRecord, ChangeTx, DomainEvent};
use crate::Db;

/// The expiry when the request names none.
pub const DEFAULT_EXPIRY: Duration = Duration::from_secs(90 * 24 * 3600);
/// The shortest and longest expiry a request may name.
pub const MIN_EXPIRY: Duration = Duration::from_secs(24 * 3600);
pub const MAX_EXPIRY: Duration = Duration::from_secs(365 * 24 * 3600);
/// Active (not revoked, not expired) tokens one user may hold.
pub const MAX_ACTIVE_TOKENS: i64 = 50;
/// The longest token name, in characters.
const MAX_NAME_CHARS: usize = 100;

/// The audit resource type of token changes (tables.toml `audit_resource`):
/// the owner.
const RESOURCE: &str = "user";
/// The aggregate of the `token.*` events.
const AGGREGATE: &str = "token";

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

// Declared after `utc!` so the submodules can use it (textual macro scope).
mod auth;
mod list;

pub use auth::authenticate;
pub use list::list;

/// The request to issue a token.
#[derive(Debug, Clone)]
pub struct NewToken {
    pub user: UserId,
    /// 1..=100 characters, no NUL.
    pub name: String,
    pub scopes: Vec<Scope>,
    /// `None` = [`DEFAULT_EXPIRY`].
    pub expires_in: Option<Duration>,
    /// A token held by an AI agent (ADR-0017 §4.0).
    pub is_agent: bool,
}

/// What `issue` wrote. `token` is the only place the secret exists.
#[derive(Debug)]
pub struct Issued {
    pub token_id: i64,
    pub token: PlainToken,
    /// RFC 3339, UTC.
    pub expires_at: String,
}

/// What `revoke` did.
#[must_use = "AlreadyRevoked wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeOutcome {
    /// One `token.revoked` audit row and event.
    Revoked,
    /// Already revoked: nothing was written.
    AlreadyRevoked,
}

/// One token as `list` shows it: never the secret or its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    pub token_id: i64,
    pub public_id: String,
    pub name: String,
    pub scopes: Vec<Scope>,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub expires_at: String,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
}

/// Who a valid token stands for (defined in forge-core so forge-web can
/// receive it through `forge_core::Authenticator` without depending on
/// forge-db).
pub use forge_core::TokenIdentity;

fn request_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::TokenRequestInvalid).with_context("reason", reason)
}

fn not_found(resource: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", resource)
        .with_context("reason", "absent")
}

/// The longest stored value quoted in an error (it bypassed the CHECK and is
/// not trusted to be short).
const MAX_RAW_CHARS: usize = 64;

/// Stored scope names back to `Scope`; a name this build does not know is
/// `FG-DB-0019 reason=unknown_scope` (the CHECK was bypassed).
pub(crate) fn parse_scopes(
    token_id: i64,
    public_id: &str,
    names: &[String],
) -> Result<Vec<Scope>, ForgeError> {
    names
        .iter()
        .map(|name| {
            Scope::parse(name).ok_or_else(|| {
                ForgeError::new(ErrorCode::CredentialDataInvalid)
                    .with_context("reason", "unknown_scope")
                    .with_context("token_id", token_id)
                    .with_context("public_id", public_id)
                    .with_context("raw", name.chars().take(MAX_RAW_CHARS).collect::<String>())
            })
        })
        .collect()
}

/// Invisible format characters (Unicode `Cf`: bidi overrides and isolates,
/// zero-width characters, the BOM, the soft hyphen) that make a name look
/// different from what it is.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
    )
}

/// The rules checked before anything is read or written. Returns the
/// expiry in whole seconds and the scopes without repeats (first wins).
fn validate(new: &NewToken) -> Result<(i64, Vec<Scope>), ForgeError> {
    let expires_in = new.expires_in.unwrap_or(DEFAULT_EXPIRY);
    if expires_in < MIN_EXPIRY || expires_in > MAX_EXPIRY {
        return Err(request_invalid("expires_out_of_range"));
    }
    let len = new.name.chars().count();
    if len == 0 || len > MAX_NAME_CHARS {
        return Err(request_invalid("name_length").with_context("len", len));
    }
    // PostgreSQL text cannot hold NUL; refuse it here, not as a query error.
    if new.name.contains('\0') {
        return Err(request_invalid("nul_character"));
    }
    // Tabs, newlines and escapes would forge rows or recolour the operator's
    // terminal in `token list`; invisible format characters would disguise it.
    if new
        .name
        .chars()
        .any(|c| c.is_control() || is_invisible_format(c))
    {
        return Err(request_invalid("control_character"));
    }
    if new.scopes.is_empty() {
        return Err(request_invalid("scopes_empty"));
    }
    let mut scopes = Vec::with_capacity(new.scopes.len());
    for scope in &new.scopes {
        if !scopes.contains(scope) {
            scopes.push(*scope);
        }
    }
    // MAX_EXPIRY fits easily: 365 days in seconds.
    let secs =
        i64::try_from(expires_in.as_secs()).map_err(|_| request_invalid("expires_out_of_range"))?;
    Ok((secs, scopes))
}

/// Issue a token for `new.user`: one `token.created` audit row and one
/// `token.created` event (payload: token_id, public_id, user_id, scopes,
/// expires_at, is_agent; never the secret or hash).
///
/// The owner's row is locked (`FOR UPDATE`) for the whole change, so
/// concurrent issues for one user — and a concurrent disable — run one
/// after another and the active-token limit holds.
///
/// Errors (nothing is written): `FG-AUTH-0008` with `reason` =
/// `expires_out_of_range`, `scopes_empty`, `name_length`, `nul_character`,
/// `control_character`,
/// `owner_bot`, `owner_disabled`, `admin_scope_requires_admin` or
/// `active_limit`; `FG-REPO-0005` for an unknown owner.
pub async fn issue(db: &Db, ctx: &ChangeContext, new: &NewToken) -> Result<Issued, ForgeError> {
    let user = new.user;
    // Every refusal names the owner (FG-AUTH-0008 diagnosis).
    let refuse = |reason: &'static str| request_invalid(reason).with_context("user", user.get());
    let (expires_secs, scopes) = validate(new).map_err(|e| e.with_context("user", user.get()))?;
    let plain = token::generate().map_err(|e| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "rng_unavailable")
            .with_context("source", format!("{e:?}"))
    })?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let owner: Option<(String, String, bool)> = tx
        .step(|c| async move {
            sqlx::query_as("SELECT kind, state, is_admin FROM forge.users WHERE id = $1 FOR UPDATE")
                .bind(user.get())
                .fetch_optional(c)
                .await
                .map_err(|e| crate::query_error("tokens.issue.owner", e))
        })
        .await?;
    let Some((kind, state, is_admin)) = owner else {
        return Err(not_found("user"));
    };
    if kind == UserKind::Bot.as_str() {
        return Err(refuse("owner_bot"));
    }
    if state != "active" {
        return Err(refuse("owner_disabled"));
    }
    if scopes.contains(&Scope::Admin) && !is_admin {
        return Err(refuse("admin_scope_requires_admin"));
    }
    let active: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "SELECT count(*) FROM forge.tokens \
                 WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now()",
            )
            .bind(user.get())
            .fetch_one(c)
            .await
            .map_err(|e| crate::query_error("tokens.issue.count", e))
        })
        .await?;
    if active >= MAX_ACTIVE_TOKENS {
        return Err(refuse("active_limit").with_context("limit", MAX_ACTIVE_TOKENS));
    }
    let names: Vec<&'static str> = scopes.iter().map(|s| s.as_str()).collect();
    let hash = token::sha256(plain.secret());
    let public_id = plain.public_id().as_str();
    let (token_id, expires_at): (i64, String) = tx
        .step(|c| async move {
            sqlx::query_as(concat!(
                "INSERT INTO forge.tokens (user_id, public_id, secret_sha256, name, scopes, \
                 expires_at, is_agent) \
                 VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6), $7) \
                 RETURNING id, ",
                utc!("expires_at")
            ))
            .bind(user.get())
            .bind(public_id)
            .bind(hash.as_slice())
            .bind(new.name.as_str())
            .bind(&names)
            .bind(expires_secs as f64)
            .bind(new.is_agent)
            .fetch_one(c)
            .await
            .map_err(|e| crate::query_error("tokens.issue.insert", e))
        })
        .await?;
    let details = AuditDetails::TokenCreated {
        token_id,
        user,
        public_id: public_id.to_owned(),
        scopes,
        expires_at: expires_at.clone(), // clone: the audit details and the result both keep it
        is_agent: new.is_agent,
    };
    tx.commit(with_event(&details, user.get(), token_id)?)
        .await?
        .require_recorded(details.action())?;
    Ok(Issued {
        token_id,
        token: plain,
        expires_at,
    })
}

/// The change record of a token change: the audit row on the owner and one
/// event with the same safe fields.
fn with_event(
    details: &AuditDetails,
    user_id: i64,
    token_id: i64,
) -> Result<ChangeRecord, ForgeError> {
    let mut record = ChangeRecord::audit_only(details, RESOURCE, user_id);
    record.events.push(event(details, token_id)?);
    Ok(record)
}

/// The `token.*` event of `details` (instance-wide, admin class). Its type
/// is `details.event_type()`, so the event and the audit action cannot
/// disagree; details of an audit-only action are `FG-SYS-0001
/// reason=audit_only_event` (a bug in the caller).
pub(crate) fn event(details: &AuditDetails, token_id: i64) -> Result<DomainEvent, ForgeError> {
    let event_type = details.event_type().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "audit_only_event")
            .with_context("action", details.action())
    })?;
    Ok(DomainEvent {
        event_type,
        schema_version: 1,
        aggregate_type: AGGREGATE,
        aggregate_id: token_id,
        project_id: None,
        payload: details.to_value(),
        dedup_key: None,
    })
}

/// Revoke a token (`revoked_reason = 'revoked'`): one `token.revoked` audit
/// row and one `token.revoked` event. Revoking a revoked token is
/// `AlreadyRevoked` and writes nothing; an unknown token is `FG-REPO-0005`
/// (`resource=token`, `reason=absent`).
///
/// Operator-only: it does not check who owns the token (the CLI is the trust
/// root, ADR-0019 §8). An HTTP route must first decide with `authorize()`
/// and refuse another user's token as not found before calling it.
pub async fn revoke(
    db: &Db,
    ctx: &ChangeContext,
    token_id: i64,
) -> Result<RevokeOutcome, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let row: Option<(i64, String, bool)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT user_id, public_id, revoked_at IS NOT NULL FROM forge.tokens \
                 WHERE id = $1 FOR UPDATE",
            )
            .bind(token_id)
            .fetch_optional(c)
            .await
            .map_err(|e| crate::query_error("tokens.revoke.lock", e))
        })
        .await?;
    let Some((user_id, public_id, revoked)) = row else {
        return Err(not_found(AGGREGATE));
    };
    if revoked {
        // Dropping the transaction rolls back the lock; nothing was written.
        return Ok(RevokeOutcome::AlreadyRevoked);
    }
    tx.step(|c| async move {
        sqlx::query(
            "UPDATE forge.tokens SET revoked_at = now(), revoked_reason = 'revoked' WHERE id = $1",
        )
        .bind(token_id)
        .execute(c)
        .await
        .map_err(|e| crate::query_error("tokens.revoke.update", e))
    })
    .await?;
    let details = AuditDetails::TokenRevoked {
        token_id,
        user: UserId::new(user_id),
        public_id,
    };
    tx.commit(with_event(&details, user_id, token_id)?)
        .await?
        .require_recorded(details.action())?;
    Ok(RevokeOutcome::Revoked)
}

#[cfg(test)]
mod tests;
