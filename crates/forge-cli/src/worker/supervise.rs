//! The task loop and its supervisor, independent of what a task does: a
//! [`Runner`] is one run of a task (in production a [`super::Task`] on the
//! database, in tests a scripted closure).
//!
//! Each runner loops: run, then wait — its period, nothing when the run
//! returned [`Next::Immediately`] (a bounded number of times in a row, see
//! `backoff`), or the backoff after a failure — then run again. A failed
//! run is logged by the failure policy (`backoff`) and retried; it never
//! stops its task or the others.
//!
//! Shutdown is noticed only while waiting, also in the middle of a backoff,
//! so nothing waits for the rest of a sleep; a run in progress when shutdown
//! is requested finishes first (bounded by its own timeout). A dropped
//! shutdown sender without a shutdown request is an error, not a clean
//! stop: nothing could stop the worker any more.
//!
//! Only two things end a task with an error: that closed shutdown channel
//! and a panic. Either one aborts every other task (`JoinSet::abort_all`),
//! which cancels a sibling's run mid-run at its next `.await` — hence the
//! cancel-safety requirement of the [`Runner`] contract.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;

pub(crate) use super::backoff::Next;
use super::backoff::{Backoff, Decision, Signature};

/// One run of a task; on success it says when the next run should start.
pub(crate) type RunFuture = Pin<Box<dyn Future<Output = Result<Next, ForgeError>> + Send>>;

/// A periodic task as the supervisor sees it.
///
/// Contract for `run`:
/// - A run must be cancel-safe (dropping it mid-run must leave no partial
///   state): the supervisor aborts every task at its next `.await` when one
///   panics or loses its shutdown channel, and the process may stop at any
///   time. Work that must be all-or-nothing belongs in one database
///   transaction, which a dropped connection rolls back.
/// - A run bounds itself with its own timeout: shutdown waits for a run in
///   progress.
/// - A failure is returned, never swallowed: the supervisor logs it (with
///   `task`, `failures`, `retry_in_ms`) and retries after a backoff.
pub(crate) struct Runner {
    pub name: &'static str,
    pub period: Duration,
    pub run: Box<dyn FnMut() -> RunFuture + Send>,
}

/// Run every runner until `shutdown` turns true. A failed run is retried
/// inside its task; only a task error (a closed shutdown channel, a panic)
/// ends a task early. The first one aborts the other tasks — cancelling a
/// run in progress — and is returned for the caller to log once.
pub(crate) async fn supervise(
    runners: Vec<Runner>,
    max_backoff: Duration,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    let mut set = JoinSet::new();
    let mut names = HashMap::new();
    for runner in runners {
        let name = runner.name;
        let shutdown = shutdown.clone(); // clone: one receiver per task
        let handle = set.spawn(task_loop(runner, max_backoff, shutdown));
        names.insert(handle.id(), name);
    }
    let mut outcome = Ok(());
    while let Some(joined) = set.join_next_with_id().await {
        let failed = match joined {
            Ok((_, Ok(()))) => None,
            Ok((_, Err(e))) => Some(e),
            // Aborted by us after another task failed.
            Err(e) if e.is_cancelled() => None,
            Err(e) => {
                let task = names.get(&e.id()).copied().unwrap_or("unknown");
                Some(
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("operation", "worker_task")
                        .with_context("task", task)
                        .with_context("reason", "task_panicked")
                        .with_source(e),
                )
            }
        };
        if let Some(e) = failed {
            set.abort_all();
            if outcome.is_ok() {
                outcome = Err(e);
            }
        }
    }
    outcome
}

async fn task_loop(
    mut runner: Runner,
    max_backoff: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ForgeError> {
    let mut backoff = Backoff::new(runner.period, max_backoff);
    // Logs of one failure streak share a request id.
    let mut streak_id: Option<RequestId> = None;
    let mut delay = Duration::ZERO;
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        tokio::select! {
            // Cancel-safe: changed() only marks the value seen; sleep restarts.
            changed = shutdown.changed() => {
                // A closed channel after a shutdown request is that request
                // (the loop head sees it); without one it is an error.
                if changed.is_err() && !*shutdown.borrow() {
                    return Err(ForgeError::new(ErrorCode::Internal)
                        .with_context("operation", "worker_shutdown")
                        .with_context("task", runner.name)
                        .with_context("reason", "shutdown_channel_closed"));
                }
                continue;
            }
            () = tokio::time::sleep(delay) => {}
        }
        let decision = match (runner.run)().await {
            Ok(next) => backoff.success(next),
            Err(e) => {
                let d = backoff.failure(Signature::of(&e), Instant::now());
                if d.log_failure {
                    let id = streak_id.get_or_insert_with(RequestId::generate);
                    let e = e
                        .with_context("task", runner.name)
                        .with_context("failures", d.failures)
                        .with_context("retry_in_ms", d.delay.as_millis());
                    telemetry::log_failure(&e, id);
                }
                d
            }
        };
        log_recovery(runner.name, &decision, &mut streak_id);
        delay = decision.delay;
    }
}

fn log_recovery(task: &'static str, decision: &Decision, streak_id: &mut Option<RequestId>) {
    if let Some(failures) = decision.recovered_after {
        let id = streak_id.take();
        // The field is left out (not empty) when the streak has no id.
        tracing::info!(
            task,
            failures,
            request_id = id.as_ref().map(tracing::field::display),
            "forge.worker.task_recovered"
        );
    }
}
