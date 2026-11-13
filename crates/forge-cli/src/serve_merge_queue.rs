//! The merge queue's worker in `forge serve` (M4 C8, ADR-0030 §13): like
//! the merge job (`serve_merges`), it writes the target branches, so it
//! runs next to the browse cache its ref writes invalidate.
//!
//! One task under the supervisor of `serve_merges` ([`supervise`]): it
//! ticks [`forge_review::MergeQueue`] — at once again after a queue was
//! worked, every [`IDLE_PERIOD`] while none is due. Each tick is bounded by
//! [`TICK_TIMEOUT`] (the queue's lease outlives it, so a tick cut short is
//! taken over by the next one). A failing tick is logged on the first
//! failure of a streak and then at most once a minute with its count
//! (the claim's own failures included), and backs off.

use std::sync::Arc;
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::MergeQueue;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::serve_merges::supervise;

/// How often an idle worker looks for a due queue.
const IDLE_PERIOD: Duration = Duration::from_secs(2);
/// The most one tick may take (git, a landing under the merge lock).
pub(crate) const TICK_TIMEOUT: Duration = Duration::from_secs(240);
/// The backoff of a failing tick and of a restarted task.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A failure streak is logged at most this often after its first line.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// The supervised task; [`QueueTasks::stop`] ends it.
pub(crate) struct QueueTasks {
    stop: watch::Sender<bool>,
    supervisor: JoinHandle<()>,
}

impl QueueTasks {
    /// Stop the worker and wait at most `wait` (a tick in progress keeps
    /// its lease: the next start takes the queue over after it expires).
    pub(crate) async fn stop(self, wait: Duration) {
        let _ = self.stop.send(true);
        match tokio::time::timeout(wait, self.supervisor).await {
            Ok(Ok(())) => {}
            Ok(Err(join)) => telemetry::log_failure(
                &ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("operation", "merge_queue_supervisor_exited")
                    .with_context("panicked", join.is_panic()),
                &RequestId::generate(),
            ),
            Err(_) => telemetry::log_failure(
                &ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("operation", "merge_queue_task_pending")
                    .with_context("wait_ms", wait.as_millis()),
                &RequestId::generate(),
            ),
        }
    }
}

/// Start the merge queue's worker under its supervisor.
pub(crate) fn spawn(queue: Arc<MergeQueue>) -> QueueTasks {
    let (stop, stopped) = watch::channel(false);
    tracing::info!(
        lease_s = queue.limits().lease.as_secs(),
        test_timeout_s = queue.limits().test_timeout.as_secs(),
        "forge.serve.merge_queue_started"
    );
    let supervisor = tokio::spawn(supervise(
        "merge_queue",
        stopped,
        (BACKOFF_MIN, BACKOFF_MAX),
        move |s| tokio::spawn(run(Arc::clone(&queue), s)),
    ));
    QueueTasks { stop, supervisor }
}

/// A failure streak (logged once at its start, then at most every
/// [`LOG_EVERY`]) and its backoff.
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
                &e.with_context("operation", "merge_queue_tick")
                    .with_context("streak", self.count),
                &RequestId::generate(),
            );
            self.logged_at = Some(tokio::time::Instant::now());
        }
        let shift = u32::try_from(self.count.saturating_sub(1).min(5)).unwrap_or(5);
        BACKOFF_MIN.saturating_mul(1 << shift).min(BACKOFF_MAX)
    }
}

async fn run(queue: Arc<MergeQueue>, mut stop: watch::Receiver<bool>) {
    let mut streak = Streak::default();
    loop {
        if *stop.borrow() {
            break;
        }
        let ticked = tokio::time::timeout(TICK_TIMEOUT, queue.tick()).await;
        let wait = match ticked {
            Ok(Ok(Some(_))) => {
                streak = Streak::default();
                continue;
            }
            Ok(Ok(None)) => {
                streak = Streak::default();
                IDLE_PERIOD
            }
            Ok(Err(e)) => streak.fail(e),
            Err(_) => streak.fail(
                ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("reason", "merge_queue_tick_timeout")
                    .with_context("timeout_ms", TICK_TIMEOUT.as_millis()),
            ),
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = stop.wait_for(|s| *s) => break,
        }
    }
    tracing::info!("forge.serve.merge_queue_stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_tick_backs_off_and_caps() {
        let mut s = Streak::default();
        let waits: Vec<Duration> = (0..8)
            .map(|_| s.fail(ForgeError::new(ErrorCode::DbUnavailable)))
            .collect();
        assert_eq!(waits[0], BACKOFF_MIN);
        assert_eq!(waits[1], Duration::from_secs(2));
        assert_eq!(waits.last().copied(), Some(BACKOFF_MAX));
        // The lease must outlive a tick, or two workers could hold a queue.
        assert!(forge_review::QueueLimits::default().lease > TICK_TIMEOUT);
    }
}
