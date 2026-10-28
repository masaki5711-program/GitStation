//! `forge worker` — the background tasks of ADR-0016 §7, until SIGINT /
//! SIGTERM. Each [`Task`] runs in its own loop (`supervise`): one run, then
//! a wait (its period, or the backoff after a failure); shutdown is noticed
//! while waiting, so an in-flight run (bounded by its own timeout) finishes
//! first. Failure logging and backoff: `backoff`.
//!
//! Tasks today: the outbox sequencer (every 200 ms). The job runner (C6)
//! and the audit seal job (C8) become further [`Task`] variants with their
//! arm in [`Task::run`] and their entry in [`tasks`].
//!
//! Before the first run the database is connected and the schema checked
//! (ADR-0014): the worker never migrates.

use std::time::Duration;

use forge_core::config::DatabaseConfig;
use forge_db::sequencer::{self, AuditSequencing, BatchSize};
use forge_db::{Db, MIGRATOR};
use forge_errors::ForgeError;
use tokio::sync::watch;

use crate::runtime::block_on;

mod backoff;
mod supervise;

use supervise::{supervise, Runner};

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

    /// The task as the supervisor runs it, with its own pool handles.
    fn runner(self, db: &Db) -> Runner {
        let db = db.clone(); // clone: pool handles (Arc inside), one per task
        Runner {
            name: self.name(),
            period: self.period(),
            run: Box::new(move || {
                let db = db.clone(); // clone: each run future owns a pool handle
                Box::pin(async move { self.run(&db).await })
            }),
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

/// Run every task until `shutdown` turns true (see `supervise`).
pub(crate) async fn run(db: &Db, shutdown: watch::Receiver<bool>) -> Result<(), ForgeError> {
    let runners = tasks().into_iter().map(|task| task.runner(db)).collect();
    supervise(runners, MAX_BACKOFF, shutdown).await
}

#[cfg(test)]
mod loop_tests;
#[cfg(test)]
mod tests;
