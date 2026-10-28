//! The SQL of a subscriber step ([`super::run`]): lock the subscriber's
//! row, read the source event, and move or mark the cursor. Each helper is
//! one statement on the step's transaction.

use forge_core::ids::ProjectId;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{Ordering, SourceEvent, Subscriber};
use crate::query_error;

/// The subscriber's row.
#[derive(Debug, Clone, Copy)]
pub(super) struct Cursor {
    pub(super) last_seq: i64,
    pub(super) failures: i32,
}

/// Lock the subscriber's row without waiting. `None`: another worker holds
/// it (55P03, a normal path); `Some(None)`: disabled.
pub(super) async fn claim(
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
    // A programming error about the subscriber, not about an event: it is
    // not counted against the event (nothing would be gained by stopping or
    // quarantining it); the worker logs it at every retry (≤ 1 per minute).
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

/// What identifies a source event in logs and incidents, readable even
/// when the rest of the row cannot be decoded.
#[derive(Debug, Clone)]
pub(super) struct EventRef {
    pub(super) seq: i64,
    pub(super) event_id: String,
    pub(super) event_type: String,
    pub(super) request_id: String,
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

/// The event `seq`: `None` if no such row; the decoded event or its decode
/// failure (`FG-SYS-0001 reason=decode`, counted like a handler failure).
pub(super) async fn source_event(
    conn: &mut PgConnection,
    seq: i64,
) -> Result<Option<(EventRef, Result<SourceEvent, ForgeError>)>, ForgeError> {
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
    let at = EventRef {
        seq,
        event_id: row.0.clone(), // clone: kept for the incident if the step fails
        event_type: row.1.clone(), // clone: idem
        request_id: row.8.clone(), // clone: idem
    };
    Ok(Some((at, decode(seq, row))))
}

fn decode(seq: i64, row: EventRow) -> Result<SourceEvent, ForgeError> {
    let failed = |what: &'static str| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "subscribers.source_event")
            .with_context("reason", "decode")
            .with_context("column", what)
            .with_context("seq", seq)
    };
    let event_id = uuid::Uuid::parse_str(&row.0).map_err(|_| failed("event_id"))?;
    let payload = serde_json::from_str(&row.11).map_err(|_| failed("payload"))?;
    Ok(SourceEvent {
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
    })
}

/// `last_seq` → `prev + 1` and the failure streak cleared, only from `prev`
/// (monotonic: a second writer cannot move it twice).
pub(super) async fn advance(
    conn: &mut PgConnection,
    name: &str,
    prev: i64,
) -> Result<(), ForgeError> {
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

pub(super) async fn record_failure(
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
pub(super) fn progress_moved(updated: u64, operation: &'static str) -> Result<(), ForgeError> {
    if updated == 1 {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::Internal)
        .with_context("operation", operation)
        .with_context("reason", "cursor_moved")
        .with_context("rows", updated))
}

pub(super) async fn quarantine(
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
