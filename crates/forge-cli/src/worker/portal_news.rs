//! The portal news lane of `forge worker` (ADR-0036): `portal.news_send` on
//! **its own job runner** (a portal that answers slowly holds a run for up
//! to `portal.timeout_ms`; the main runner's merge and provisioning jobs
//! never wait behind it), and once an hour the deletion of finished news
//! rows older than 30 days. Present only with `portal.enabled` and
//! `portal.news_enabled`; the main runner's reaper also reaps this lane's
//! expired leases (the reap is per queue, not per kind).
//!
//! A failed run is logged by the job runner (`FG-INT-0022`) or by the
//! sender itself (`FG-INT-0023`); a failed loop step (the database) is
//! logged here once and retried after a pause, never silently.

use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::{telemetry, RequestId};
use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::Db;
use forge_errors::ForgeError;
use forge_integ::portal_news::NewsSender;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::jobs::{run_job, JOB_POLL_PERIOD, SHUTDOWN_GRACE};
use super::supervise::Next;

/// How often finished rows are purged.
const PURGE_PERIOD: Duration = Duration::from_secs(3600);
/// The rows one purge deletes at most.
const PURGE_BATCH: i64 = 500;
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
                if let Err(e) = forge_db::portal::purge_finished(&db, PURGE_BATCH).await {
                    telemetry::log_failure(
                        &e.with_context("task", "portal_news_retention"),
                        &RequestId::generate(),
                    );
                }
            }
            let pause = match run_job(&runner, SHUTDOWN_GRACE, stop.clone()).await {
                Ok(Next::Immediately) => continue,
                Ok(_) => JOB_POLL_PERIOD,
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
