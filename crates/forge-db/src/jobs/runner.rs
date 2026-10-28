//! [`JobRunner`]: claims one job at a time for the kinds it has a
//! [`JobHandler`] for, runs it under its timeout and settles it (fenced).
//! It is the boundary for job failures: it logs them once, with the job's
//! origin `request_id`, and returns what it logged (tests, the worker).
//!
//! The handler runs in its own task. `abort()` takes effect only at the
//! handler's next `.await`, so after a timeout or a stop the runner waits up
//! to [`ABORT_WAIT`] for the task to end before it settles or releases the
//! job (otherwise the job could be claimed again while the old run still
//! works). A handler that does not end in time is logged
//! (`FG-DB-0007 reason=abort_pending`).
//!
//! Cancel-safety (the `forge worker` Runner contract): dropping a run aborts
//! the handler's task (it cannot be awaited from a drop). The job stays
//! `running` with its lease; the lease expires and [`super::reap`] queues it
//! again (or deads it) — never lost, at worst run again.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use forge_core::telemetry;
use forge_errors::{ErrorCode, ForgeError};
use tokio::task::JoinHandle;

use super::reports::{dead_lettered, finished, job_error, log_outcome, report_reaped};
use super::{
    claim, complete, fail, reap, release, ClaimedJob, Completed, Failed, JobFailure, JobId,
    JobKind, Retry, WorkerId,
};
use crate::Db;

/// How long an aborted handler may take to end before the job is settled.
pub const ABORT_WAIT: Duration = Duration::from_secs(5);
/// Bound for handing a job back at shutdown.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// One run of a job's handler.
pub type JobFuture = Pin<Box<dyn Future<Output = Result<(), JobFailure>> + Send + 'static>>;

/// Runs the jobs of one kind.
///
/// Delivery is at-least-once: a run may be repeated after a crash, an
/// expired lease or a lost answer, and an aborted run may still be alive
/// for up to [`ABORT_WAIT`] (longer if it blocks without awaiting) while
/// the next attempt starts. Every side effect must therefore be idempotent
/// or fenced by `job.lease.attempt` (e.g. a conditional write that only
/// applies for the newest attempt). Never swallow a failure: return a
/// [`JobFailure`].
pub trait JobHandler: Send + Sync {
    fn kind(&self) -> JobKind;
    fn run(&self, job: ClaimedJob) -> JobFuture;
}

/// What [`JobRunner::run_once`] did with the job it claimed.
#[derive(Debug)]
pub enum Ran {
    Succeeded(JobId),
    /// The run failed and the job is queued again (the failure was logged).
    Retrying {
        job_id: JobId,
        failure: ForgeError,
        retry_in: Duration,
    },
    /// The job is dead: `FG-DB-0006` (logged, alert).
    Dead {
        job_id: JobId,
        report: ForgeError,
    },
    /// The lease was lost before the result was recorded: `FG-DB-0007`
    /// (logged); the result is discarded.
    Fenced(ForgeError),
    /// Stopped before it finished and handed back without counting the
    /// attempt: `FG-DB-0007 reason=released_on_shutdown` (logged).
    Released {
        job_id: JobId,
        report: ForgeError,
    },
    /// Stopped, but the handler did not end in time: the job keeps its lease
    /// (reap queues it after expiry): `FG-DB-0007 reason=left_leased`.
    LeftLeased {
        job_id: JobId,
        report: ForgeError,
    },
}

/// What [`JobRunner::reap_and_report`] logged for one expired lease.
#[derive(Debug)]
pub enum Reported {
    /// `FG-DB-0007 reason=lease_expired` (warn): queued again.
    Requeued(ForgeError),
    /// `FG-DB-0006 reason=lease_expired` (alert): attempts used up.
    Dead(ForgeError),
}

pub struct JobRunner {
    db: Db,
    worker: WorkerId,
    handlers: BTreeMap<JobKind, Arc<dyn JobHandler>>,
    abort_wait: Duration,
}

/// Aborts the handler's task when the run is dropped.
struct AbortOnDrop(JoinHandle<Result<(), JobFailure>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How the handler's run ended.
enum Exec {
    Finished(Result<(), JobFailure>),
    /// Stopped after the grace; `drained`: the handler's task has ended.
    Stopped {
        drained: bool,
    },
}

impl JobRunner {
    pub fn new(db: Db, worker: WorkerId) -> Self {
        Self {
            db,
            worker,
            handlers: BTreeMap::new(),
            abort_wait: ABORT_WAIT,
        }
    }

    /// Another bound for an aborted handler to end (tests).
    pub fn with_abort_wait(mut self, wait: Duration) -> Self {
        self.abort_wait = wait;
        self
    }

    /// Add the handler of a kind; a second handler for a kind is a bug.
    pub fn register(&mut self, handler: Arc<dyn JobHandler>) -> Result<(), ForgeError> {
        let kind = handler.kind();
        if self.handlers.contains_key(&kind) {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "jobs.register")
                .with_context("reason", "duplicate_handler")
                .with_context("kind", kind.as_str()));
        }
        self.handlers.insert(kind, handler);
        Ok(())
    }

    pub fn worker_id(&self) -> &WorkerId {
        &self.worker
    }

    /// The kinds this runner claims.
    pub fn kinds(&self) -> Vec<JobKind> {
        self.handlers.keys().cloned().collect()
    }

    /// Claim and run one job. `Ok(None)`: nothing ready (always, without
    /// handlers). `Err`: a queue statement failed (the caller logs it and
    /// retries; a claimed job whose result could not be recorded is reaped).
    pub async fn run_once(&self) -> Result<Option<Ran>, ForgeError> {
        self.run_once_until(std::future::pending(), Duration::ZERO)
            .await
    }

    /// [`Self::run_once`] that stops when `stop` resolves: the job in
    /// progress gets `grace` to finish, then it is aborted and — once its
    /// handler has ended — released ([`Ran::Released`]).
    pub async fn run_once_until(
        &self,
        stop: impl Future<Output = ()>,
        grace: Duration,
    ) -> Result<Option<Ran>, ForgeError> {
        let Some(job) = claim(&self.db, &self.worker, &self.kinds()).await? else {
            return Ok(None);
        };
        let request_id = job.request_id();
        let outcome = match self.execute(&job, stop, grace).await {
            Exec::Finished(Ok(())) => match complete(&self.db, &job).await? {
                Completed::Applied => {
                    tracing::debug!(
                        target: "forge_db",
                        request_id = %request_id,
                        job_id = job.lease.job_id.get(),
                        kind = job.kind.as_str(),
                        attempt = job.lease.attempt,
                        "forge.jobs.succeeded"
                    );
                    Ran::Succeeded(job.lease.job_id)
                }
                Completed::Fenced(e) => Ran::Fenced(e),
            },
            Exec::Finished(Err(failure)) => self.settle_failure(&job, failure).await?,
            Exec::Stopped { drained: true } => self.release(&job).await?,
            Exec::Stopped { drained: false } => Ran::LeftLeased {
                job_id: job.lease.job_id,
                report: job_error(&job, "jobs.stop", "left_leased"),
            },
        };
        log_outcome(&outcome, &request_id);
        Ok(Some(outcome))
    }

    /// The handler in its own task under the job's timeout, until `stop`.
    async fn execute(
        &self,
        job: &ClaimedJob,
        stop: impl Future<Output = ()>,
        grace: Duration,
    ) -> Exec {
        let Some(handler) = self.handlers.get(&job.kind) else {
            return Exec::Finished(Err(JobFailure::retry(
                ForgeError::new(ErrorCode::Internal)
                    .with_context("operation", "jobs.run")
                    .with_context("reason", "no_handler"),
            )));
        };
        let handler = Arc::clone(handler);
        let input = job.clone(); // clone: the handler's task owns its copy
        let mut task = AbortOnDrop(tokio::spawn(async move { handler.run(input).await }));
        tokio::pin!(stop);
        let joined = tokio::select! {
            joined = &mut task.0 => joined,
            () = tokio::time::sleep(job.timeout.get()) => {
                self.drain(&mut task, job).await;
                return Exec::Finished(Err(JobFailure::retry(
                    ForgeError::new(ErrorCode::JobLeaseExpired)
                        .with_context("operation", "jobs.run")
                        .with_context("reason", "execution_timeout")
                        .with_context("timeout_ms", job.timeout.get().as_millis()),
                )));
            }
            () = &mut stop => match tokio::time::timeout(grace, &mut task.0).await {
                Ok(joined) => joined,
                Err(_) => return Exec::Stopped { drained: self.drain(&mut task, job).await },
            },
        };
        Exec::Finished(finished(joined))
    }

    /// Abort the handler and wait (bounded) until its task has ended.
    async fn drain(&self, task: &mut AbortOnDrop, job: &ClaimedJob) -> bool {
        task.0.abort();
        match tokio::time::timeout(self.abort_wait, &mut task.0).await {
            Ok(_) => true,
            Err(_) => {
                let e = job_error(job, "jobs.abort", "abort_pending")
                    .with_context("wait_ms", self.abort_wait.as_millis());
                telemetry::log_failure(&e, &job.request_id());
                false
            }
        }
    }

    async fn settle_failure(
        &self,
        job: &ClaimedJob,
        failure: JobFailure,
    ) -> Result<Ran, ForgeError> {
        let job_id = job.lease.job_id;
        let JobFailure { error, retry } = failure;
        let error = error
            .with_context("job_id", job_id.get())
            .with_context("kind", job.kind.as_str())
            .with_context("attempt", job.lease.attempt);
        // Logged before it is recorded: a failing `fail` must not lose it.
        telemetry::log_failure(&error, &job.request_id());
        let failure = JobFailure { error, retry };
        Ok(match fail(&self.db, job, &failure).await? {
            Failed::Retrying { retry_in } => {
                tracing::debug!(
                    target: "forge_db",
                    request_id = %job.request_id(),
                    job_id = job_id.get(),
                    kind = job.kind.as_str(),
                    retry_in_ms = retry_in.as_millis(),
                    "forge.jobs.retry_scheduled"
                );
                Ran::Retrying {
                    job_id,
                    failure: failure.error,
                    retry_in,
                }
            }
            Failed::Dead => {
                let reason = match failure.retry {
                    Retry::Never => "permanent_failure",
                    Retry::Backoff => "attempts_exhausted",
                };
                let report = dead_lettered(
                    job_id,
                    job.kind.as_str(),
                    job.lease.attempt,
                    failure.error.code().id(),
                    &job.origin_request_id,
                )
                .with_context("reason", reason);
                Ran::Dead { job_id, report }
            }
            Failed::Fenced(e) => Ran::Fenced(e),
        })
    }

    /// Hand the stopped job back (bounded).
    async fn release(&self, job: &ClaimedJob) -> Result<Ran, ForgeError> {
        let job_id = job.lease.job_id;
        match tokio::time::timeout(RELEASE_TIMEOUT, release(&self.db, job)).await {
            Ok(Ok(true)) => Ok(Ran::Released {
                job_id,
                report: job_error(job, "jobs.release", "released_on_shutdown"),
            }),
            Ok(Ok(false)) => Ok(Ran::Fenced(job_error(job, "jobs.release", "fenced"))),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(Ran::LeftLeased {
                job_id,
                report: job_error(job, "jobs.release", "release_timeout")
                    .with_context("timeout_ms", RELEASE_TIMEOUT.as_millis()),
            }),
        }
    }

    /// [`reap`], then log each expired lease: `FG-DB-0007` when it was
    /// queued again, `FG-DB-0006` when it is dead.
    pub async fn reap_and_report(&self) -> Result<Vec<Reported>, ForgeError> {
        let reaped = reap(&self.db).await?;
        Ok(reaped.iter().map(report_reaped).collect())
    }
}
