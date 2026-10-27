//! `forge worker` — the background tasks of ADR-0016 §7, until SIGINT /
//! SIGTERM. Each [`Task`] runs in its own loop: one run, then a wait (its
//! period, or the backoff after a failure); shutdown is noticed between
//! runs, so an in-flight run (bounded by its own timeout) finishes first.
//!
//! Tasks today: the outbox sequencer (every 200 ms). The job runner (C6)
//! and the audit seal job (C8) become further [`Task`] variants with their
//! arm in [`Task::run`] and their entry in [`tasks`].
//!
//! Before the first run the database is connected and the schema checked
//! (ADR-0014): the worker never migrates.

use std::time::Duration;

use forge_core::config::DatabaseConfig;
use forge_core::{telemetry, RequestId};
use forge_db::sequencer::{self, AuditSequencing, BatchSize};
use forge_db::{Db, MIGRATOR};
use forge_errors::{ErrorCode, ForgeError};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::runtime::block_on;

mod backoff;

use backoff::{Backoff, Decision};

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
        result.map(|()| String::new())
    })
}

/// A background task of the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Task {
    /// `forge.assign_seq` + NOTIFY (forge-db `sequencer::run_once`).
    Sequencer,
}

/// Every task of this worker.
fn tasks() -> [Task; 1] {
    [Task::Sequencer]
}

impl Task {
    fn name(self) -> &'static str {
        match self {
            Self::Sequencer => "sequencer",
        }
    }

    fn period(self) -> Duration {
        match self {
            Self::Sequencer => SEQUENCER_PERIOD,
        }
    }

    /// One run, bounded by the callee's own timeout.
    async fn run(self, db: &Db) -> Result<(), ForgeError> {
        match self {
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
                Ok(())
            }
        }
    }
}

/// Run every task until `shutdown` turns true (or its sender is gone).
pub(crate) async fn run(db: &Db, shutdown: watch::Receiver<bool>) -> Result<(), ForgeError> {
    let mut set = JoinSet::new();
    for task in tasks() {
        let db = db.clone(); // clone: pool handles (Arc inside), one per task
        let shutdown = shutdown.clone(); // clone: one receiver per task
        set.spawn(task_loop(task, db, shutdown));
    }
    let mut outcome = Ok(());
    while let Some(joined) = set.join_next().await {
        if let Err(e) = joined {
            // A task panicked: stop the others and report it.
            set.abort_all();
            if outcome.is_ok() && !e.is_cancelled() {
                outcome = Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("operation", "worker_task")
                    .with_source(e));
            }
        }
    }
    outcome
}

async fn task_loop(task: Task, db: Db, mut shutdown: watch::Receiver<bool>) {
    let mut backoff = Backoff::new(task.period(), MAX_BACKOFF);
    // Logs of one failure streak share a request id.
    let mut streak_id: Option<RequestId> = None;
    let mut delay = Duration::ZERO;
    loop {
        if *shutdown.borrow() {
            return;
        }
        tokio::select! {
            // Cancel-safe: changed() only marks the value seen; sleep restarts.
            changed = shutdown.changed() => {
                if changed.is_err() {
                    return;
                }
                continue;
            }
            () = tokio::time::sleep(delay) => {}
        }
        let decision = match task.run(&db).await {
            Ok(()) => backoff.success(),
            Err(e) => {
                let d = backoff.failure(e.code());
                if d.log_failure {
                    let id = streak_id.get_or_insert_with(RequestId::generate);
                    let e = e
                        .with_context("task", task.name())
                        .with_context("retry_in_ms", d.delay.as_millis());
                    telemetry::log_failure(&e, id);
                }
                d
            }
        };
        log_recovery(task, &decision, &mut streak_id);
        delay = decision.delay;
    }
}

fn log_recovery(task: Task, decision: &Decision, streak_id: &mut Option<RequestId>) {
    if let Some(failures) = decision.recovered_after {
        let id = streak_id.take();
        tracing::info!(
            task = task.name(),
            failures,
            request_id = id.as_ref().map(|r| r.to_string()).unwrap_or_default(),
            "forge.worker.task_recovered"
        );
    }
}

#[cfg(test)]
mod tests;
