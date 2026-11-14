//! The auto-merge worker in `forge serve` (M4 C9; plan 04 M-5; ADR-0017
//! Amendment 2026-11-13): it adds mergeable merge requests to the merge
//! queue as the auto-merge principal, so it runs next to the queue's worker.
//!
//! One task under the supervisor of `serve_merges` ([`supervise`]): it
//! ticks [`forge_review::AutoMerger`] every [`PERIOD`]. Each tick is
//! bounded by [`TICK_TIMEOUT`]; a failing tick (listing the merge requests)
//! is logged on the first failure of a streak and then at most once a
//! minute, and backs off. One merge request's failure is the tick's own
//! log line (`FG-REVIEW-0028`).

use std::sync::Arc;
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::AutoMerger;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::serve_merges::supervise;

/// How often the worker looks for due merge requests.
const PERIOD: Duration = Duration::from_secs(5);
/// The most one tick may take (git reads for each merge request).
pub(crate) const TICK_TIMEOUT: Duration = Duration::from_secs(240);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const LOG_EVERY: Duration = Duration::from_secs(60);

/// The supervised task; [`AutoMergeTasks::stop`] ends it.
pub(crate) struct AutoMergeTasks {
    stop: watch::Sender<bool>,
    supervisor: JoinHandle<()>,
}

impl AutoMergeTasks {
    /// Stop the worker and wait at most `wait`.
    pub(crate) async fn stop(self, wait: Duration) {
        let _ = self.stop.send(true);
        match tokio::time::timeout(wait, self.supervisor).await {
            Ok(Ok(())) => {}
            Ok(Err(join)) => telemetry::log_failure(
                &ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("operation", "auto_merge_supervisor_exited")
                    .with_context("panicked", join.is_panic()),
                &RequestId::generate(),
            ),
            Err(_) => telemetry::log_failure(
                &ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("operation", "auto_merge_task_pending")
                    .with_context("wait_ms", wait.as_millis()),
                &RequestId::generate(),
            ),
        }
    }
}

/// Start the auto-merge worker under its supervisor.
pub(crate) fn spawn(worker: Arc<AutoMerger>) -> AutoMergeTasks {
    let (stop, stopped) = watch::channel(false);
    tracing::info!(
        batch = worker.limits().batch,
        recheck_s = worker.limits().recheck.as_secs(),
        "forge.serve.auto_merge_started"
    );
    let supervisor = tokio::spawn(supervise(
        "auto_merge",
        stopped,
        (BACKOFF_MIN, BACKOFF_MAX),
        move |s| tokio::spawn(run(Arc::clone(&worker), s)),
    ));
    AutoMergeTasks { stop, supervisor }
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
                &e.with_context("operation", "auto_merge_tick")
                    .with_context("streak", self.count),
                &RequestId::generate(),
            );
            self.logged_at = Some(tokio::time::Instant::now());
        }
        let shift = u32::try_from(self.count.saturating_sub(1).min(6)).unwrap_or(6);
        BACKOFF_MIN.saturating_mul(1 << shift).min(BACKOFF_MAX)
    }
}

async fn run(worker: Arc<AutoMerger>, mut stop: watch::Receiver<bool>) {
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
                    .with_context("reason", "auto_merge_tick_timeout")
                    .with_context("timeout_ms", TICK_TIMEOUT.as_millis()),
            ),
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.wait_for(|s| *s) => break,
        }
    }
    tracing::info!("forge.serve.auto_merge_stopped");
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
