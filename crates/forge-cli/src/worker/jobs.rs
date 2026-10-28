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
//! stop should wait. A job in progress gets [`SHUTDOWN_GRACE`]; after that
//! its run is dropped (the handler's task is aborted, see
//! `forge_db::jobs::JobRunner`). The job keeps its lease, which expires, and
//! the next reaper queues it again — the attempt counts.

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

/// One run of the job runner: one job (or none), bounded on shutdown by
/// `grace`.
pub(crate) async fn run_job(
    jobs: &JobRunner,
    grace: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> Result<Next, ForgeError> {
    let run = jobs.run_once();
    tokio::pin!(run);
    // `false`: the channel closed without a request (the task loop reports
    // it); the watch guard is dropped inside, never held across an await.
    let stop_requested = async move { shutdown.wait_for(|stop| *stop).await.is_ok() };
    tokio::select! {
        ran = &mut run => after_job(ran?),
        // A closed channel disables this branch.
        true = stop_requested => {
            match tokio::time::timeout(grace, &mut run).await {
                Ok(ran) => after_job(ran?),
                Err(_) => {
                    tracing::info!(
                        task = "job_runner",
                        worker_id = jobs.worker_id().as_str(),
                        grace_ms = grace.as_millis(),
                        "forge.worker.job_abandoned"
                    );
                    Ok(Next::AfterPeriod)
                }
            }
        }
    }
}

/// A job ran: more may be ready, look again at once.
fn after_job(ran: Option<Ran>) -> Result<Next, ForgeError> {
    Ok(match ran {
        Some(Ran::Succeeded(_) | Ran::Retrying { .. } | Ran::Dead { .. } | Ran::Fenced(_)) => {
            Next::Immediately
        }
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
