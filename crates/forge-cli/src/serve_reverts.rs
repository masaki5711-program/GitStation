//! The revert drafts of `forge serve` (M4 C10, plan 04 M-4, ADR-0024
//! Amendment C10): the job `merge_request.revert_draft` (enqueued by the
//! worker's `revert-drafts` subscriber when a target branch's `ci/pipeline`
//! failed on a commit a merge produced) runs here, next to the browse cache
//! the new branch's write invalidates — as the merge job (`serve_merges`).
//! `forge worker` does not register the kind.
//!
//! One task under the supervisor of `serve_merges` ([`supervise`]): it
//! claims one job at a time (idle, it waits for the wake hub's
//! `forge_jobs` notification; every [`POLL_PERIOD`] without a hub). The
//! handler: a decision (opened, conflicted, skipped) completes the job and
//! is logged by `forge_review::Reverter` (`forge.review.revert_drafted`);
//! a failure that may pass later ([`retryable`]) is retried with the
//! queue's backoff; any other dies with its code (the job runner logs it
//! once).

use std::sync::Arc;
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind, JobRunner, WorkerId};
use forge_db::review::reverts::{revert_draft_payload, REVERT_DRAFT_JOB};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_review::Reverter;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::serve_merges::{retryable, supervise};

/// How often an idle runner looks for a draft.
const POLL_PERIOD: Duration = Duration::from_secs(2);
/// How long a draft in progress may finish after a shutdown request.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// The backoff of a failing claim and of a restarted task.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A failure streak is logged at most this often after its first line.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// The handler of `merge_request.revert_draft`.
pub(crate) struct RevertDraftHandler {
    kind: JobKind,
    reverter: Arc<Reverter>,
}

impl RevertDraftHandler {
    pub(crate) fn new(reverter: Arc<Reverter>) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(REVERT_DRAFT_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", REVERT_DRAFT_JOB)
        })?;
        Ok(Self { kind, reverter })
    }
}

impl JobHandler for RevertDraftHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let reverter = Arc::clone(&self.reverter);
        Box::pin(async move {
            let asked = revert_draft_payload(&job.payload).map_err(JobFailure::permanent)?;
            let rid = job.request_id();
            match reverter
                .draft_after_failure(asked.project, &asked.sha, &rid)
                .await
            {
                Ok(_report) => Ok(()),
                Err(e) if retryable(&e) => Err(JobFailure::retry(e)),
                Err(e) => Err(JobFailure::permanent(e)),
            }
        })
    }
}

/// The supervised task; [`RevertTasks::stop`] ends it.
pub(crate) struct RevertTasks {
    stop: watch::Sender<bool>,
    supervisor: JoinHandle<()>,
}

impl RevertTasks {
    /// Stop the runner and wait at most `wait` (a job in progress keeps
    /// its lease: the reaper requeues it after it expires).
    pub(crate) async fn stop(self, wait: Duration) {
        let _ = self.stop.send(true);
        match tokio::time::timeout(wait, self.supervisor).await {
            Ok(Ok(())) => {}
            Ok(Err(join)) => telemetry::log_failure(
                &ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("operation", "revert_supervisor_exited")
                    .with_context("panicked", join.is_panic()),
                &RequestId::generate(),
            ),
            Err(_) => telemetry::log_failure(
                &ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("operation", "revert_task_pending")
                    .with_context("wait_ms", wait.as_millis()),
                &RequestId::generate(),
            ),
        }
    }
}

/// Start the draft runner under its supervisor.
pub(crate) fn spawn(db: &Db, reverter: Arc<Reverter>) -> Result<RevertTasks, ForgeError> {
    let mut runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
    runner.register(Arc::new(RevertDraftHandler::new(reverter)?))?;
    tracing::info!(
        worker_id = runner.worker_id().as_str(),
        kind = REVERT_DRAFT_JOB,
        "forge.serve.revert_runner_started"
    );
    let runner = Arc::new(runner);
    let (stop, stopped) = watch::channel(false);
    let supervisor = tokio::spawn(supervise(
        "revert_runner",
        stopped,
        (BACKOFF_MIN, BACKOFF_MAX),
        move |s| tokio::spawn(run_jobs(Arc::clone(&runner), s)),
    ));
    Ok(RevertTasks { stop, supervisor })
}

/// A failure streak (logged once at its start, then at most every
/// [`LOG_EVERY`]) and its backoff.
#[derive(Default)]
struct Streak {
    count: u64,
    logged_at: Option<tokio::time::Instant>,
}

impl Streak {
    fn fail(&mut self, e: ForgeError) -> Duration {
        self.count += 1;
        if self.logged_at.is_none_or(|at| at.elapsed() >= LOG_EVERY) {
            telemetry::log_failure(
                &e.with_context("operation", "revert_runner_claim")
                    .with_context("streak", self.count),
                &RequestId::generate(),
            );
            self.logged_at = Some(tokio::time::Instant::now());
        }
        let shift = u32::try_from(self.count.saturating_sub(1).min(5)).unwrap_or(5);
        BACKOFF_MIN.saturating_mul(1 << shift).min(BACKOFF_MAX)
    }
}

async fn run_jobs(runner: Arc<JobRunner>, mut stop: watch::Receiver<bool>) {
    let mut streak = Streak::default();
    loop {
        if *stop.borrow() {
            break;
        }
        let mut job_stop = stop.clone(); // clone: the run waits on its own receiver
        let stopping = async move {
            let closed = job_stop.wait_for(|s| *s).await.is_err();
            if closed {
                std::future::pending::<()>().await;
            }
        };
        let wait = match runner.run_once_until(stopping, SHUTDOWN_GRACE).await {
            Ok(Some(_)) => {
                streak = Streak::default();
                continue;
            }
            // Plan 18 S2: idle, the runner waits for `NOTIFY forge_jobs`
            // (a revert was requested) or its safety-net period.
            Ok(None) if runner.waits_on_wake() => {
                streak = Streak::default();
                match crate::worker::jobs::wait_for_work(&runner, &mut stop).await {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(e) => streak.fail(e),
                }
            }
            Ok(None) => {
                streak = Streak::default();
                POLL_PERIOD
            }
            Err(e) => streak.fail(e),
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.wait_for(|s| *s) => break,
        }
    }
    tracing::info!("forge.serve.revert_runner_stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_claim_backs_off_and_caps() {
        let mut s = Streak::default();
        let waits: Vec<Duration> = (0..8)
            .map(|_| s.fail(ForgeError::new(ErrorCode::DbUnavailable)))
            .collect();
        assert_eq!(waits[0], BACKOFF_MIN);
        assert_eq!(waits.last().copied(), Some(BACKOFF_MAX));
        // The draft's own bound fits in the job's timeout.
        assert!(
            forge_db::review::reverts::REVERT_DRAFT_TIMEOUT
                > forge_review::RevertTimeouts::default().merge_tree
        );
    }
}
