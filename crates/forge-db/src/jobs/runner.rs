//! [`JobRunner`]: claims a job for the kinds it has a [`JobHandler`] for,
//! runs it under its timeout and settles it (fenced). One run is one job;
//! several runs of one runner may run at once (`forge worker`'s slots):
//! claims are taken one at a time, a kind under its cap
//! ([`JobRunner::limit_kind`]) and never a job whose ordering key
//! (`super::ordering_key`: one merge request, one head) runs already. A run
//! whose handler did not end after its abort (`left_leased`) keeps its
//! ordering key and its kind's place until the job's lease ends: nothing
//! else guards the order of one merge request's versions —
//! `review::record_mr_version` moves the head to whatever head its job
//! read, with no check of the head it read it after, so a second version
//! job of the same merge request running beside a stale one could record
//! the older head last. After the lease the reaper may hand the job out
//! again anyway (the at-least-once contract).
//! It is the boundary for job failures: it logs them once, with the job's
//! origin `request_id`, and returns what it logged (tests, the worker).
//!
//! The handler runs in its own task. `abort()` takes effect only at the
//! handler's next `.await`, so after a timeout or a stop the runner waits up
//! to [`ABORT_WAIT`] for the task to end before it settles or releases the
//! job. A handler that does not end in time is logged
//! (`FG-DB-0007 reason=abort_pending`) and its job is left leased
//! ([`Ran::LeftLeased`]): only the reaper, after the lease, makes it
//! claimable again — never while the old run may still work.
//!
//! Cancel-safety (the `forge worker` Runner contract): dropping a run aborts
//! the handler's task (it cannot be awaited from a drop). The job stays
//! `running` with its lease; the lease expires and [`super::reap`] queues it
//! again (or deads it) — never lost, at worst run again.
//!
//! Idle (plan 18 S2): on a [`Db`] with a wake hub ([`Db::with_wake`]) a
//! caller whose run found nothing awaits [`JobRunner::wait_for_work`]
//! instead of sleeping its own poll period (`super::idle`).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use forge_core::telemetry;
use forge_errors::{ErrorCode, ForgeError};
use tokio::task::JoinHandle;

use super::idle::JobWake;
use super::reports::{
    dead_lettered, finished, job_error, log_outcome, report_reaped, with_job_context, Ran, Reported,
};
use super::{
    claim_timed, complete, fail, ordering_key, reap, release, ClaimedJob, Completed, Failed,
    JobFailure, JobKind, Retry, WorkerId,
};
use crate::Db;

/// How long an aborted handler may take to end before the job is settled.
pub const ABORT_WAIT: Duration = Duration::from_secs(5);
/// Bound for handing a job back at shutdown.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// One run of a job's handler.
pub type JobFuture = Pin<Box<dyn Future<Output = Result<(), JobFailure>> + Send + 'static>>;

/// Runs the jobs of one kind.
///
/// Delivery is at-least-once: a run may be repeated after a crash, an
/// expired lease or a lost answer. Every side effect must therefore be
/// idempotent or fenced by `job.lease.attempt` (e.g. a conditional write
/// that only applies for the newest attempt). A handler should await
/// regularly: an abort only takes effect at an `.await`, and a handler that
/// does not end within [`ABORT_WAIT`] keeps its job leased until the lease
/// expires. Never swallow a failure: return a [`JobFailure`].
pub trait JobHandler: Send + Sync {
    fn kind(&self) -> JobKind;
    fn run(&self, job: ClaimedJob) -> JobFuture;
}

pub struct JobRunner {
    db: Db,
    worker: WorkerId,
    handlers: BTreeMap<JobKind, Arc<dyn JobHandler>>,
    abort_wait: Duration,
    release_timeout: Duration,
    /// The most jobs of a kind running at once in this runner (absent:
    /// no cap besides the callers' own number of runs).
    caps: BTreeMap<JobKind, NonZeroUsize>,
    /// What runs now: claims are taken one at a time under `gate`, so a
    /// cap and an ordering key are never passed by two claims at once.
    in_flight: Arc<Mutex<InFlight>>,
    gate: tokio::sync::Mutex<()>,
    /// The idle wait, on a [`Db`] with a wake hub (`super::idle`).
    wake: Option<Arc<JobWake>>,
}

/// The jobs of a runner that run now.
#[derive(Debug, Default)]
struct InFlight {
    per_kind: BTreeMap<JobKind, usize>,
    /// Their ordering keys (`super::ordering_key`).
    keys: BTreeSet<String>,
}

/// One claimed job's place in [`InFlight`], given back when the run ends
/// (also when it is dropped: a cancelled run).
struct Slot {
    in_flight: Arc<Mutex<InFlight>>,
    kind: JobKind,
    key: Option<String>,
}

impl Slot {
    fn take(in_flight: &Arc<Mutex<InFlight>>, job: &ClaimedJob) -> Self {
        let key = ordering_key(&job.kind, &job.payload);
        {
            // A poisoned lock still holds counts that are right: every
            // change below is one statement.
            let mut held = in_flight.lock().unwrap_or_else(PoisonError::into_inner);
            *held.per_kind.entry(job.kind.clone()).or_insert(0) += 1; // clone: the map owns its key
            if let Some(k) = &key {
                held.keys.insert(k.clone()); // clone: the set owns its key
            }
        }
        Self {
            in_flight: Arc::clone(in_flight),
            kind: job.kind.clone(), // clone: the slot outlives the borrow
            key,
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut held = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = held.per_kind.get_mut(&self.kind) {
            *n = n.saturating_sub(1);
        }
        if let Some(k) = &self.key {
            held.keys.remove(k);
        }
    }
}

/// Aborts the handler's task when the run is dropped.
struct AbortOnDrop(JoinHandle<Result<(), JobFailure>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Why a handler was aborted.
#[derive(Debug, Clone, Copy)]
enum Cut {
    Timeout,
    Shutdown,
}

/// How the handler's run ended.
enum Exec {
    /// The handler ended by itself (also when it won a race with an abort).
    Finished(Result<(), JobFailure>),
    /// Aborted for `Cut`; the handler's task has ended.
    Aborted(Cut),
    /// Aborted for `Cut`, but the task did not end within the abort wait.
    StillRunning(Cut),
}

/// What waiting for an aborted handler found.
enum Drain {
    Ended(Result<(), JobFailure>),
    Aborted,
    Pending,
}

impl JobRunner {
    pub fn new(db: Db, worker: WorkerId) -> Self {
        Self {
            worker,
            handlers: BTreeMap::new(),
            abort_wait: ABORT_WAIT,
            release_timeout: RELEASE_TIMEOUT,
            caps: BTreeMap::new(),
            in_flight: Arc::new(Mutex::new(InFlight::default())),
            gate: tokio::sync::Mutex::new(()),
            wake: JobWake::of(&db).map(Arc::new),
            db,
        }
    }

    /// This runner waits on the wake hub of its [`Db`]: a caller whose run
    /// found nothing awaits [`Self::wait_for_work`]. False (no hub
    /// attached): the caller sleeps its own poll period, as before.
    pub fn waits_on_wake(&self) -> bool {
        self.wake.is_some()
    }

    /// Wait until another claim is worth a try: a `forge_jobs`
    /// notification for one of this runner's kinds, the due time of its
    /// next scheduled job, or `jobs.safety_poll_ms` (one second while the
    /// LISTEN connection is down) — `super::idle`. Returns at once without
    /// a hub ([`Self::waits_on_wake`]). Cancel-safe.
    pub async fn wait_for_work(&self) -> Result<(), ForgeError> {
        match &self.wake {
            Some(wake) => {
                let takes_now = |kind: &str| self.takes_now(kind);
                wake.wait(&self.db, &self.kinds(), takes_now).await
            }
            None => Ok(()),
        }
    }

    /// A claim could take a job of the kind named `kind` now: it has a
    /// handler and is under its cap.
    fn takes_now(&self, kind: &str) -> bool {
        let held = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.handlers.keys().any(|k| {
            k.as_str() == kind
                && self
                    .caps
                    .get(k)
                    .is_none_or(|cap| held.per_kind.get(k).copied().unwrap_or(0) < cap.get())
        })
    }

    /// At most `max` jobs of `kind` run at once in this runner (its runs
    /// share the runner: `forge worker`'s slots).
    pub fn limit_kind(&mut self, kind: &JobKind, max: NonZeroUsize) {
        self.caps.insert(kind.clone(), max); // clone: the map owns its key
    }

    /// The kinds a claim may take now (under their cap) and the ordering
    /// keys running now.
    fn claimable(&self) -> (Vec<JobKind>, Vec<String>) {
        let held = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let kinds = self
            .handlers
            .keys()
            .filter(|kind| {
                self.caps
                    .get(*kind)
                    .is_none_or(|cap| held.per_kind.get(*kind).copied().unwrap_or(0) < cap.get())
            })
            .cloned()
            .collect();
        (kinds, held.keys.iter().cloned().collect())
    }

    /// Jobs of this runner running now (tests, logs).
    pub fn running(&self) -> usize {
        let held = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        held.per_kind.values().sum()
    }

    /// Another bound for an aborted handler to end (tests).
    pub fn with_abort_wait(mut self, wait: Duration) -> Self {
        self.abort_wait = wait;
        self
    }

    /// Another bound for the hand-back at shutdown (tests).
    pub fn with_release_timeout(mut self, timeout: Duration) -> Self {
        self.release_timeout = timeout;
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
    /// handlers). `Err`: the claim failed (the caller logs it and retries).
    /// A result that cannot be recorded is [`Ran::Unrecorded`].
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
        let (job, slot, claimed_at, ready_for) = {
            // One claim at a time: a cap or an ordering key is decided and
            // taken together.
            let _gate = self.gate.lock().await;
            if let Some(wake) = &self.wake {
                wake.claim_starts();
            }
            let (kinds, busy) = self.claimable();
            let Some((job, ready_for)) = claim_timed(&self.db, &self.worker, &kinds, &busy).await?
            else {
                if let Some(wake) = &self.wake {
                    wake.found_nothing();
                }
                return Ok(None);
            };
            if let Some(wake) = &self.wake {
                wake.claimed(&job, ready_for, &self.worker);
            }
            let slot = Slot::take(&self.in_flight, &job);
            (job, slot, tokio::time::Instant::now(), ready_for)
        };
        let request_id = job.request_id();
        let settled = match self.execute(&job, stop, grace).await {
            Exec::Finished(Ok(())) => self.complete(&job).await,
            Exec::Finished(Err(failure)) => self.settle_failure(&job, failure).await,
            Exec::Aborted(Cut::Timeout) => self.settle_failure(&job, timed_out(&job)).await,
            Exec::Aborted(Cut::Shutdown) => self.release(&job).await,
            Exec::StillRunning(cut) => Ok(self.left_leased(&job, cut)),
        };
        let outcome = settled.unwrap_or_else(|e| Ran::Unrecorded {
            job_id: job.lease.job_id,
            error: with_job_context(e, &job),
        });
        log_outcome(&outcome, &job, ready_for, &request_id);
        if let Ran::LeftLeased { .. } = &outcome {
            // The old handler may still run: its ordering key (and its
            // kind's place) stays taken until its lease ends, when the
            // reaper may hand the job out again (module docs).
            let lease_end = claimed_at + job.timeout.get() + super::LEASE_SLACK;
            let wake = self.wake.clone(); // clone: the timer outlives this run
            tokio::spawn(async move {
                tokio::time::sleep_until(lease_end).await;
                drop(slot);
                if let Some(wake) = wake {
                    wake.place_freed();
                }
            });
        } else if let Some(wake) = &self.wake {
            // Before this run's place is given back (the end of the scope).
            wake.run_ended();
        }
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
        let cut = tokio::select! {
            joined = &mut task.0 => return Exec::Finished(finished(joined)),
            () = tokio::time::sleep(job.timeout.get()) => Cut::Timeout,
            () = &mut stop => match tokio::time::timeout(grace, &mut task.0).await {
                Ok(joined) => return Exec::Finished(finished(joined)),
                Err(_) => Cut::Shutdown,
            },
        };
        match self.drain(&mut task, job).await {
            Drain::Ended(result) => Exec::Finished(result),
            Drain::Aborted => Exec::Aborted(cut),
            Drain::Pending => Exec::StillRunning(cut),
        }
    }

    /// Abort the handler and wait (bounded) until its task has ended.
    async fn drain(&self, task: &mut AbortOnDrop, job: &ClaimedJob) -> Drain {
        task.0.abort();
        match tokio::time::timeout(self.abort_wait, &mut task.0).await {
            Ok(Err(e)) if e.is_cancelled() => Drain::Aborted,
            // It ended on its own before the abort took effect.
            Ok(joined) => Drain::Ended(finished(joined)),
            Err(_) => {
                let e = job_error(job, "jobs.abort", "abort_pending")
                    .with_context("wait_ms", self.abort_wait.as_millis());
                telemetry::log_failure(&e, &job.request_id());
                Drain::Pending
            }
        }
    }

    /// The handler may still run: keep the lease (the reaper requeues it
    /// after expiry). A timeout is logged as such first.
    fn left_leased(&self, job: &ClaimedJob, cut: Cut) -> Ran {
        let after = match cut {
            Cut::Timeout => {
                let timeout = with_job_context(timed_out(job).error, job);
                telemetry::log_failure(&timeout, &job.request_id());
                "execution_timeout"
            }
            Cut::Shutdown => "shutdown",
        };
        Ran::LeftLeased {
            job_id: job.lease.job_id,
            report: job_error(job, "jobs.abort", "left_leased").with_context("after", after),
        }
    }

    async fn complete(&self, job: &ClaimedJob) -> Result<Ran, ForgeError> {
        Ok(match complete(&self.db, job).await? {
            // Logged with the outcome (`reports::log_outcome`).
            Completed::Applied => Ran::Succeeded(job.lease.job_id),
            Completed::Fenced(e) => Ran::Fenced(e),
        })
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
            // The schedule is logged with the outcome (`reports::log_outcome`).
            Failed::Retrying { retry_in } => Ran::Retrying {
                job_id,
                failure: failure.error,
                retry_in,
            },
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

    /// Hand the stopped job back (bounded). Runs only after the handler's
    /// task has ended, which is what makes reusing the attempt safe.
    async fn release(&self, job: &ClaimedJob) -> Result<Ran, ForgeError> {
        let job_id = job.lease.job_id;
        match tokio::time::timeout(self.release_timeout, release(&self.db, job)).await {
            Ok(Ok(true)) => Ok(Ran::Released {
                job_id,
                report: job_error(job, "jobs.release", "released_on_shutdown"),
            }),
            Ok(Ok(false)) => Ok(Ran::Fenced(job_error(job, "jobs.release", "fenced"))),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(Ran::LeftLeased {
                job_id,
                report: job_error(job, "jobs.release", "release_timeout")
                    .with_context("timeout_ms", self.release_timeout.as_millis()),
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

/// The failure of a run aborted at its timeout (retried).
fn timed_out(job: &ClaimedJob) -> JobFailure {
    JobFailure::retry(
        ForgeError::new(ErrorCode::JobLeaseExpired)
            .with_context("operation", "jobs.run")
            .with_context("reason", "execution_timeout")
            .with_context("timeout_ms", job.timeout.get().as_millis()),
    )
}
