//! The job tasks of `forge worker` (ADR-0016 §7): the job runner (claims
//! one job at a time for the kinds with a registered handler, every second
//! while idle, at once again after a job) and the reaper (every 30 s, also
//! once at startup so leases left by a crashed worker are ended early).
//!
//! Handlers are registered in [`handlers`]; none exist yet (the first ones
//! arrive with the subscribers and the seal job), so the runner claims
//! nothing — it must still run without error.
//!
//! Shutdown: a job may run for up to its timeout (≤ 1 h), longer than a
//! stop should wait. A job in progress gets [`SHUTDOWN_GRACE`]; then its
//! handler is aborted and, once it has ended, the job is handed back
//! (`queued`, ready now, the attempt not counted — a shutdown is not the
//! job's fault; `FG-DB-0007 reason=released_on_shutdown`). See
//! `forge_db::jobs::JobRunner::run_once_until`.

use std::sync::Arc;
use std::time::Duration;

use forge_db::jobs::{JobHandler, JobRunner, Ran, REAP_BATCH};
use forge_errors::ForgeError;
use tokio::sync::watch;

use super::supervise::Next;

/// How often an idle runner looks for a ready job.
pub(crate) const JOB_POLL_PERIOD: Duration = Duration::from_secs(1);
/// How often expired leases are reaped (ADR-0016 §7).
pub(crate) const REAP_PERIOD: Duration = Duration::from_secs(30);
/// How long a job in progress may finish after a shutdown request.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Every job handler of this build (none yet).
pub(crate) fn handlers() -> Vec<Arc<dyn JobHandler>> {
    Vec::new()
}

/// One run of the job runner: one job (or none); a shutdown request gives
/// the job in progress `grace`, then releases it.
pub(crate) async fn run_job(
    jobs: &JobRunner,
    grace: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> Result<Next, ForgeError> {
    let stop = async move {
        // The watch guard is dropped inside this statement, never held
        // across an await. A closed channel without a request never stops
        // the run (the task loop reports the channel).
        let closed = shutdown.wait_for(|stop| *stop).await.is_err();
        if closed {
            std::future::pending::<()>().await;
        }
    };
    Ok(match jobs.run_once_until(stop, grace).await? {
        // A job ran: more may be ready, look again at once.
        Some(
            Ran::Succeeded(_)
            | Ran::Retrying { .. }
            | Ran::Dead { .. }
            | Ran::Fenced(_)
            | Ran::Released { .. }
            | Ran::LeftLeased { .. },
        ) => Next::Immediately,
        None => Next::AfterPeriod,
    })
}

/// One run of the reaper (it logs every expired lease itself).
pub(crate) async fn reap(jobs: &JobRunner) -> Result<Next, ForgeError> {
    let reported = jobs.reap_and_report().await?;
    let full = i64::try_from(reported.len()).unwrap_or(i64::MAX) >= REAP_BATCH;
    Ok(if full {
        Next::Immediately
    } else {
        Next::AfterPeriod
    })
}
