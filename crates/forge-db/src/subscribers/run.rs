//! [`run_once`]: one subscriber step — at most one source event, in one
//! write-pool transaction (ADR-0016 §7). Bounded on the client: the step up
//! to COMMIT by [`Timeouts::step`], the handler alone by
//! [`Timeouts::handler`] (a handler that hangs counts as a failure, so it
//! is eventually stopped or quarantined), COMMIT by [`Timeouts::commit`]
//! (`commit_outcome_unknown`: the step may have committed; the next step
//! reads the cursor again). Cancel-safe: dropping it rolls the transaction
//! back.
//!
//! What is counted against the event (the failure streak that ends in
//! `stuck` / `quarantined`): a handler error or timeout, a derivation
//! limit, an event that cannot be decoded, a savepoint RELEASE that fails
//! (the handler left it aborted), and derived writes refused at COMMIT
//! (`FG-DB-0013 reason=derived_*`; counted in a fresh transaction). What is
//! not (an error of the step, retried with the dispatcher's backoff): the
//! database failing, the subscriber row missing or of another ordering,
//! the event missing below `high_seq`, the cursor moved under the lock.

use std::time::Duration;

use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::Postgres;
use sqlx::{Connection as _, Transaction};

use super::count::{count, lag_error, Attempt, Failure};
use super::cursor::{self, Wait};
use super::derived::DerivedTx;
use super::Subscriber;
use crate::{query_error, Db};

/// The bounds of one step.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timeouts {
    /// Start to COMMIT.
    pub(crate) step: Duration,
    /// The handler's share of it.
    pub(crate) handler: Duration,
    /// COMMIT.
    pub(crate) commit: Duration,
}

impl Timeouts {
    pub(crate) const DEFAULT: Self = Self {
        step: Duration::from_secs(30),
        handler: Duration::from_secs(20),
        commit: Duration::from_secs(10),
    };
}

/// A failure of a step, about one source event, for the caller to log once
/// (with the source event's request id).
#[derive(Debug)]
pub struct Incident {
    /// The source event (`None` when the step failed before reading one).
    pub seq: Option<i64>,
    pub event_id: Option<String>,
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
    /// The step failed on the event; the failure was counted and the event
    /// is tried again later (the cursor stays).
    Failed(Incident),
    /// `ordered`: `FG-DB-0008 reason=stuck / fanout_limit`; the cursor stays.
    Stuck(Incident),
    /// `independent`: the event went to forge.subscriber_dead_letters and
    /// the cursor advanced (`FG-DB-0008 reason=quarantined / fanout_limit`).
    Quarantined(Incident),
}

enum Step {
    /// Roll back and report.
    Nothing {
        tx: Transaction<'static, Postgres>,
        progress: Progress,
    },
    /// Commit; `handled` is set when the step handled the event (a refusal
    /// at COMMIT is then counted against it).
    Commit {
        tx: Transaction<'static, Postgres>,
        progress: Progress,
        handled: Option<Attempt>,
    },
}

pub async fn run_once(db: &Db, subscriber: &dyn Subscriber) -> Result<Progress, ForgeError> {
    run_once_with(db, subscriber, Timeouts::DEFAULT).await
}

/// [`run_once`] with given bounds (tests shorten them).
pub(crate) async fn run_once_with(
    db: &Db,
    subscriber: &dyn Subscriber,
    t: Timeouts,
) -> Result<Progress, ForgeError> {
    let name = subscriber.name();
    let with_name = |e: ForgeError| e.with_context("subscriber", name);
    let step = bounded(t.step, "subscribers.step", step(db, subscriber, t))
        .await
        .map_err(with_name)?;
    let (tx, progress, handled) = match step {
        Step::Nothing { tx, progress } => {
            // Explicitly, not by dropping: a dropped transaction is rolled
            // back by a task sqlx spawns, so the row lock of `claim` could
            // outlive the step and make the next one `Busy`.
            tx.rollback()
                .await
                .map_err(|e| with_name(query_error("subscribers.rollback", e)))?;
            return Ok(progress);
        }
        Step::Commit {
            tx,
            progress,
            handled,
        } => (tx, progress, handled),
    };
    let refused = match commit(tx, t.commit).await {
        Ok(()) => return Ok(progress),
        Err(e) => e,
    };
    match handled {
        Some(attempt) if is_derived_refusal(&refused) => {
            let recount = count_refused_commit(db, subscriber, attempt, refused, t);
            bounded(t.step, "subscribers.recount", recount)
                .await
                .map_err(with_name)
        }
        Some(_) | None => Err(with_name(refused)),
    }
}

async fn bounded<T>(
    limit: Duration,
    query: &'static str,
    work: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    match tokio::time::timeout(limit, work).await {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::DbQueryFailed)
            .with_context("query", query)
            .with_context("reason", "step_timeout")
            .with_context("timeout_ms", limit.as_millis())),
    }
}

async fn commit(tx: Transaction<'static, Postgres>, limit: Duration) -> Result<(), ForgeError> {
    match tokio::time::timeout(limit, tx.commit()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(query_error("subscribers.commit", e)),
        Err(_) => Err(ForgeError::new(ErrorCode::DbQueryFailed)
            .with_context("query", "subscribers.commit")
            .with_context("reason", "commit_outcome_unknown")
            .with_context("timeout_ms", limit.as_millis())),
    }
}

/// A derived-event check refused the handler's writes at COMMIT (XF002).
fn is_derived_refusal(e: &ForgeError) -> bool {
    e.code() == ErrorCode::AuditMissing
        && e.context()
            .iter()
            .any(|(k, v)| *k == "reason" && v.starts_with("derived_"))
}

async fn step(db: &Db, subscriber: &dyn Subscriber, t: Timeouts) -> Result<Step, ForgeError> {
    let name = subscriber.name();
    let mut tx = db
        .write_pool()
        .begin()
        .await
        .map_err(|e| query_error("subscribers.begin", e))?;
    let Some(claimed) = cursor::claim(&mut tx, subscriber, Wait::No).await? else {
        return Ok(nothing(tx, Progress::Busy));
    };
    let Some(cursor) = claimed else {
        return Ok(nothing(tx, Progress::Disabled));
    };
    let high: i64 = sqlx::query_scalar(
        "SELECT coalesce((SELECT f.high_seq FROM forge.event_feed_state AS f \
         WHERE f.singleton), 0)",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| query_error("subscribers.high_seq", e))?;
    if cursor.last_seq >= high {
        return Ok(nothing(tx, Progress::CaughtUp));
    }
    let seq = cursor.last_seq + 1;
    let Some((at, decoded)) = cursor::source_event(&mut tx, seq).await? else {
        // Below high_seq every seq exists until retention removes it; the
        // retention overrun (FG-DB-0008 reason=retention_overrun) is step C7.
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "subscribers.step")
            .with_context("reason", "event_missing")
            .with_context("seq", seq));
    };
    let attempt = Attempt {
        cursor,
        at,
        lag_events: high - cursor.last_seq,
    };
    let event = match decoded {
        Ok(event) => event,
        Err(e) => {
            let progress = count(&mut tx, subscriber, &attempt, Failure::of(e)).await?;
            return Ok(commit_step(tx, progress, None));
        }
    };
    // The handler runs in a savepoint (a nested sqlx transaction): its
    // writes are undone on failure while the step goes on to count it.
    let mut handler_tx = tx
        .begin()
        .await
        .map_err(|e| query_error("subscribers.savepoint", e))?;
    let (outcome, limit, derived) = {
        let mut derived = DerivedTx::new(&mut handler_tx, &event);
        let outcome =
            tokio::time::timeout(t.handler, subscriber.handle(&event, &mut derived)).await;
        (outcome, derived.limit(), derived.emitted())
    };
    let failure = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(Failure::of(e)),
        Err(_) => Some(Failure::of(
            ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "subscribers.handle")
                .with_context("reason", "handler_timeout")
                .with_context("timeout_ms", t.handler.as_millis()),
        )),
    };
    let failure = match (failure, limit) {
        (None, None) => match handler_tx.commit().await {
            Ok(()) => {
                cursor::advance(&mut tx, name, cursor.last_seq).await?;
                let handled = Progress::Handled { seq, derived };
                return Ok(commit_step(tx, handled, Some(attempt)));
            }
            // Dropping the failed savepoint queued its ROLLBACK TO: the
            // step goes on in the outer transaction.
            Err(e) => Failure::of(query_error("subscribers.release", e)),
        },
        (failure, Some(limit)) => {
            handler_tx
                .rollback()
                .await
                .map_err(|e| query_error("subscribers.rollback_handler", e))?;
            Failure {
                error: lag_error("fanout_limit", name, &attempt)
                    .with_context("limit", limit.as_str()),
                code: failure.map_or(ErrorCode::SubscriberLag, |f| f.code),
                limit: Some(limit),
            }
        }
        (Some(failure), None) => {
            handler_tx
                .rollback()
                .await
                .map_err(|e| query_error("subscribers.rollback_handler", e))?;
            failure
        }
    };
    let progress = count(&mut tx, subscriber, &attempt, failure).await?;
    Ok(commit_step(tx, progress, None))
}

fn nothing(tx: Transaction<'static, Postgres>, progress: Progress) -> Step {
    Step::Nothing { tx, progress }
}

fn commit_step(
    tx: Transaction<'static, Postgres>,
    progress: Progress,
    handled: Option<Attempt>,
) -> Step {
    Step::Commit {
        tx,
        progress,
        handled,
    }
}

/// The handled event's writes were refused at COMMIT: count that in a new
/// transaction, if the cursor is still where the step found it. The claim
/// waits (bounded) for the row: the client reads the COMMIT error before
/// the server has aborted the refused transaction and released its row
/// lock, so a NOWAIT claim here could meet our own dying lock.
async fn count_refused_commit(
    db: &Db,
    subscriber: &dyn Subscriber,
    attempt: Attempt,
    refused: ForgeError,
    t: Timeouts,
) -> Result<Progress, ForgeError> {
    let mut tx = db
        .write_pool()
        .begin()
        .await
        .map_err(|e| query_error("subscribers.begin", e))?;
    let now = match cursor::claim(&mut tx, subscriber, Wait::Bounded).await? {
        Some(Some(now)) if now.last_seq == attempt.cursor.last_seq => now,
        // Held (even after the wait), disabled or moved meanwhile: not
        // counted here, and said so; the next step meets the event again.
        claimed => {
            let why = match claimed {
                None => "row_held",
                Some(None) => "disabled",
                Some(Some(_)) => "moved",
            };
            return Err(refused
                .with_context("counted", false)
                .with_context("not_counted", why));
        }
    };
    let attempt = Attempt {
        cursor: now,
        ..attempt
    };
    let progress = count(&mut tx, subscriber, &attempt, Failure::of(refused)).await?;
    commit(tx, t.commit).await?;
    Ok(progress)
}
