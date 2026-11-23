//! Delegations of approvals (ADR-0038 §3.5, step C): a person names one
//! delegate and a period of at most 30 days; during it the delegate may
//! approve on the person's behalf (merge requests always, deployments where
//! the environment allows it). At most one delegation per person (a new
//! one replaces it); one level only (a delegate never passes it on: an
//! approval names exactly one delegator, never a chain).
//!
//! Set and removed by the person (`approval_delegation.set` /
//! `.removed`, audit only, on the delegator). Read on the read pool, and
//! inside an approval's transaction ([`active_on`], `FOR SHARE`: a removal
//! committing meanwhile waits, then the approval sees it gone).

use forge_core::ids::UserId;
use forge_core::{AuditDetails, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::{query_error, read_query_error, Db};

/// The audit resource (tables.toml): the delegator.
const RESOURCE: &str = "user";

const UTC: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

/// One delegation as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDelegation {
    pub delegator: UserId,
    pub delegator_username: String,
    pub delegator_display_name: String,
    pub delegate: UserId,
    pub delegate_username: String,
    pub delegate_display_name: String,
    /// UTC, RFC 3339.
    pub starts_at: String,
    pub ends_at: String,
    /// `starts_at <= now() < ends_at`.
    pub active: bool,
}

/// What a set or a remove did.
#[must_use = "Unchanged / Absent wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationOutcome {
    Set,
    /// The same delegate and period were already set: nothing written.
    Unchanged,
    Removed,
    /// No delegation to remove: nothing written.
    Absent,
}

type Row = (
    i64,
    String,
    String,
    i64,
    String,
    String,
    String,
    String,
    bool,
);

fn columns() -> String {
    format!(
        "d.delegator_id, a.username, a.display_name, d.delegate_id, b.username, b.display_name, \
         to_char(d.starts_at AT TIME ZONE 'UTC', {UTC}), \
         to_char(d.ends_at AT TIME ZONE 'UTC', {UTC}), \
         (d.starts_at <= now() AND now() < d.ends_at) \
         FROM forge.approval_delegations d \
         JOIN forge.users a ON a.id = d.delegator_id \
         JOIN forge.users b ON b.id = d.delegate_id"
    )
}

fn of(r: Row) -> StoredDelegation {
    StoredDelegation {
        delegator: UserId::new(r.0),
        delegator_username: r.1,
        delegator_display_name: r.2,
        delegate: UserId::new(r.3),
        delegate_username: r.4,
        delegate_display_name: r.5,
        starts_at: r.6,
        ends_at: r.7,
        active: r.8,
    }
}

/// `delegator`'s delegation (read pool; `None`: none set).
pub async fn delegation_of(
    db: &Db,
    delegator: UserId,
) -> Result<Option<StoredDelegation>, ForgeError> {
    const QUERY: &str = "review.delegation_of";
    let row: Option<Row> = db
        .within(QUERY, async {
            sqlx::query_as(&format!("SELECT {} WHERE d.delegator_id = $1", columns()))
                .bind(delegator.get())
                .fetch_optional(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(of))
}

/// The delegations naming `delegate` that have not ended (active or
/// upcoming), by delegator — at most 100 (read pool).
pub async fn delegations_to(
    db: &Db,
    delegate: UserId,
) -> Result<Vec<StoredDelegation>, ForgeError> {
    const QUERY: &str = "review.delegations_to";
    let rows: Vec<Row> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} WHERE d.delegate_id = $1 AND d.ends_at > now() \
                 AND a.state = 'active' ORDER BY a.username LIMIT 100",
                columns()
            ))
            .bind(delegate.get())
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows.into_iter().map(of).collect())
}

/// Whether `delegator` (active, a person) delegates to `delegate` now, on
/// `conn` (`FOR SHARE` of the delegation row).
pub(crate) async fn active_on(
    conn: &mut PgConnection,
    delegator: UserId,
    delegate: UserId,
) -> Result<bool, ForgeError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM forge.approval_delegations d \
           JOIN forge.users u ON u.id = d.delegator_id \
         WHERE d.delegator_id = $1 AND d.delegate_id = $2 \
           AND d.starts_at <= now() AND now() < d.ends_at \
           AND u.state = 'active' AND u.kind = 'human' FOR SHARE OF d)",
    )
    .bind(delegator.get())
    .bind(delegate.get())
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("review.delegation_active", e))
}

/// [`active_on`] on its own connection (a route's loader: deployments).
pub async fn active_delegation(
    db: &Db,
    delegator: UserId,
    delegate: UserId,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "review.delegation_active";
    db.within(QUERY, async {
        let mut conn = db.acquire().await?;
        active_on(&mut conn, delegator, delegate).await
    })
    .await
}

/// `FG-REVIEW-0033` (the approval names a person without a delegation to
/// the caller now).
pub fn inactive(reason: &'static str, delegator: UserId, delegate: UserId) -> ForgeError {
    ForgeError::new(ErrorCode::ApprovalDelegationInactive)
        .with_context("reason", reason)
        .with_context("on_behalf_of", delegator.get())
        .with_context("user_id", delegate.get())
}

fn invalid(param: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "param_invalid")
        .with_context("param", param)
        .with_context("rule", reason)
}

/// Set `delegator`'s delegation to `delegate_username` for
/// `[starts_at, ends_at)` (timestamps with a time zone), replacing any.
/// Refused (`FG-WEB-0006`): an unknown, disabled or non-human delegate
/// (`delegate`: `unknown`), oneself (`self`), a timestamp that does not
/// parse (`invalid_timestamp`), an empty or ended period (`period`), longer
/// than 30 days (`too_long`). The delegator must be an active person
/// (`FG-REPO-0005` otherwise: the route decided on the session).
pub async fn set_delegation_decided(
    db: &Db,
    ctx: &ChangeContext,
    delegator: UserId,
    delegate_username: &str,
    starts_at: &str,
    ends_at: &str,
) -> Result<(DelegationOutcome, StoredDelegation), ForgeError> {
    const QUERY: &str = "review.delegation_set";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let me: Option<(String, String)> = tx
            .step(|c| async move {
                sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR UPDATE")
                    .bind(delegator.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        match me {
            Some((kind, state)) if kind == UserKind::Human.as_str() && state == "active" => {}
            _ => {
                return Err(ForgeError::new(ErrorCode::NotFound)
                    .with_context("resource", "user")
                    .with_context("reason", "absent"))
            }
        }
        let delegate: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT id FROM forge.users WHERE username = $1 AND state = 'active' \
                     AND kind = 'human'",
                )
                .bind(delegate_username)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let delegate = UserId::new(delegate.ok_or_else(|| invalid("delegate", "unknown"))?);
        if delegate == delegator {
            return Err(invalid("delegate", "self"));
        }
        // PostgreSQL 16 `pg_input_is_valid`: a bad timestamp is the
        // caller's 400, never a database error.
        let (valid, nonempty, future, short): (bool, bool, bool, bool) = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT v, v AND s < e, v AND e > now(), v AND e <= s + interval '720 hours' \
                     FROM (SELECT pg_input_is_valid($1, 'timestamptz') \
                             AND pg_input_is_valid($2, 'timestamptz') AS v) ok \
                     CROSS JOIN LATERAL (SELECT CASE WHEN ok.v THEN $1::timestamptz END AS s, \
                       CASE WHEN ok.v THEN $2::timestamptz END AS e) t",
                )
                .bind(starts_at)
                .bind(ends_at)
                .fetch_one(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        if !valid {
            return Err(invalid("starts_at", "invalid_timestamp"));
        }
        if !nonempty || !future {
            return Err(invalid("ends_at", "period"));
        }
        if !short {
            return Err(invalid("ends_at", "too_long"));
        }
        let current: Option<Row> = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "SELECT {} WHERE d.delegator_id = $1 FOR UPDATE OF d",
                    columns()
                ))
                .bind(delegator.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let same: bool = match &current {
            Some(r) if r.3 == delegate.get() => {
                tx.step(|c| async move {
                    sqlx::query_scalar(
                        "SELECT starts_at = $2::timestamptz AND ends_at = $3::timestamptz \
                         FROM forge.approval_delegations WHERE delegator_id = $1",
                    )
                    .bind(delegator.get())
                    .bind(starts_at)
                    .bind(ends_at)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
                })
                .await?
            }
            Some(_) | None => false,
        };
        if same {
            if let Some(r) = current {
                return Ok(Settle::Discard((DelegationOutcome::Unchanged, of(r))));
            }
        }
        let row: Row = tx
            .step(|c| async move {
                sqlx::query(
                    "INSERT INTO forge.approval_delegations (delegator_id, delegate_id, \
                     starts_at, ends_at) VALUES ($1, $2, $3::timestamptz, $4::timestamptz) \
                     ON CONFLICT (delegator_id) DO UPDATE SET delegate_id = EXCLUDED.delegate_id, \
                     starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at, \
                     created_at = now()",
                )
                .bind(delegator.get())
                .bind(delegate.get())
                .bind(starts_at)
                .bind(ends_at)
                .execute(&mut *c)
                .await
                .map_err(|e| query_error(QUERY, e))?;
                sqlx::query_as(&format!("SELECT {} WHERE d.delegator_id = $1", columns()))
                    .bind(delegator.get())
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let stored = of(row);
        // A new delegate replaces the old one: what the old one approved for
        // the person ends (review of ADR-0038 step C).
        let (affected, events) =
            super::commands::end_delegated_on(&mut tx, delegator, Some(delegate)).await?;
        let details = AuditDetails::ApprovalDelegationSet {
            delegator,
            delegate,
            starts_at: stored.starts_at.clone(), // clone: the answer keeps it
            ends_at: stored.ends_at.clone(),
        };
        let mut record = ChangeRecord::audit_only(&details, RESOURCE, delegator.get());
        record.affected = affected;
        record.events = events;
        Ok(Settle::Commit(record, (DelegationOutcome::Set, stored)))
    }
    .await;
    tx.settle("approval_delegation.set", outcome).await
}

/// Remove `delegator`'s delegation (none: [`DelegationOutcome::Absent`]).
pub async fn remove_delegation_decided(
    db: &Db,
    ctx: &ChangeContext,
    delegator: UserId,
) -> Result<DelegationOutcome, ForgeError> {
    const QUERY: &str = "review.delegation_remove";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let removed: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "DELETE FROM forge.approval_delegations WHERE delegator_id = $1 \
                     RETURNING delegate_id",
                )
                .bind(delegator.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some(delegate) = removed else {
            return Ok(Settle::Discard(DelegationOutcome::Absent));
        };
        // Review of ADR-0038 step C: what the delegate approved for the
        // person ends with the delegation.
        let (affected, events) =
            super::commands::end_delegated_on(&mut tx, delegator, None).await?;
        let details = AuditDetails::ApprovalDelegationRemoved {
            delegator,
            delegate: UserId::new(delegate),
        };
        let mut record = ChangeRecord::audit_only(&details, RESOURCE, delegator.get());
        record.affected = affected;
        record.events = events;
        Ok(Settle::Commit(record, DelegationOutcome::Removed))
    }
    .await;
    tx.settle("approval_delegation.removed", outcome).await
}
