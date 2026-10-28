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
//! full batch so a backlog drains without a pause) and, when any subscriber
//! is registered, the event subscribers (`subscribers`, C5: woken by the
//! sequencer's NOTIFY or the 5 s poll). The job runner (C6)
//! and the audit seal job (C8) become further [`Task`] variants with their
//! arm in [`Task::runner`] and their entry in [`tasks`].
//!
//! Before the first run the database is connected and the schema checked
//! (ADR-0014): the worker never migrates.

use std::time::Duration;

use forge_core::config::DatabaseConfig;
use forge_db::sequencer::{self, Assigned, AuditSequencing, BatchSize, SeqRange};
use forge_db::subscribers::Registry;
use forge_db::{Db, MIGRATOR};
use forge_errors::ForgeError;
use tokio::sync::watch;

use crate::runtime::block_on;

mod backoff;
// ── C5: event subscribers ──
mod subscribers;
// ── end C5 ──
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
    // ── C5: event subscribers ──
    /// Every registered subscriber (forge-db `subscribers::Dispatcher`).
    Subscribers,
    // ── end C5 ──
}

/// Every task of this worker.
fn tasks(registry: &Registry) -> Vec<Task> {
    let mut tasks = vec![Task::Sequencer];
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

impl Task {
    /// The task as the supervisor runs it, with its own pool handles.
    fn runner(self, db: &Db, registry: &Registry) -> Runner {
        match self {
            Self::Sequencer => {
                let db = db.clone(); // clone: pool handles (Arc inside), one per task
                Runner {
                    name: "sequencer",
                    period: SEQUENCER_PERIOD,
                    run: Box::new(move || {
                        let db = db.clone(); // clone: each run future owns a pool handle
                        Box::pin(async move { sequence(&db).await })
                    }),
                }
            }
            // ── C5 ──
            Self::Subscribers => subscribers::runner(db, registry),
            // ── end C5 ──
        }
    }
}

/// One sequencer run, bounded by `sequencer::run_once`'s own timeout.
async fn sequence(db: &Db) -> Result<Next, ForgeError> {
    // Audit rows are not numbered yet: ADR-0018 §5 allows it only after
    // this process's own sink comparison succeeded, and that per-process
    // state arrives in C9. Events are numbered.
    let assigned = sequencer::run_once(db, BatchSize::MAX, AuditSequencing::Skip).await?;
    if let Some(events) = assigned.events {
        tracing::debug!(
            task = "sequencer",
            from_seq = events.first(),
            to_seq = events.last(),
            "forge.worker.sequenced"
        );
    }
    Ok(after_sequencing(assigned, BatchSize::MAX))
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

/// Run every task until `shutdown` turns true (see `supervise`).
pub(crate) async fn run(db: &Db, shutdown: watch::Receiver<bool>) -> Result<(), ForgeError> {
    run_with(db, &registry(), shutdown).await
}

/// [`run`] with the given subscribers (tests register their own).
pub(crate) async fn run_with(
    db: &Db,
    registry: &Registry,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    let runners = tasks(registry)
        .into_iter()
        .map(|task| task.runner(db, registry))
        .collect();
    supervise(runners, MAX_BACKOFF, shutdown).await
}

#[cfg(test)]
mod loop_tests;
#[cfg(test)]
mod tests;
