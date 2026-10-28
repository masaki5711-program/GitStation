//! `forge worker` — the background tasks of ADR-0016 §7, until SIGINT /
//! SIGTERM. Each [`Task`] runs in its own loop (`supervise`): one run, then
//! a wait (its period, none when the run asked to go on at once, or the
//! backoff after a failure). A failed run is logged and retried. Shutdown is
//! noticed while waiting, so a run in progress (bounded by its own timeout)
//! finishes first; only a panic or a lost shutdown channel aborts the other
//! tasks mid-run, so every run must be cancel-safe (see `supervise::Runner`).
//! Failure logging and backoff: `backoff`.
//!
//! Tasks today: the outbox sequencer (every 200 ms, at once again after a
//! full batch so a backlog drains without a pause), the job runner and the
//! job reaper (`jobs`: the runner is the one task that reacts to shutdown
//! mid-run, after a grace). The audit seal job (C8) becomes a further
//! [`Task`] variant with its arm in [`Task::run`] and its entry in [`tasks`].
//!
//! Before the first run the database is connected and the schema checked
//! (ADR-0014): the worker never migrates.

use std::sync::Arc;
use std::time::Duration;

use forge_core::config::DatabaseConfig;
use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::sequencer::{self, Assigned, AuditSequencing, BatchSize, SeqRange};
use forge_db::{Db, MIGRATOR};
use forge_errors::ForgeError;
use tokio::sync::watch;

use crate::runtime::block_on;

mod backoff;
mod jobs;
mod supervise;

use supervise::{supervise, Next, Runner};

/// The sequencer's period (ADR-0016 §7).
const SEQUENCER_PERIOD: Duration = Duration::from_millis(200);
/// The longest wait after repeated failures.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

pub fn worker(database: &DatabaseConfig) -> Result<String, ForgeError> {
    // Like serve: the migration and maintenance credentials stay out of the
    // long-running processes (ADR-0018).
    database.reject_privileged_logins()?;
    block_on(async {
        let db = Db::connect(database).await?;
        forge_db::check_schema(&db, &MIGRATOR).await?;
        let (stop, stopped) = watch::channel(false);
        let signal = tokio::spawn(async move {
            forge_web::server::shutdown_signal().await;
            // No receiver left means every task already ended: nothing to stop.
            let _ = stop.send(true);
        });
        tracing::info!(db_target = %db.target(), "forge.worker.started");
        let result = run(&db, stopped).await;
        signal.abort();
        db.close().await;
        tracing::info!("forge.worker.stopped");
        // A failure is logged once by the CLI boundary (main::report).
        result.map(|()| String::new())
    })
}

/// A background task of the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Task {
    /// `forge.assign_seq` + NOTIFY (forge-db `sequencer::run_once`).
    Sequencer,
    /// One job at a time (forge-db `jobs::JobRunner::run_once`).
    JobRunner,
    /// Expired leases (forge-db `jobs::JobRunner::reap_and_report`).
    JobReaper,
}

/// Every task of this worker.
fn tasks() -> [Task; 3] {
    [Task::Sequencer, Task::JobRunner, Task::JobReaper]
}

/// What the runs of the tasks share.
#[derive(Clone)]
struct Shared {
    db: Db,
    jobs: Arc<JobRunner>,
    /// How long a job in progress may finish after a shutdown request.
    grace: Duration,
    shutdown: watch::Receiver<bool>,
}

impl Task {
    fn name(self) -> &'static str {
        match self {
            Self::Sequencer => "sequencer",
            Self::JobRunner => "job_runner",
            Self::JobReaper => "job_reaper",
        }
    }

    fn period(self) -> Duration {
        match self {
            Self::Sequencer => SEQUENCER_PERIOD,
            Self::JobRunner => jobs::JOB_POLL_PERIOD,
            Self::JobReaper => jobs::REAP_PERIOD,
        }
    }

    /// The task as the supervisor runs it, with its own handles.
    fn runner(self, shared: &Shared) -> Runner {
        let shared = shared.clone(); // clone: pool handles and Arcs, one set per task
        Runner {
            name: self.name(),
            period: self.period(),
            run: Box::new(move || {
                let shared = shared.clone(); // clone: each run future owns its handles
                Box::pin(async move { self.run(&shared).await })
            }),
        }
    }

    /// One run, bounded by the callee's own timeout (the job runner also by
    /// the shutdown grace).
    async fn run(self, shared: &Shared) -> Result<Next, ForgeError> {
        let db = &shared.db;
        match self {
            Self::JobRunner => {
                let stop = shared.shutdown.clone(); // clone: the run waits on its own receiver
                jobs::run_job(&shared.jobs, shared.grace, stop).await
            }
            Self::JobReaper => jobs::reap(&shared.jobs).await,
            Self::Sequencer => {
                // Audit rows are not numbered yet: ADR-0018 §5 allows it only
                // after this process's own sink comparison succeeded, and
                // that per-process state arrives in C9. Events are numbered.
                let assigned =
                    sequencer::run_once(db, BatchSize::MAX, AuditSequencing::Skip).await?;
                if let Some(events) = assigned.events {
                    tracing::debug!(
                        task = self.name(),
                        from_seq = events.first(),
                        to_seq = events.last(),
                        "forge.worker.sequenced"
                    );
                }
                Ok(after_sequencing(assigned, BatchSize::MAX))
            }
        }
    }
}

/// A full batch (of events or audit rows) means more rows may wait: run
/// again at once instead of after the period.
fn after_sequencing(assigned: Assigned, batch: BatchSize) -> Next {
    let full = |r: Option<SeqRange>| r.is_some_and(|r| r.count() >= i64::from(batch.get()));
    if full(assigned.events) || full(assigned.audit) {
        Next::Immediately
    } else {
        Next::AfterPeriod
    }
}

/// Run every task until `shutdown` turns true (see `supervise`), with the
/// job handlers of this build.
pub(crate) async fn run(db: &Db, shutdown: watch::Receiver<bool>) -> Result<(), ForgeError> {
    let mut runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
    for handler in jobs::handlers() {
        runner.register(handler)?;
    }
    run_with(db, runner, jobs::SHUTDOWN_GRACE, shutdown).await
}

/// [`run`] with a given job runner and shutdown grace (tests).
pub(crate) async fn run_with(
    db: &Db,
    runner: JobRunner,
    grace: Duration,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    tracing::info!(
        worker_id = runner.worker_id().as_str(),
        job_kinds = runner.kinds().len(),
        "forge.worker.jobs"
    );
    let shared = Shared {
        db: db.clone(), // clone: pool handles
        jobs: Arc::new(runner),
        grace,
        shutdown: shutdown.clone(), // clone: the job runner's own receiver
    };
    let runners = tasks()
        .into_iter()
        .map(|task| task.runner(&shared))
        .collect();
    supervise(runners, MAX_BACKOFF, shutdown).await
}

#[cfg(test)]
mod jobs_tests;
#[cfg(test)]
mod loop_tests;
#[cfg(test)]
mod tests;
