//! [`run_once`]: one subscriber step — at most one source event, in one
//! write-pool transaction (ADR-0016 §7). Bounded on the client: the step up
//! to COMMIT by [`STEP_TIMEOUT`], the handler alone by [`HANDLER_TIMEOUT`]
//! (a handler that hangs counts as a failure, so it is eventually stopped
//! or quarantined), COMMIT by [`COMMIT_TIMEOUT`] (`commit_outcome_unknown`:
//! the step may have committed; the next step reads the cursor again).
//! Cancel-safe: dropping it rolls the transaction back.

use std::time::Duration;

use forge_core::ids::ProjectId;
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::{PgConnection, Postgres};
use sqlx::{Connection as _, Transaction};

use super::derived::DerivedTx;
use super::{lag_error, Ordering, SourceEvent, Subscriber, FAILURES_BEFORE_STOP};
use crate::{query_error, Db};

/// A step never takes longer than this, start to COMMIT.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// The handler's share of it.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(20);
/// Bound for COMMIT.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(10);

/// A failure of a step, about one source event, for the caller to log once
/// (with the source event's request id).
#[derive(Debug)]
pub struct Incident {
    /// The source event (`None` when the step failed before reading one).
    pub seq: Option<i64>,
    pub request_id: Option<RequestId>,
    pub error: ForgeError,
}

/// What one step did.
#[derive(Debug)]
pub enum Progress {
    /// Another worker holds the subscriber's row (55P03): nothing done.
    Busy,
    /// `enabled = false` (set by an operator): nothing done.
    Disabled,
    /// No published event waits for it.
    CaughtUp,
    /// The event was handled and the cursor advanced.
    Handled { seq: i64, derived: i64 },
    /// The handler failed; the failure was counted and the event is tried
    /// again later (the cursor stays).
    Failed(Incident),
    /// `ordered`: `FG-DB-0008 reason=stuck / fanout_limit`; the cursor stays.
    Stuck(Incident),
    /// `independent`: the event went to forge.subscriber_dead_letters and
    /// the cursor advanced (`FG-DB-0008 reason=quarantined / fanout_limit`).
    Quarantined(Incident),
}

enum Step {
    /// Roll back (by dropping the transaction) and report.
    Nothing(Progress),
    Commit(Transaction<'static, Postgres>, Progress),
}

/// The subscriber's row.
struct Cursor {
    last_seq: i64,
    failures: i32,
}

pub async fn run_once(db: &Db, subscriber: &dyn Subscriber) -> Result<Progress, ForgeError> {
    let name = subscriber.name();
    let step = match tokio::time::timeout(STEP_TIMEOUT, step(db, subscriber)).await {
        Ok(step) => step,
        Err(_) => Err(ForgeError::new(ErrorCode::DbQueryFailed)
            .with_context("query", "subscribers.step")
            .with_context("reason", "step_timeout")
            .with_context("timeout_ms", STEP_TIMEOUT.as_millis())),
    }
    .map_err(|e| e.with_context("subscriber", name))?;
    match step {
        Step::Nothing(progress) => Ok(progress),
        Step::Commit(tx, progress) => match tokio::time::timeout(COMMIT_TIMEOUT, tx.commit()).await
        {
            Ok(Ok(())) => Ok(progress),
            Ok(Err(e)) => {
                Err(query_error("subscribers.commit", e).with_context("subscriber", name))
            }
            Err(_) => Err(ForgeError::new(ErrorCode::DbQueryFailed)
                .with_context("query", "subscribers.commit")
                .with_context("reason", "commit_outcome_unknown")
                .with_context("timeout_ms", COMMIT_TIMEOUT.as_millis())
                .with_context("subscriber", name)),
        },
    }
}

async fn step(db: &Db, subscriber: &dyn Subscriber) -> Result<Step, ForgeError> {
    let name = subscriber.name();
    let mut tx = db
        .write_pool()
        .begin()
        .await
        .map_err(|e| query_error("subscribers.begin", e))?;
    let Some(cursor) = claim(&mut tx, subscriber).await? else {
        return Ok(Step::Nothing(Progress::Busy));
    };
    let Some(cursor) = cursor else {
        return Ok(Step::Nothing(Progress::Disabled));
    };
    let high: i64 = sqlx::query_scalar(
        "SELECT coalesce((SELECT f.high_seq FROM forge.event_feed_state AS f \
         WHERE f.singleton), 0)",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| query_error("subscribers.high_seq", e))?;
    if cursor.last_seq >= high {
        return Ok(Step::Nothing(Progress::CaughtUp));
    }
    let seq = cursor.last_seq + 1;
    let Some(event) = source_event(&mut tx, seq).await? else {
        // Below high_seq every seq exists until retention removes it; the
        // retention overrun (FG-DB-0008 reason=retention_overrun) is step C7.
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "subscribers.step")
            .with_context("reason", "event_missing")
            .with_context("seq", seq));
    };
    let lag_events = high - cursor.last_seq;
    // The handler runs in a savepoint (a nested sqlx transaction): its
    // writes are undone on failure while the step goes on to count it.
    let mut handler_tx = tx
        .begin()
        .await
        .map_err(|e| query_error("subscribers.savepoint", e))?;
    let (outcome, limit, derived) = {
        let mut derived = DerivedTx::new(&mut handler_tx, &event);
        let outcome =
            tokio::time::timeout(HANDLER_TIMEOUT, subscriber.handle(&event, &mut derived)).await;
        (outcome, derived.limit(), derived.emitted())
    };
    let failure = match (outcome, limit) {
        (Ok(Ok(())), None) => None,
        (Ok(Ok(())) | Ok(Err(_)) | Err(_), Some(limit)) => Some(
            lag_error("fanout_limit", name, &event, lag_events)
                .with_context("limit", limit.as_str()),
        ),
        (Ok(Err(e)), None) => Some(e),
        (Err(_), None) => Some(
            ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "subscribers.handle")
                .with_context("reason", "handler_timeout")
                .with_context("timeout_ms", HANDLER_TIMEOUT.as_millis()),
        ),
    };
    let Some(failure) = failure else {
        handler_tx
            .commit()
            .await
            .map_err(|e| query_error("subscribers.release", e))?;
        advance(&mut tx, name, cursor.last_seq).await?;
        return Ok(Step::Commit(tx, Progress::Handled { seq, derived }));
    };
    handler_tx
        .rollback()
        .await
        .map_err(|e| query_error("subscribers.rollback_handler", e))?;
    let failures = cursor.failures.saturating_add(1);
    let incident = |error: ForgeError| Incident {
        seq: Some(seq),
        request_id: event.request_id(),
        error,
    };
    let code = failure.code();
    let summary = |reason: &'static str| {
        lag_error(reason, name, &event, lag_events)
            .with_context("failures", failures)
            .with_context("last_error_code", code.id())
    };
    let progress = match (
        subscriber.ordering(),
        limit,
        failures >= FAILURES_BEFORE_STOP,
    ) {
        (Ordering::Independent, Some(_), _) | (Ordering::Independent, None, true) => {
            quarantine(&mut tx, name, seq, code, failures).await?;
            advance(&mut tx, name, cursor.last_seq).await?;
            let error = match limit {
                Some(_) => failure.with_context("failures", failures),
                None => summary("quarantined"),
            };
            Progress::Quarantined(incident(error))
        }
        (Ordering::Ordered, Some(_), _) => {
            record_failure(&mut tx, name, cursor.last_seq, failures, code).await?;
            Progress::Stuck(incident(failure.with_context("failures", failures)))
        }
        (Ordering::Ordered, None, true) => {
            record_failure(&mut tx, name, cursor.last_seq, failures, code).await?;
            Progress::Stuck(incident(summary("stuck")))
        }
        (Ordering::Ordered | Ordering::Independent, None, false) => {
            record_failure(&mut tx, name, cursor.last_seq, failures, code).await?;
            Progress::Failed(incident(
                failure
                    .with_context("subscriber", name)
                    .with_context("seq", seq)
                    .with_context("event_id", event.event_id)
                    .with_context("failures", failures),
            ))
        }
    };
    Ok(Step::Commit(tx, progress))
}

/// Lock the subscriber's row without waiting. `None`: another worker holds
/// it (55P03, a normal path); `Some(None)`: disabled.
async fn claim(
    conn: &mut PgConnection,
    subscriber: &dyn Subscriber,
) -> Result<Option<Option<Cursor>>, ForgeError> {
    let name = subscriber.name();
    let row: Option<(i64, String, bool, i32)> = match sqlx::query_as(
        "SELECT last_seq, ordering, enabled, failures FROM forge.event_subscribers \
         WHERE name = $1 FOR UPDATE NOWAIT",
    )
    .bind(name)
    .fetch_optional(&mut *conn)
    .await
    {
        Ok(row) => row,
        Err(e) if is_lock_not_available(&e) => {
            tracing::debug!(
                target: "forge_db",
                subscriber = name,
                "subscriber row is held by another worker; skipped"
            );
            return Ok(None);
        }
        Err(e) => return Err(query_error("subscribers.claim", e)),
    };
    let Some((last_seq, ordering, enabled, failures)) = row else {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "subscribers.claim")
            .with_context("reason", "subscriber_not_registered"));
    };
    if Ordering::parse(&ordering) != Some(subscriber.ordering()) {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "subscribers.claim")
            .with_context("reason", "ordering_mismatch")
            .with_context("stored", ordering));
    }
    if !enabled {
        return Ok(Some(None));
    }
    Ok(Some(Some(Cursor { last_seq, failures })))
}

fn is_lock_not_available(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "55P03")
}

type EventRow = (
    String,
    String,
    i16,
    String,
    i64,
    Option<i64>,
    String,
    Option<i64>,
    String,
    Option<i64>,
    i16,
    String,
);

async fn source_event(
    conn: &mut PgConnection,
    seq: i64,
) -> Result<Option<SourceEvent>, ForgeError> {
    let row: Option<EventRow> = sqlx::query_as(
        "SELECT event_id::text, event_type, schema_version, aggregate_type, aggregate_id, \
         project_id, actor_kind, actor_user_id, request_id, root_seq, causation_depth, \
         payload::text FROM ONLY forge.events WHERE seq = $1",
    )
    .bind(seq)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("subscribers.source_event", e))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let decode = |what: &'static str| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "subscribers.source_event")
            .with_context("reason", "decode")
            .with_context("column", what)
            .with_context("seq", seq)
    };
    let event_id = uuid::Uuid::parse_str(&row.0).map_err(|_| decode("event_id"))?;
    let payload = serde_json::from_str(&row.11).map_err(|_| decode("payload"))?;
    Ok(Some(SourceEvent {
        seq,
        event_id,
        event_type: row.1,
        schema_version: row.2,
        aggregate_type: row.3,
        aggregate_id: row.4,
        project_id: row.5.map(ProjectId::new),
        actor_kind: row.6,
        actor_user_id: row.7,
        request_id: row.8,
        root_seq: row.9,
        causation_depth: row.10,
        payload,
    }))
}

/// `last_seq` → `prev + 1` and the failure streak cleared, only from `prev`
/// (monotonic: a second writer cannot move it twice).
async fn advance(conn: &mut PgConnection, name: &str, prev: i64) -> Result<(), ForgeError> {
    let updated = sqlx::query(
        "UPDATE forge.event_subscribers SET last_seq = $2 + 1, \
         last_txid = pg_catalog.pg_current_xact_id(), updated_at = pg_catalog.now(), \
         failures = 0, failing_since = NULL, last_error_code = NULL \
         WHERE name = $1 AND last_seq = $2",
    )
    .bind(name)
    .bind(prev)
    .execute(&mut *conn)
    .await
    .map_err(|e| query_error("subscribers.advance", e))?
    .rows_affected();
    progress_moved(updated, "subscribers.advance")
}

async fn record_failure(
    conn: &mut PgConnection,
    name: &str,
    prev: i64,
    failures: i32,
    code: ErrorCode,
) -> Result<(), ForgeError> {
    let updated = sqlx::query(
        "UPDATE forge.event_subscribers SET failures = $3, \
         failing_since = coalesce(failing_since, pg_catalog.now()), last_error_code = $4 \
         WHERE name = $1 AND last_seq = $2",
    )
    .bind(name)
    .bind(prev)
    .bind(failures)
    .bind(code.id())
    .execute(&mut *conn)
    .await
    .map_err(|e| query_error("subscribers.record_failure", e))?
    .rows_affected();
    progress_moved(updated, "subscribers.record_failure")
}

/// The row is locked by this transaction: exactly one row must match.
fn progress_moved(updated: u64, operation: &'static str) -> Result<(), ForgeError> {
    if updated == 1 {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::Internal)
        .with_context("operation", operation)
        .with_context("reason", "cursor_moved")
        .with_context("rows", updated))
}

async fn quarantine(
    conn: &mut PgConnection,
    name: &str,
    seq: i64,
    code: ErrorCode,
    attempts: i32,
) -> Result<(), ForgeError> {
    sqlx::query(
        "INSERT INTO forge.subscriber_dead_letters (subscriber, seq, error_code, attempts) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (subscriber, seq) DO NOTHING",
    )
    .bind(name)
    .bind(seq)
    .bind(code.id())
    .bind(attempts)
    .execute(&mut *conn)
    .await
    .map_err(|e| query_error("subscribers.quarantine", e))?;
    Ok(())
}
