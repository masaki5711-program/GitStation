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
//! (`subscribers`, C5: woken by the sequencer's NOTIFY or the 5 s poll),
//! and the purge of projects whose grace period is over (`purge`, D5 MR4:
//! every `hosting.purge_interval_secs`, at once again after a full batch),
//! and the purge of web sessions 30 days past their end (`session_purge`,
//! M2 MR6: every `web.session_purge_interval_secs`, at once again after a
//! full batch), and the mergeability sweep (`mergeability_sweep`, M3 R2:
//! every 30 s, stale `checking` rows ended and deferred `unchecked` rows
//! re-requested), and the record of expired CODEOWNERS suspensions
//! (`codeowners_restore`, M3 R6: every 60 s), and the repository
//! maintenance (`maintenance`, M3 O3: its OWN job runner for
//! `repository.maintain` — the main runner never claims it, so a long gc
//! never delays another job — and the nightly schedule, every 60 s).
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

use forge_core::config::{Config, MaintenanceConfig, ReviewConfig, SealKeys};
use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::sequencer::{self, Assigned, AuditSequencing, BatchSize, SeqRange};
use forge_db::subscribers::Registry;
use forge_db::{Db, MIGRATOR};
use forge_errors::{ErrorCode, ForgeError};
#[cfg(test)]
use forge_notify::Sender;
use tokio::sync::watch;

use crate::runtime::block_on;

mod backoff;
mod jobs;
// ── D4 MR3b: repository provisioning ──
pub(crate) mod provision;
// ── end D4 MR3b ──
// ── D5 MR4: the purge ──
pub(crate) mod purge;
// ── end D5 MR4 ──
// ── M3 R1b: merge request versions ──
mod review;
// ── end M3 R1b ──
// ── M3 R2: stale and deferred mergeability checks ──
mod mergeability_sweep;
// ── end M3 R2 ──
// ── M3 R6: expired CODEOWNERS suspensions ──
mod codeowners_restore;
// ── end M3 R6 ──
// ── M3 O3: the repository maintenance (own runner + schedule) ──
pub(crate) mod maintenance;
// ── end M3 O3 ──
// ── M3 N1: the notification lane (the subscriber and its own job runner) ──
mod notify_lane;
// ── end M3 N1 ──
mod seal;
// ── M3 I1c: events retention ──
mod events_retention;
// ── end M3 I1c ──
// ── M2 MR6: the session purge ──
mod session_purge;
// ── end M2 MR6 ──
// ── C5: event subscribers ──
mod subscribers;
// ── end C5 ──
mod supervise;

use notify_lane::{NotifyLane, NotifyTask};
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
    // M3 O3: the prune grace against the merge window, before anything runs.
    maintenance::check_settings(&cfg.maintenance)?;
    // M3 N1: mail lane inputs (the lane itself is built after the database).
    let notify = cfg.notify.enabled.then(|| NotifyLane {
        cfg: cfg.notify.clone(), // clone: the lane owns its settings
        origin: cfg.web.base_url().map(|u| u.origin().to_owned()),
    });
    block_on(async {
        let db = Db::connect(database).await?;
        forge_db::check_schema(&db, &MIGRATOR).await?;
        // M3 O3: the database must know the window's time zone.
        forge_db::maintenance::local_minute(&db, &cfg.maintenance.time_zone).await?;
        let (stop, stopped) = watch::channel(false);
        let signal = tokio::spawn(async move {
            forge_web::server::shutdown_signal().await;
            // No receiver left means every task already ended: nothing to stop.
            let _ = stop.send(true);
        });
        tracing::info!(db_target = %db.target(), "forge.worker.started");
        let purger = purge::Purger::new(&db, storage.root.clone()); // clone: the purge's own handle
        let purge = Some(PurgeTask {
            purger,
            interval: cfg.hosting.purge_interval,
            session_interval: cfg.web.session_purge_interval,
        });
        let result = run_full(
            &db,
            keys,
            &storage,
            purge,
            (&cfg.review, Some(&cfg.maintenance), notify.as_ref()),
            stopped,
        )
        .await;
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
    // ── D5 MR4 ──
    /// The purge of expired projects (`purge::run_purge`).
    Purge,
    // ── end D5 MR4 ──
    /// The purge of ended web sessions (`session_purge`, M2 MR6).
    SessionPurge,
    /// Stale and deferred mergeability checks (`mergeability_sweep`, M3 R2).
    MergeabilitySweep,
    /// Expired CODEOWNERS suspensions recorded (`codeowners_restore`, M3 R6).
    CodeownersRestore,
    /// Events past the retention period (`events_retention`, M3 I1c).
    EventsRetention,
    // ── M3 O3 ──
    /// The maintenance's own job runner (`maintenance`).
    MaintenanceRunner,
    /// The nightly maintenance schedule (`maintenance::run_schedule`).
    MaintenanceSchedule,
    // ── end M3 O3 ──
    // ── M3 N1 ──
    /// The notification lane's own job runner (`notify_lane`).
    NotifyRunner,
    // ── end M3 N1 ──
}

/// The maintenance lane (M3 O3): its own job runner and its settings.
#[derive(Clone)]
pub(crate) struct MaintenanceTask {
    pub(crate) runner: Arc<JobRunner>,
    pub(crate) cfg: MaintenanceConfig,
}

/// The purge tasks' handles and periods (`hosting.purge_interval_secs`;
/// M2 MR6: the session purge's `web.session_purge_interval_secs`).
#[derive(Clone)]
pub(crate) struct PurgeTask {
    pub(crate) purger: purge::Purger,
    pub(crate) interval: Duration,
    pub(crate) session_interval: Duration,
}

/// Every task of this worker.
#[cfg(test)]
fn tasks(
    registry: &Registry,
    purge: Option<&PurgeTask>,
    maintenance: Option<&MaintenanceTask>,
) -> Vec<Task> {
    tasks_with(registry, purge, maintenance, None)
}

/// [`tasks`] with the notification lane's runner when given (M3 N1).
fn tasks_with(
    registry: &Registry,
    purge: Option<&PurgeTask>,
    maintenance: Option<&MaintenanceTask>,
    notify: Option<&NotifyTask>,
) -> Vec<Task> {
    let mut tasks = vec![
        Task::Sequencer,
        Task::JobRunner,
        Task::JobReaper,
        Task::AuditSeal,
        // M3 R2: needs only the database.
        Task::MergeabilitySweep,
        // M3 R6: needs only the database.
        Task::CodeownersRestore,
        // M3 I1c: needs only the database.
        Task::EventsRetention,
    ];
    // ── C5: no subscriber, no task (and no LISTEN connection) ──
    if !registry.is_empty() {
        tasks.push(Task::Subscribers);
    }
    // ── end C5 ──
    // ── D5 MR4: the purge needs the storage root (not in the loop tests) ──
    if purge.is_some() {
        tasks.push(Task::Purge);
        tasks.push(Task::SessionPurge);
    }
    // ── M3 O3: only with the storage (not in the loop tests) ──
    if maintenance.is_some() {
        tasks.push(Task::MaintenanceRunner);
        tasks.push(Task::MaintenanceSchedule);
    }
    // ── M3 N1: only with the notification lane (notify.enabled) ──
    if notify.is_some() {
        tasks.push(Task::NotifyRunner);
    }
    tasks
}

// ── C5: the registered subscribers ──
/// The subscribers this worker runs: `merge-request-refresh` (M3 R1b, the
/// first in production: enqueues the version job on `repository.pushed`;
/// registered at the head on the first start, ADR-0016 §1; M3 R2: it also
/// enqueues the mergeability checks of a target push, at most
/// `review.mergeability_fanout_max`) and `work-item-sync` (M3 I1b,
/// `ordered`: the work item links and the events' work items; it stops and
/// alerts on an event it cannot process). The others (`notify`,
/// `webhook-dispatch`, …) join with their features.
#[cfg(test)]
fn registry(review: &ReviewConfig) -> Result<Registry, ForgeError> {
    registry_with(review, None)
}

/// [`registry`] with the `notify` subscriber when the lane exists.
fn registry_with(
    review: &ReviewConfig,
    notify: Option<&NotifyTask>,
) -> Result<Registry, ForgeError> {
    let registry = Registry::new()
        .with(Arc::new(forge_db::review::MergeRequestRefresh::new(
            review.mergeability_fanout_max,
        )))?
        .with(Arc::new(forge_db::work_items::WorkItemSync))?;
    // M3 N1 (`independent`): merge request events become notifications
    // when mail is configured; without it nothing is recorded at all (no
    // backlog that nothing would send).
    match notify {
        Some(task) => {
            let subscriber: Arc<dyn forge_db::subscribers::Subscriber> =
                Arc::clone(&task.subscriber) as Arc<dyn forge_db::subscribers::Subscriber>;
            registry.with(subscriber)
        }
        None => Ok(registry),
    }
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
    // ── D5 MR4 ──
    purge: Option<PurgeTask>,
    // ── end D5 MR4 ──
    /// The repositories (M3 R2: the mergeability sweep reads target tips;
    /// `None` in the loop tests, which then skip that part).
    storage: Option<Storage>,
    /// The maintenance lane (M3 O3); `None` in the loop tests.
    maintenance: Option<MaintenanceTask>,
    /// The notification lane (M3 N1); `None` unless `notify.enabled`.
    notify: Option<NotifyTask>,
}

impl Task {
    fn name(self) -> &'static str {
        match self {
            Self::Sequencer => "sequencer",
            Self::JobRunner => "job_runner",
            Self::JobReaper => "job_reaper",
            Self::AuditSeal => "audit_seal",
            Self::Subscribers => subscribers::NAME,
            Self::Purge => purge::NAME,
            Self::SessionPurge => session_purge::NAME,
            Self::MergeabilitySweep => mergeability_sweep::NAME,
            Self::CodeownersRestore => codeowners_restore::NAME,
            Self::EventsRetention => events_retention::NAME,
            Self::MaintenanceRunner => maintenance::RUNNER_NAME,
            Self::MaintenanceSchedule => maintenance::SCHEDULE_NAME,
            Self::NotifyRunner => notify_lane::RUNNER_NAME,
        }
    }

    fn period(self, shared: &Shared) -> Duration {
        match self {
            Self::Sequencer => SEQUENCER_PERIOD,
            Self::JobRunner => jobs::JOB_POLL_PERIOD,
            Self::JobReaper => jobs::REAP_PERIOD,
            Self::AuditSeal => seal::SEAL_POLL_PERIOD,
            Self::Subscribers => subscribers::PERIOD,
            // Without a purge configuration the task is not started (`tasks`).
            Self::Purge => shared
                .purge
                .as_ref()
                .map_or(MAX_BACKOFF, |purge| purge.interval),
            Self::SessionPurge => shared
                .purge
                .as_ref()
                .map_or(MAX_BACKOFF, |purge| purge.session_interval),
            Self::MergeabilitySweep => mergeability_sweep::PERIOD,
            Self::CodeownersRestore => codeowners_restore::PERIOD,
            Self::EventsRetention => events_retention::PERIOD,
            Self::MaintenanceRunner => jobs::JOB_POLL_PERIOD,
            Self::MaintenanceSchedule => maintenance::SCHEDULE_PERIOD,
            Self::NotifyRunner => jobs::JOB_POLL_PERIOD,
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
            period: self.period(&shared),
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
            Self::Purge => match shared.purge.as_ref() {
                Some(purge) => purge::run_purge(&purge.purger).await,
                // `tasks` starts it only with a configuration: reaching this is a bug.
                None => Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("operation", "worker_task")
                    .with_context("task", self.name())
                    .with_context("reason", "purge_unconfigured")),
            },
            Self::SessionPurge => session_purge::run_session_purge(db).await,
            Self::MergeabilitySweep => {
                mergeability_sweep::run_mergeability_sweep(db, shared.storage.as_ref()).await
            }
            Self::CodeownersRestore => codeowners_restore::run_codeowners_restore(db).await,
            Self::EventsRetention => events_retention::run_events_retention(db).await,
            Self::MaintenanceRunner => match shared.maintenance.as_ref() {
                Some(m) => {
                    let stop = shared.shutdown.clone(); // clone: the run waits on its own receiver
                    jobs::run_job(&m.runner, shared.grace, stop).await
                }
                None => Err(self.unconfigured()),
            },
            Self::MaintenanceSchedule => match shared.maintenance.as_ref() {
                Some(m) => maintenance::run_schedule(db, &m.cfg).await,
                None => Err(self.unconfigured()),
            },
            Self::NotifyRunner => match shared.notify.as_ref() {
                Some(n) => {
                    let stop = shared.shutdown.clone(); // clone: the run waits on its own receiver
                    jobs::run_job(&n.runner, shared.grace, stop).await
                }
                None => Err(self.unconfigured()),
            },
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

impl Task {
    /// `tasks` starts the maintenance and notification tasks only with
    /// their lane: reaching this is a bug.
    fn unconfigured(self) -> ForgeError {
        ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "worker_task")
            .with_context("task", self.name())
            .with_context("reason", "lane_unconfigured")
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
/// job handlers (creating repositories under `storage`), the subscribers of
/// this build and, when given, the purge (D5 MR4).
#[cfg(test)]
pub(crate) async fn run(
    db: &Db,
    keys: Arc<SealKeys>,
    storage: &Storage,
    purge: Option<PurgeTask>,
    review: &ReviewConfig,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    run_full(db, keys, storage, purge, (review, None, None), shutdown).await
}

/// [`run`] with the maintenance lane when `maintenance` is given (M3 O3:
/// `forge worker` always gives it; the loop tests do not).
pub(crate) async fn run_full(
    db: &Db,
    keys: Arc<SealKeys>,
    storage: &Storage,
    purge: Option<PurgeTask>,
    (review, maintenance, notify): (
        &ReviewConfig,
        Option<&MaintenanceConfig>,
        Option<&NotifyLane>,
    ),
    shutdown: watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    let mut runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
    for handler in jobs::handlers(db, storage)? {
        runner.register(handler)?;
    }
    let maintenance = maintenance
        .map(|cfg| {
            Ok::<_, ForgeError>(MaintenanceTask {
                runner: Arc::new(maintenance::runner(db, storage, cfg)?),
                cfg: cfg.clone(), // clone: the lane owns its settings
            })
        })
        .transpose()?;
    // M3 N1: the relay's CA file and password are read now.
    let notify = notify
        .map(|lane| NotifyTask::new(db, lane, None))
        .transpose()?;
    run_all(
        db,
        runner,
        registry_with(review, notify.as_ref())?,
        jobs::SHUTDOWN_GRACE,
        keys,
        (purge, Some(storage.clone()), maintenance, notify), // clone: the sweep's own handles
        shutdown,
    )
    .await
}

/// [`run_with`] with the notification lane built on `sender` (tests).
#[cfg(test)]
pub(crate) async fn run_with_notify(
    db: &Db,
    lane: &NotifyLane,
    sender: Option<Arc<dyn Sender>>,
    keys: Arc<SealKeys>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    let notify = NotifyTask::new(db, lane, sender)?;
    let runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
    run_all(
        db,
        runner,
        registry_with(&ReviewConfig::default(), Some(&notify))?,
        Duration::from_secs(5),
        keys,
        (None, None, None, Some(notify)),
        shutdown,
    )
    .await
}

/// [`run`] with a given job runner, subscribers and shutdown grace, and no
/// purge (tests).
#[cfg(test)]
pub(crate) async fn run_with(
    db: &Db,
    runner: JobRunner,
    registry: Registry,
    grace: Duration,
    keys: Arc<SealKeys>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    run_all(
        db,
        runner,
        registry,
        grace,
        keys,
        (None, None, None, None),
        shutdown,
    )
    .await
}

async fn run_all(
    db: &Db,
    runner: JobRunner,
    registry: Registry,
    grace: Duration,
    keys: Arc<SealKeys>,
    // The purge (D5 MR4), the repositories (M3 R2 sweep) and the
    // maintenance lane (M3 O3), when given.
    (purge, storage, maintenance, notify): (
        Option<PurgeTask>,
        Option<Storage>,
        Option<MaintenanceTask>,
        Option<NotifyTask>,
    ),
    shutdown: watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    tracing::info!(
        worker_id = runner.worker_id().as_str(),
        job_kinds = runner.kinds().len(),
        "forge.worker.jobs"
    );
    if let Some(m) = &maintenance {
        tracing::info!(
            worker_id = m.runner.worker_id().as_str(),
            job_kinds = m.runner.kinds().len(),
            "forge.worker.maintenance_jobs"
        );
    }
    let shared = Shared {
        db: db.clone(), // clone: pool handles
        jobs: Arc::new(runner),
        grace,
        shutdown: shutdown.clone(), // clone: the job runner's own receiver
        seal_keys: keys,
        seal_draining: Arc::new(AtomicBool::new(false)),
        registry,
        purge,
        storage,
        maintenance,
        notify,
    };
    let runners = tasks_with(
        &shared.registry,
        shared.purge.as_ref(),
        shared.maintenance.as_ref(),
        shared.notify.as_ref(),
    )
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
mod notify_tests;
#[cfg(test)]
mod seal_tests;
// ── C5: the subscribers task's tests live in subscribers_tests.rs ──
#[cfg(test)]
mod tests;
