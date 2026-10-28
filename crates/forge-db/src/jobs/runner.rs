//! [`JobRunner`]: claims one job at a time for the kinds it has a
//! [`JobHandler`] for, runs it under its timeout and settles it (fenced).
//! It is the boundary for job failures: it logs them once, with the job's
//! origin `request_id`, and returns what it logged (tests, the worker).
//!
//! Cancel-safety (the `forge worker` Runner contract): the handler runs in
//! its own task, aborted when the run is dropped. A dropped run leaves the
//! job `running` with a lease; the lease expires and [`super::reap`] queues
//! it again (or deads it) — the job is never lost, at worst run again.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use tokio::task::JoinHandle;

use super::{
    claim, complete, fail, reap, ClaimedJob, Completed, Failed, JobFailure, JobId, JobKind,
    ReapedJob, WorkerId,
};
use crate::Db;

/// One run of a job's handler.
pub type JobFuture = Pin<Box<dyn Future<Output = Result<(), JobFailure>> + Send + 'static>>;

/// Runs the jobs of one kind. Handlers must be idempotent (delivery is
/// at-least-once) and must not swallow failures: return a [`JobFailure`].
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
}

/// Aborts the handler's task when the run is dropped.
struct AbortOnDrop(JoinHandle<Result<(), JobFailure>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl JobRunner {
    pub fn new(db: Db, worker: WorkerId) -> Self {
        Self {
            db,
            worker,
            handlers: BTreeMap::new(),
        }
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
        let Some(job) = claim(&self.db, &self.worker, &self.kinds()).await? else {
            return Ok(None);
        };
        let request_id = job.request_id();
        let outcome = match self.execute(&job).await {
            Ok(()) => match complete(&self.db, &job).await? {
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
            Err(failure) => self.settle_failure(&job, failure).await?,
        };
        log_outcome(&outcome, &request_id);
        Ok(Some(outcome))
    }

    /// The handler in its own task under the job's timeout.
    async fn execute(&self, job: &ClaimedJob) -> Result<(), JobFailure> {
        let Some(handler) = self.handlers.get(&job.kind) else {
            return Err(JobFailure::retry(
                ForgeError::new(ErrorCode::Internal)
                    .with_context("operation", "jobs.run")
                    .with_context("reason", "no_handler"),
            ));
        };
        let handler = Arc::clone(handler);
        let input = job.clone(); // clone: the handler's task owns its copy
        let mut task = AbortOnDrop(tokio::spawn(async move { handler.run(input).await }));
        match tokio::time::timeout(job.timeout.get(), &mut task.0).await {
            Ok(Ok(result)) => result,
            Ok(Err(joined)) => Err(JobFailure::retry(
                ForgeError::new(ErrorCode::Internal)
                    .with_context("operation", "jobs.run")
                    .with_context(
                        "reason",
                        if joined.is_panic() {
                            "job_panicked"
                        } else {
                            "job_cancelled"
                        },
                    ),
            )),
            // Dropping the guard aborts the handler.
            Err(_) => Err(JobFailure::retry(
                ForgeError::new(ErrorCode::JobLeaseExpired)
                    .with_context("operation", "jobs.run")
                    .with_context("reason", "execution_timeout")
                    .with_context("timeout_ms", job.timeout.get().as_millis()),
            )),
        }
    }

    async fn settle_failure(
        &self,
        job: &ClaimedJob,
        failure: JobFailure,
    ) -> Result<Ran, ForgeError> {
        let job_id = job.lease.job_id;
        let settled = fail(&self.db, job, &failure).await?;
        let error = failure
            .error
            .with_context("job_id", job_id.get())
            .with_context("kind", job.kind.as_str())
            .with_context("attempt", job.lease.attempt);
        Ok(match settled {
            Failed::Retrying { retry_in } => Ran::Retrying {
                job_id,
                failure: error.with_context("retry_in_ms", retry_in.as_millis()),
                retry_in,
            },
            Failed::Dead => {
                let report = dead_lettered(
                    job_id,
                    job.kind.as_str(),
                    job.lease.attempt,
                    error.code().id(),
                    &job.origin_request_id,
                )
                .with_context(
                    "reason",
                    if job.lease.attempt >= job.max_attempts {
                        "attempts_exhausted"
                    } else {
                        "permanent_failure"
                    },
                );
                // The cause first (its own code), then the dead letter.
                telemetry::log_failure(&error, &job.request_id());
                Ran::Dead { job_id, report }
            }
            Failed::Fenced(e) => Ran::Fenced(e),
        })
    }

    /// [`reap`], then log each expired lease: `FG-DB-0007` when it was
    /// queued again, `FG-DB-0006` when it is dead.
    pub async fn reap_and_report(&self) -> Result<Vec<Reported>, ForgeError> {
        let reaped = reap(&self.db).await?;
        Ok(reaped.iter().map(report_reaped).collect())
    }
}

fn dead_lettered(
    job_id: JobId,
    kind: &str,
    attempts: i32,
    last_error_code: &str,
    origin_request_id: &str,
) -> ForgeError {
    ForgeError::new(ErrorCode::JobDeadLettered)
        .with_context("job_id", job_id.get())
        .with_context("kind", kind)
        .with_context("attempts", attempts)
        .with_context("last_error_code", last_error_code)
        .with_context("origin_request_id", origin_request_id)
}

fn report_reaped(job: &ReapedJob) -> Reported {
    let request_id = RequestId::parse(&job.origin_request_id).unwrap_or_else(RequestId::generate);
    let reported = if job.dead {
        Reported::Dead(
            dead_lettered(
                job.job_id,
                &job.kind,
                job.attempts,
                ErrorCode::JobLeaseExpired.id(),
                &job.origin_request_id,
            )
            .with_context("reason", "lease_expired"),
        )
    } else {
        Reported::Requeued(
            ForgeError::new(ErrorCode::JobLeaseExpired)
                .with_context("operation", "jobs.reap")
                .with_context("reason", "lease_expired")
                .with_context("job_id", job.job_id.get())
                .with_context("kind", &job.kind)
                .with_context("worker_id", job.worker_id.as_deref().unwrap_or("none"))
                .with_context("attempt", job.attempts),
        )
    };
    match &reported {
        Reported::Requeued(e) | Reported::Dead(e) => telemetry::log_failure(e, &request_id),
    }
    reported
}

fn log_outcome(outcome: &Ran, request_id: &RequestId) {
    match outcome {
        Ran::Succeeded(_) => {}
        Ran::Retrying { failure: e, .. } | Ran::Dead { report: e, .. } | Ran::Fenced(e) => {
            telemetry::log_failure(e, request_id);
        }
    }
}
