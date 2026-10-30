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
//! mid-run, after a grace), and the audit seal (`seal`: looks every 10 s,
//! seals a minute after the last seal or when more than 1000 rows wait)
//! and, when any subscriber is registered, the event subscribers
//! (`subscribers`, C5: woken by the sequencer's NOTIFY or the 5 s poll).
//!
//! Before the first run the seal keys are read (`audit.seal_keys_file` is
//! required: without it the worker does not start, `FG-SYS-0002
//! reason=missing`), the storage root is opened (`storage.root` is required
//! too: the `repository.provision` job creates repositories there, D4 MR3b;
//! `FG-SYS-0002` / `FG-REPO-0001` before anything runs), the database is
//! connected and the schema checked (ADR-0014): the worker never migrates.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use forge_core::config::{Config, SealKeys};
use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::sequencer::{self, Assigned, AuditSequencing, BatchSize, SeqRange};
use forge_db::subscribers::Registry;
use forge_db::{Db, MIGRATOR};
use forge_errors::{ErrorCode, ForgeError};
use tokio::sync::watch;

use crate::runtime::block_on;

mod backoff;
mod jobs;
// ── D4 MR3b: repository provisioning ──
pub(crate) mod provision;
// ── end D4 MR3b ──
mod seal;
// ── C5: event subscribers ──
mod subscribers;
// ── end C5 ──
mod supervise;

use provision::Storage;
use supervise::{supervise, Next, Runner};

/// The sequencer's period (ADR-0016 §7).
const SEQUENCER_PERIOD: Duration = Duration::from_millis(200);
/// The longest wait after repeated failures.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

pub fn worker(cfg: &Config) -> Result<String, ForgeError> {
    let database = &cfg.database;
    // Like serve: the migration and maintenance credentials stay out of the
    // long-running processes (ADR-0018).
    database.reject_privileged_logins()?;
    // The worker seals the audit log: no keys, no start (ADR-0018 §4).
    let keys = Arc::new(cfg.audit.require_seal_keys()?);
    // No storage, no start: provisioning would only fail (D4 MR3b).
    let storage = Storage::open(cfg.storage_root()?)?;
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
        let result = run(&db, keys, &storage, stopped).await;
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
    /// The audit seal (forge-db `seal::seal_due`, ADR-0018 §4).
    AuditSeal,
    // ── C5: event subscribers ──
    /// Every registered subscriber (forge-db `subscribers::Dispatcher`).
    Subscribers,
    // ── end C5 ──
}

/// Every task of this worker.
fn tasks(registry: &Registry) -> Vec<Task> {
    let mut tasks = vec![
        Task::Sequencer,
        Task::JobRunner,
        Task::JobReaper,
        Task::AuditSeal,
    ];
    // ── C5: no subscriber, no task (and no LISTEN connection) ──
    if !registry.is_empty() {
        tasks.push(Task::Subscribers);
    }
    // ── end C5 ──
    tasks
}

// ── C5: the registered subscribers ──
/// The subscribers this worker runs. None yet: the concrete ones (`notify`,
/// `webhook-dispatch`, `work-item-sync`, …) are added with their features.
fn registry() -> Registry {
    Registry::new()
}
// ── end C5 ──

/// What the runs of the tasks share.
#[derive(Clone)]
struct Shared {
    db: Db,
    jobs: Arc<JobRunner>,
    /// How long a job in progress may finish after a shutdown request.
    grace: Duration,
    shutdown: watch::Receiver<bool>,
    seal_keys: Arc<SealKeys>,
    /// The last seal left rows: the next one seals at once.
    seal_draining: Arc<AtomicBool>,
    // ── C5 ──
    /// The subscribers of the `subscribers` task.
    registry: Registry,
    // ── end C5 ──
}

impl Task {
    fn name(self) -> &'static str {
        match self {
            Self::Sequencer => "sequencer",
            Self::JobRunner => "job_runner",
            Self::JobReaper => "job_reaper",
            Self::AuditSeal => "audit_seal",
            Self::Subscribers => subscribers::NAME,
        }
    }

    fn period(self) -> Duration {
        match self {
            Self::Sequencer => SEQUENCER_PERIOD,
            Self::JobRunner => jobs::JOB_POLL_PERIOD,
            Self::JobReaper => jobs::REAP_PERIOD,
            Self::AuditSeal => seal::SEAL_POLL_PERIOD,
            Self::Subscribers => subscribers::PERIOD,
        }
    }

    /// The task as the supervisor runs it, with its own handles.
    fn runner(self, shared: &Shared) -> Runner {
        // ── C5: the dispatcher keeps state between runs ──
        if self == Self::Subscribers {
            return subscribers::runner(&shared.db, &shared.registry);
        }
        // ── end C5 ──
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
            Self::AuditSeal => seal::run_seal(db, &shared.seal_keys, &shared.seal_draining).await,
            // `runner` hands this task its own runner: reaching this is a bug.
            Self::Subscribers => Err(ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "worker_task")
                .with_context("task", self.name())
                .with_context("reason", "subscribers_without_dispatcher")),
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
/// job handlers (creating repositories under `storage`) and subscribers of
/// this build.
pub(crate) async fn run(
    db: &Db,
    keys: Arc<SealKeys>,
    storage: &Storage,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    let mut runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
    for handler in jobs::handlers(db, storage)? {
        runner.register(handler)?;
    }
    run_with(db, runner, registry(), jobs::SHUTDOWN_GRACE, keys, shutdown).await
}

/// [`run`] with a given job runner, subscribers and shutdown grace (tests).
pub(crate) async fn run_with(
    db: &Db,
    runner: JobRunner,
    registry: Registry,
    grace: Duration,
    keys: Arc<SealKeys>,
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
        seal_keys: keys,
        seal_draining: Arc::new(AtomicBool::new(false)),
        registry,
    };
    let runners = tasks(&shared.registry)
        .into_iter()
        .map(|task| task.runner(&shared))
        .collect();
    supervise(runners, MAX_BACKOFF, shutdown).await
}

#[cfg(test)]
mod jobs_tests;
#[cfg(test)]
pub(crate) mod loop_tests;
#[cfg(test)]
mod seal_tests;
// ── C5: the subscribers task's tests live in subscribers_tests.rs ──
#[cfg(test)]
mod tests;
