//! Counting a failure against a source event (ADR-0016 §7): record it, or
//! stop an `ordered` subscriber / quarantine the event of an
//! `independent` one after [`FAILURES_BEFORE_STOP`] failures in a row (a
//! derivation limit at once).

use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::cursor::{self, Cursor, EventRef};
use super::derived::Limit;
use super::run::{Incident, Progress};
use super::{Ordering, Subscriber, FAILURES_BEFORE_STOP};

/// The event a step works on, as far as counting a failure needs it.
pub(super) struct Attempt {
    pub(super) cursor: Cursor,
    pub(super) at: EventRef,
    pub(super) lag_events: i64,
}

/// A failure to count against the event.
pub(super) struct Failure {
    pub(super) error: ForgeError,
    /// The code stored as `last_error_code` / the dead letter's code: the
    /// handler's own failure, also when a limit decides what happens.
    pub(super) code: ErrorCode,
    pub(super) limit: Option<Limit>,
}

impl Failure {
    pub(super) fn of(error: ForgeError) -> Self {
        Self {
            code: error.code(),
            error,
            limit: None,
        }
    }
}

/// Count `failure` against the event: record it, or stop / quarantine
/// after [`FAILURES_BEFORE_STOP`] (a limit at once).
pub(super) async fn count(
    tx: &mut PgConnection,
    subscriber: &dyn Subscriber,
    attempt: &Attempt,
    failure: Failure,
) -> Result<Progress, ForgeError> {
    let name = subscriber.name();
    let Attempt { cursor, at, .. } = attempt;
    let failures = cursor.failures.saturating_add(1);
    let incident = |error: ForgeError| Incident {
        seq: Some(at.seq),
        event_id: Some(at.event_id.clone()), // clone: the incident outlives the step
        request_id: RequestId::parse(&at.request_id),
        error,
    };
    let Failure { error, code, limit } = failure;
    let summary = |reason: &'static str| {
        lag_error(reason, name, attempt)
            .with_context("failures", failures)
            .with_context("last_error_code", code.id())
    };
    let progress = match (
        subscriber.ordering(),
        limit,
        failures >= FAILURES_BEFORE_STOP,
    ) {
        (Ordering::Independent, Some(_), _) | (Ordering::Independent, None, true) => {
            cursor::quarantine(tx, name, at.seq, code, failures).await?;
            cursor::advance(tx, name, cursor.last_seq).await?;
            let error = match limit {
                Some(_) => error
                    .with_context("failures", failures)
                    .with_context("last_error_code", code.id()),
                None => summary("quarantined"),
            };
            Progress::Quarantined(incident(error))
        }
        (Ordering::Ordered, Some(_), _) => {
            cursor::record_failure(tx, name, cursor.last_seq, failures, code).await?;
            Progress::Stuck(incident(
                error
                    .with_context("failures", failures)
                    .with_context("last_error_code", code.id()),
            ))
        }
        (Ordering::Ordered, None, true) => {
            cursor::record_failure(tx, name, cursor.last_seq, failures, code).await?;
            Progress::Stuck(incident(summary("stuck")))
        }
        (Ordering::Ordered | Ordering::Independent, None, false) => {
            cursor::record_failure(tx, name, cursor.last_seq, failures, code).await?;
            Progress::Failed(incident(
                error
                    .with_context("subscriber", name)
                    .with_context("seq", at.seq)
                    .with_context("event_id", &at.event_id)
                    .with_context("failures", failures),
            ))
        }
    };
    Ok(progress)
}

/// `FG-DB-0008` about one subscriber and one source event.
pub(super) fn lag_error(reason: &'static str, subscriber: &str, attempt: &Attempt) -> ForgeError {
    ForgeError::new(ErrorCode::SubscriberLag)
        .with_context("reason", reason)
        .with_context("subscriber", subscriber)
        .with_context("seq", attempt.at.seq)
        .with_context("event_id", &attempt.at.event_id)
        .with_context("event_type", &attempt.at.event_type)
        .with_context("lag_events", attempt.lag_events)
}
