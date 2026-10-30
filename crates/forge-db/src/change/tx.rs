//! [`ChangeTx`]: one transaction on the write pool for a change's state
//! writes, its audit row and its events, bounded on the client
//! (`CHANGE_TIMEOUT`, ADR-0016 §6). Dropping it without `commit` rolls back.
//!
//! The deadline covers everything up to COMMIT. COMMIT itself gets its own
//! bound and is never cancelled by the change's deadline: once it has been
//! sent the outcome may already be durable, so a timeout there is reported
//! as `commit_outcome_unknown`, never as a plain failure.

use std::time::{Duration, Instant};

use forge_core::DedupKey;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::{PgConnection, Postgres};
use sqlx::Transaction;

use super::validate::{fit_sizes, fitted, sanitized, validate};
use super::{ChangeContext, ChangeRecord, Committed, DomainEvent};
use crate::jobs::{enqueue_on, Enqueued, NewJob};
use crate::{query_error, Db};

/// A change never takes longer than this, start to COMMIT.
const CHANGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound for COMMIT (on a half-dead link it would never answer).
const COMMIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound for an explicit ROLLBACK (dropping the transaction rolls back too).
const ROLLBACK_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct ChangeTx {
    tx: Transaction<'static, Postgres>,
    ctx: ChangeContext,
    deadline: Instant,
    timeout: Duration,
    commit_timeout: Duration,
}

impl ChangeTx {
    pub(crate) async fn begin(db: &Db, ctx: &ChangeContext) -> Result<Self, ForgeError> {
        Self::begin_within(db, ctx, CHANGE_TIMEOUT).await
    }

    pub(crate) async fn begin_within(
        db: &Db,
        ctx: &ChangeContext,
        timeout: Duration,
    ) -> Result<Self, ForgeError> {
        let deadline = Instant::now() + timeout;
        let tx = bounded(deadline, timeout, async {
            db.write_pool()
                .begin()
                .await
                .map_err(|e| query_error("change.begin", e))
        })
        .await?;
        Ok(Self {
            tx,
            ctx: sanitized(ctx),
            deadline,
            timeout,
            commit_timeout: COMMIT_TIMEOUT,
        })
    }

    /// A shorter COMMIT bound (tests of `commit_outcome_unknown`).
    #[cfg(test)]
    pub(crate) fn with_commit_timeout(mut self, commit_timeout: Duration) -> Self {
        self.commit_timeout = commit_timeout;
        self
    }

    /// The connection of the change, unbounded: commands write through
    /// [`ChangeTx::step`], which counts against the deadline.
    // Tests write through it directly.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    /// Run one step of the change inside its deadline.
    // Tests bound arbitrary steps with it; commands use `step`.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) async fn within<T>(
        &self,
        step: impl std::future::Future<Output = Result<T, ForgeError>>,
    ) -> Result<T, ForgeError> {
        bounded(self.deadline, self.timeout, step).await
    }

    /// Run one statement of the change on its connection, inside its
    /// deadline (the typed commands' state writes).
    pub(crate) async fn step<'s, T, F, Fut>(&'s mut self, step: F) -> Result<T, ForgeError>
    where
        F: FnOnce(&'s mut PgConnection) -> Fut,
        Fut: std::future::Future<Output = Result<T, ForgeError>> + 's,
    {
        let (deadline, timeout) = (self.deadline, self.timeout);
        bounded(deadline, timeout, step(&mut self.tx)).await
    }

    /// Enqueue a job inside the change (ADR-0016 §7): it exists only if the
    /// change commits, and the insert counts against the change's deadline.
    // First production caller: `projects::create` (repository provisioning,
    // step D4).
    pub(crate) async fn enqueue_job(&mut self, job: &NewJob) -> Result<Enqueued, ForgeError> {
        let (deadline, timeout) = (self.deadline, self.timeout);
        bounded(deadline, timeout, enqueue_on(&mut self.tx, job)).await
    }

    /// Write the audit row and the events, then commit. A `dedup_key` that
    /// is already recorded rolls everything back ([`Committed::Deduped`]).
    pub(crate) async fn commit(self, record: ChangeRecord) -> Result<Committed, ForgeError> {
        let action = record.audit.action;
        let Self {
            mut tx,
            ctx,
            deadline,
            timeout,
            commit_timeout,
        } = self;
        let record = fitted(record);
        validate(&ctx, &record)?;
        let written = bounded(deadline, timeout, async {
            // Each JSON value is serialized once: measured, then inserted.
            let payloads: Vec<String> = record
                .events
                .iter()
                .map(|e| e.payload.to_string())
                .collect();
            let details = record.audit.details.to_string();
            let (details_bytes, payload_bytes) = stored_sizes(&mut tx, &details, &payloads).await?;
            let record = fit_sizes(record, details_bytes, &payload_bytes)?;
            // The details may have been replaced by a marker (≤ 8 KiB either way).
            let details = record.audit.details.to_string();
            insert_audit(&mut tx, &ctx, &record, &details).await?;
            let mut ids = Vec::with_capacity(record.events.len());
            for (event, payload) in record.events.iter().zip(&payloads) {
                match insert_event(&mut tx, &ctx, event, payload).await? {
                    Some(id) => ids.push(id),
                    None => return Ok(None),
                }
            }
            Ok(Some(ids))
        })
        .await
        .map_err(|e| e.with_context("action", action))?;
        let Some(event_ids) = written else {
            // Nothing of this change may stay. Dropping the transaction rolls
            // back too, so the change is still Deduped; a slow or failed
            // ROLLBACK points at a sick connection and is logged as such.
            let failure = match tokio::time::timeout(ROLLBACK_TIMEOUT, tx.rollback()).await {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(query_error("change.rollback", e)),
                Err(_) => Some(
                    ForgeError::new(ErrorCode::DbQueryFailed)
                        .with_context("query", "change.rollback")
                        .with_context("reason", "rollback_timeout")
                        .with_context("timeout_ms", ROLLBACK_TIMEOUT.as_millis()),
                ),
            };
            if let Some(e) = failure {
                tracing::warn!(
                    target: "forge_db",
                    request_id = %ctx.request_id,
                    error_code = e.code().id(),
                    action,
                    error = %e,
                    "rollback of a deduped change failed; the connection rolls back on release"
                );
            }
            return Ok(Committed::Deduped);
        };
        match tokio::time::timeout(commit_timeout, tx.commit()).await {
            Ok(Ok(())) => Ok(Committed::Recorded { event_ids }),
            Ok(Err(e)) => Err(query_error("change.commit", e).with_context("action", action)),
            Err(_) => Err(ForgeError::new(ErrorCode::DbQueryFailed)
                .with_context("query", "change.commit")
                .with_context("reason", "commit_outcome_unknown")
                .with_context("timeout_ms", commit_timeout.as_millis())
                .with_context("action", action)),
        }
    }
}

async fn bounded<T>(
    deadline: Instant,
    timeout: Duration,
    step: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    let left = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(left, step).await {
        Ok(result) => result,
        // Dropping the transaction rolls it back.
        Err(_) => Err(ForgeError::new(ErrorCode::DbQueryFailed)
            .with_context("query", "change")
            .with_context("reason", "change_timeout")
            .with_context("timeout_ms", timeout.as_millis())),
    }
}

/// The stored sizes of the details and payloads, measured as the table
/// CHECKs measure them.
async fn stored_sizes(
    conn: &mut PgConnection,
    details: &str,
    payloads: &[String],
) -> Result<(i32, Vec<i32>), ForgeError> {
    sqlx::query_as(
        "SELECT pg_catalog.pg_column_size($1::jsonb), ARRAY(SELECT          pg_catalog.pg_column_size(p.v::jsonb) FROM unnest($2::text[]) WITH ORDINALITY          AS p(v, n) ORDER BY p.n)",
    )
    .bind(details)
    .bind(payloads)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| query_error("change.sizes", e))
}

async fn insert_audit(
    conn: &mut PgConnection,
    ctx: &ChangeContext,
    record: &ChangeRecord,
    details: &str,
) -> Result<(), ForgeError> {
    let audit = &record.audit;
    let (credential, claimed, key_id, assertion) = match &ctx.actor {
        super::Actor::UserViaServiceAccount {
            credential_id,
            act_as_claimed,
            act_as_key_id,
            via_assertion_id,
            ..
        } => (
            Some(*credential_id),
            Some(act_as_claimed.as_str()),
            Some(*act_as_key_id),
            via_assertion_id.as_deref(),
        ),
        super::Actor::User(_)
        | super::Actor::ServiceAccount(_)
        | super::Actor::System
        | super::Actor::Anonymous => (ctx.credential_id, None, None, None),
    };
    sqlx::query(
        "INSERT INTO forge.audit_log (actor_kind, actor_user_id, actor_via_id, credential_id, \
         act_as_claimed, act_as_key_id, via_assertion_id, effective_scope, actor_label, action, \
         outcome, resource_type, resource_id, project_id, source_ip, user_agent, request_id, \
         error_code, details) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, \
         $14, $15::inet, $16, $17, $18, $19::jsonb)",
    )
    .bind(ctx.actor.kind())
    .bind(ctx.actor.user())
    .bind(ctx.actor.via())
    .bind(credential)
    .bind(claimed)
    .bind(key_id)
    .bind(assertion)
    .bind(&ctx.effective_scope)
    .bind(ctx.actor_label.as_deref())
    .bind(audit.action)
    .bind(audit.outcome.as_str())
    .bind(audit.resource_type)
    .bind(audit.resource_id)
    .bind(audit.project_id.map(|p| p.get()))
    .bind(ctx.source_ip.map(|ip| ip.to_string()))
    .bind(ctx.user_agent.as_deref())
    .bind(ctx.request_id.as_str())
    .bind(audit.error_code.map(|c| c.id()))
    .bind(details)
    .execute(&mut *conn)
    .await
    .map_err(|e| query_error("change.audit", e))?;
    Ok(())
}

/// Insert one event; `None` when its `dedup_key` is already recorded.
async fn insert_event(
    conn: &mut PgConnection,
    ctx: &ChangeContext,
    event: &DomainEvent,
    payload: &str,
) -> Result<Option<uuid::Uuid>, ForgeError> {
    let id = uuid::Uuid::now_v7();
    let inserted: Option<String> = sqlx::query_scalar(
        "INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, project_id, actor_kind, actor_user_id, actor_via_id, request_id, \
         dedup_key, payload) VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, \
         $12::jsonb) ON CONFLICT (dedup_key) DO NOTHING RETURNING event_id::text",
    )
    .bind(id.to_string())
    .bind(event.event_type.as_str())
    .bind(event.schema_version)
    .bind(event.aggregate_type)
    .bind(event.aggregate_id)
    .bind(event.project_id.map(|p| p.get()))
    .bind(ctx.actor.kind())
    .bind(ctx.actor.user())
    .bind(ctx.actor.via())
    .bind(ctx.request_id.as_str())
    .bind(event.dedup_key.as_ref().map(DedupKey::as_str))
    .bind(payload)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("change.event", e))?;
    Ok(inserted.map(|_| id))
}
