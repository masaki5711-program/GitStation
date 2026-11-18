//! The portal news lane of `forge worker` (ADR-0036): `portal.news_send` on
//! **its own job runner** (a portal that answers slowly holds a run for up
//! to `portal.timeout_ms`; the main runner's merge and provisioning jobs
//! never wait behind it), and once an hour the outbox's housekeeping
//! ([`housekeep`]): `pending` rows that no job will send any more are ended
//! as `failed` (`FG-INT-0023 reason=tries_used_up`, logged here once per
//! row), and finished rows older than 30 days are deleted. Present only
//! with `portal.enabled` and `portal.news_enabled`; the main runner's
//! reaper also reaps this lane's expired leases (the reap is per queue,
//! not per kind).
//!
//! A failed run is logged by the job runner (`FG-INT-0022`) or by the
//! sender itself (`FG-INT-0023`); a failed loop step (the database) is
//! logged here once and retried after a pause, never silently.

use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::{telemetry, RequestId};
use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_integ::portal_news::NewsSender;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::jobs::{run_job, JOB_POLL_PERIOD, SHUTDOWN_GRACE};
use super::supervise::Next;

/// How often the outbox is swept and purged.
const PURGE_PERIOD: Duration = Duration::from_secs(3600);
/// The rows one statement of the housekeeping ends or deletes at most.
const PURGE_BATCH: i64 = 500;
/// The statements one housekeeping runs at most per step (a backlog larger
/// than `PURGE_BATCH * HOUSEKEEP_ROUNDS` waits for the next hour, so the
/// lane's jobs never wait behind it for long).
const HOUSEKEEP_ROUNDS: usize = 20;
/// The pause after a failed step.
const FAILURE_PAUSE: Duration = Duration::from_secs(5);

/// The lane's runner with the sender registered.
pub(crate) fn runner(db: &Db, sender: NewsSender) -> Result<Arc<JobRunner>, ForgeError> {
    let mut runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
    runner.register(Arc::new(sender))?;
    Ok(Arc::new(runner))
}

/// Wait `pause` or until a stop is requested; `true`: stop.
async fn pause_or_stop(pause: Duration, stop: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        () = tokio::time::sleep(pause) => *stop.borrow(),
        changed = stop.changed() => changed.is_err() || *stop.borrow(),
    }
}

/// What one [`housekeep`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Housekept {
    /// `pending` rows ended as `failed` (each logged once).
    pub(crate) swept: u64,
    /// Finished rows deleted.
    pub(crate) purged: u64,
}

/// `FG-INT-0023` for a row the sweep ended, with the request id of the
/// change that staged it.
fn log_swept(row: &forge_db::portal::SweptNews) {
    let e = ForgeError::new(ErrorCode::PortalNewsUndeliverable)
        .with_context("event", "forge.portal.news_failed")
        .with_context("news_id", row.id)
        .with_context("source_id", row.source_id.clone()) // clone: the context owns it
        .with_context("reason", "tries_used_up")
        .with_context("detail", "job_ended")
        .with_context("attempts", row.attempts);
    let request_id = RequestId::parse(&row.request_id).unwrap_or_else(RequestId::generate);
    telemetry::log_failure(&e, &request_id);
}

/// The outbox's hourly housekeeping: end the `pending` rows whose job is
/// gone (review H2: no row stays `pending` for ever), then delete the
/// finished rows past their retention — each in batches of [`PURGE_BATCH`]
/// until a batch is smaller than that (at most [`HOUSEKEEP_ROUNDS`]).
/// The first failure stops it (the caller logs it; the next hour retries).
pub(crate) async fn housekeep(db: &Db) -> Result<Housekept, ForgeError> {
    let full = usize::try_from(PURGE_BATCH).unwrap_or(usize::MAX);
    let mut done = Housekept::default();
    for _ in 0..HOUSEKEEP_ROUNDS {
        let swept = forge_db::portal::sweep_orphaned(db, PURGE_BATCH).await?;
        for row in &swept {
            log_swept(row);
        }
        done.swept = done
            .swept
            .saturating_add(u64::try_from(swept.len()).unwrap_or(u64::MAX));
        if swept.len() < full {
            break;
        }
    }
    for _ in 0..HOUSEKEEP_ROUNDS {
        let purged = forge_db::portal::purge_finished(db, PURGE_BATCH).await?;
        done.purged = done.purged.saturating_add(purged);
        if purged < PURGE_BATCH.unsigned_abs() {
            break;
        }
    }
    Ok(done)
}

/// Run the lane until a stop is requested.
pub(crate) fn spawn(
    db: Db,
    runner: Arc<JobRunner>,
    mut stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut purged = Instant::now()
            .checked_sub(PURGE_PERIOD)
            .unwrap_or_else(Instant::now);
        loop {
            if *stop.borrow() {
                return;
            }
            if purged.elapsed() >= PURGE_PERIOD {
                purged = Instant::now();
                match housekeep(&db).await {
                    Ok(done) if done != Housekept::default() => tracing::info!(
                        target: "forge_cli",
                        event = "forge.portal.news_housekept",
                        swept = done.swept,
                        purged = done.purged,
                        "the portal news outbox was swept and purged"
                    ),
                    Ok(_) => {}
                    Err(e) => telemetry::log_failure(
                        &e.with_context("task", "portal_news_retention"),
                        &RequestId::generate(),
                    ),
                }
            }
            let pause = match run_job(&runner, SHUTDOWN_GRACE, stop.clone()).await {
                // A job ran, or the idle run waited by itself (plan 18 S2).
                Ok(Next::Immediately | Next::Waited) => continue,
                Ok(Next::AfterPeriod) => JOB_POLL_PERIOD,
                Err(e) => {
                    telemetry::log_failure(
                        &e.with_context("task", "portal_news_runner"),
                        &RequestId::generate(),
                    );
                    FAILURE_PAUSE
                }
            };
            if pause_or_stop(pause, &mut stop).await {
                return;
            }
        }
    })
}
