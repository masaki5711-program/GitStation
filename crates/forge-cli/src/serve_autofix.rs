//! The deterministic autofix worker in `forge serve` (M4 C14, ADR-0017
//! Amendment C14): it writes the source branches of merge requests (the
//! bot's commit), so it runs next to the browse cache that write
//! invalidates — as the merges and the revert drafts.
//!
//! One task under the supervisor of `serve_merges` ([`supervise`]): it
//! ticks [`forge_review::Autofixer`] every [`PERIOD`]. Each tick is bounded
//! by [`TICK_TIMEOUT`]; a failing tick (listing the attempts) is logged on
//! the first failure of a streak and then at most once a minute, and backs
//! off. One attempt's failure is its own log line (`forge.autofix.*`).

use std::sync::Arc;
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::Autofixer;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::serve_merges::supervise;

/// How often the worker looks for attempts to start or land.
const PERIOD: Duration = Duration::from_secs(5);
/// The most one tick may take (git work for each attempt of the batch).
pub(crate) const TICK_TIMEOUT: Duration = Duration::from_secs(600);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const LOG_EVERY: Duration = Duration::from_secs(60);

/// The supervised task; [`AutofixTasks::stop`] ends it.
pub(crate) struct AutofixTasks {
    stop: watch::Sender<bool>,
    supervisor: JoinHandle<()>,
}

impl AutofixTasks {
    /// Stop the worker and wait at most `wait` (an attempt in the middle
    /// of its landing finishes or is picked up again: every step is
    /// idempotent on the attempt's row).
    pub(crate) async fn stop(self, wait: Duration) {
        let _ = self.stop.send(true);
        match tokio::time::timeout(wait, self.supervisor).await {
            Ok(Ok(())) => {}
            Ok(Err(join)) => telemetry::log_failure(
                &ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("operation", "autofix_supervisor_exited")
                    .with_context("panicked", join.is_panic()),
                &RequestId::generate(),
            ),
            Err(_) => telemetry::log_failure(
                &ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("operation", "autofix_task_pending")
                    .with_context("wait_ms", wait.as_millis()),
                &RequestId::generate(),
            ),
        }
    }
}

/// Start the autofix worker under its supervisor.
pub(crate) fn spawn(worker: Arc<Autofixer>) -> AutofixTasks {
    let (stop, stopped) = watch::channel(false);
    tracing::info!(batch = worker.limits().batch, "forge.serve.autofix_started");
    let supervisor = tokio::spawn(supervise(
        "autofix",
        stopped,
        (BACKOFF_MIN, BACKOFF_MAX),
        move |s| tokio::spawn(run(Arc::clone(&worker), s)),
    ));
    AutofixTasks { stop, supervisor }
}

#[derive(Default)]
struct Streak {
    count: u64,
    logged_at: Option<tokio::time::Instant>,
}

impl Streak {
    fn fail(&mut self, e: ForgeError) -> Duration {
        self.count += 1;
        if self.logged_at.is_none_or(|at| at.elapsed() >= LOG_EVERY) {
            telemetry::log_failure(
                &e.with_context("operation", "autofix_tick")
                    .with_context("streak", self.count),
                &RequestId::generate(),
            );
            self.logged_at = Some(tokio::time::Instant::now());
        }
        let shift = u32::try_from(self.count.saturating_sub(1).min(6)).unwrap_or(6);
        BACKOFF_MIN.saturating_mul(1 << shift).min(BACKOFF_MAX)
    }
}

async fn run(worker: Arc<Autofixer>, mut stop: watch::Receiver<bool>) {
    let mut streak = Streak::default();
    loop {
        if *stop.borrow() {
            break;
        }
        let ticked = tokio::time::timeout(TICK_TIMEOUT, worker.tick()).await;
        let wait = match ticked {
            Ok(Ok(_)) => {
                streak = Streak::default();
                PERIOD
            }
            Ok(Err(e)) => streak.fail(e),
            Err(_) => streak.fail(
                ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("reason", "autofix_tick_timeout")
                    .with_context("timeout_ms", TICK_TIMEOUT.as_millis()),
            ),
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.wait_for(|s| *s) => break,
        }
    }
    tracing::info!("forge.serve.autofix_stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_tick_backs_off_and_caps() {
        let mut s = Streak::default();
        let waits: Vec<Duration> = (0..9)
            .map(|_| s.fail(ForgeError::new(ErrorCode::DbUnavailable)))
            .collect();
        assert_eq!(waits[0], BACKOFF_MIN);
        assert_eq!(waits.last().copied(), Some(BACKOFF_MAX));
    }
}
