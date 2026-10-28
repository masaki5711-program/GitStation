//! The task loop and its supervisor, independent of what a task does: a
//! [`Runner`] is one run of a task (in production a [`super::Task`] on the
//! database, in tests a scripted closure).
//!
//! Each runner loops: wait (its period, or the backoff after a failure),
//! then run. Shutdown is noticed while waiting, also in the middle of a
//! backoff, so an in-flight run (bounded by its own timeout) finishes first
//! and nothing waits for the rest of a sleep. A dropped shutdown sender
//! without a shutdown request is an error, not a clean stop: nothing could
//! stop the worker any more. A task that fails (or panics) stops the others.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::backoff::{Backoff, Decision, Signature};

/// One run of a task.
pub(crate) type RunFuture = Pin<Box<dyn Future<Output = Result<(), ForgeError>> + Send>>;

/// A periodic task as the supervisor sees it.
pub(crate) struct Runner {
    pub name: &'static str,
    pub period: Duration,
    pub run: Box<dyn FnMut() -> RunFuture + Send>,
}

/// Run every runner until `shutdown` turns true. The first task error (a
/// closed shutdown channel, a panic) aborts the other tasks and is returned
/// for the caller to log once.
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
            Ok(()) => backoff.success(),
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
        tracing::info!(
            task,
            failures,
            request_id = id.as_ref().map(|r| r.to_string()).unwrap_or_default(),
            "forge.worker.task_recovered"
        );
    }
}
