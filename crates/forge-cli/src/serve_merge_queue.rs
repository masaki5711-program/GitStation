//! The merge queue's worker in `forge serve` (M4 C8, ADR-0030 §13): like
//! the merge job (`serve_merges`), it writes the target branches, so it
//! runs next to the browse cache its ref writes invalidate.
//!
//! `merge_queue.workers` turn tasks (plan 18 S3, ADR-0037 (a)), each under
//! its own supervisor of `serve_merges` ([`supervise`]): a task ticks its
//! [`forge_review::QueueTurn`] — at once again after a queue was worked —
//! and, while no queue is due, waits for a wake (`forge_merge_queue`), the
//! next due time or the safety-net period (`merge_queue.safety_poll_ms`)
//! instead of polling. Different queues progress on different tasks; one
//! queue is one task's at a time (the claim's lease — every task of every
//! process may wake for it, one steps it). Each tick is bounded by
//! [`TICK_TIMEOUT`] (the queue's lease outlives it, so a tick cut short is
//! taken over by the next one). A failing tick or wait is logged on the
//! first failure of a streak and then at most once a minute with its count
//! (the claim's own failures included), and backs off.

use std::time::Duration;

use forge_core::config::MergeQueueConfig;
use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{MergeQueue, QueueLimits, QueueTurn};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::serve_merges::supervise;

/// The most one tick may take (git, a landing under the merge lock).
pub(crate) const TICK_TIMEOUT: Duration = Duration::from_secs(240);
/// The backoff of a failing tick and of a restarted task.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A failure streak is logged at most this often after its first line.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// The supervised turn tasks; [`QueueTasks::stop`] ends them.
pub(crate) struct QueueTasks {
    stop: watch::Sender<bool>,
    supervisors: Vec<JoinHandle<()>>,
}

impl QueueTasks {
    /// Stop the turn tasks and wait at most `wait` for all of them (a tick
    /// in progress is not cut: it runs to its end within `wait`, and past
    /// it keeps its lease — the next start takes the queue over after it
    /// expires). An idle task ends at once.
    pub(crate) async fn stop(self, wait: Duration) {
        let _ = self.stop.send(true);
        let deadline = tokio::time::Instant::now() + wait;
        for (turn, supervisor) in self.supervisors.into_iter().enumerate() {
            match tokio::time::timeout_at(deadline, supervisor).await {
                Ok(Ok(())) => {}
                Ok(Err(join)) => telemetry::log_failure(
                    &ForgeError::new(ErrorCode::ServeFailed)
                        .with_context("operation", "merge_queue_supervisor_exited")
                        .with_context("turn", turn)
                        .with_context("panicked", join.is_panic()),
                    &RequestId::generate(),
                ),
                Err(_) => telemetry::log_failure(
                    &ForgeError::new(ErrorCode::ServeFailed)
                        .with_context("operation", "merge_queue_task_pending")
                        .with_context("turn", turn)
                        .with_context("wait_ms", wait.as_millis()),
                    &RequestId::generate(),
                ),
            }
        }
    }
}

/// The queue's limits with `merge_queue.*`'s safety nets, batch bounds
/// (plan 18 S7) and speculation depth (plan 18 S9).
fn limits_of(base: QueueLimits, cfg: &MergeQueueConfig) -> QueueLimits {
    QueueLimits {
        idle: cfg.safety_poll(),
        poll: cfg.waiting_safety_poll(),
        doc_batch_max: usize::try_from(cfg.doc_batch_max).unwrap_or(base.doc_batch_max),
        ci_batch_max: usize::try_from(cfg.ci_batch_max).unwrap_or(base.ci_batch_max),
        speculation_depth: usize::try_from(cfg.speculation_depth).unwrap_or(base.speculation_depth),
        ..base
    }
}

/// Start the merge queue's turn tasks, each under its supervisor.
pub(crate) fn spawn(queue: MergeQueue, cfg: &MergeQueueConfig) -> QueueTasks {
    let limits = limits_of(queue.limits(), cfg);
    let queue = queue.with_limits(limits);
    let workers = usize::try_from(cfg.workers).unwrap_or(1).max(1);
    let (stop, stopped) = watch::channel(false);
    // Plan 18 S1: `train_lint=off` is `merge_queue.train_lint_budget_ms = 0`
    // — trains land without the document lint of their tree.
    let (train_lint, train_lint_budget_ms) =
        train_lint_fields(queue.lints_trains(), limits.train_lint_budget);
    tracing::info!(
        lease_s = limits.lease.as_secs(),
        test_timeout_s = limits.test_timeout.as_secs(),
        train_lint,
        train_lint_budget_ms,
        workers,
        doc_batch_max = limits.doc_batch_max,
        ci_batch_max = limits.ci_batch_max,
        speculation_depth = limits.speculation_depth,
        safety_poll_ms = limits.idle.as_millis(),
        waiting_safety_poll_ms = limits.poll.as_millis(),
        "forge.serve.merge_queue_started"
    );
    let supervisors = (0..workers)
        .map(|n| {
            let queue = queue.clone(); // clone: handles; one per supervisor
            tokio::spawn(supervise(
                "merge_queue",
                stopped.clone(), // clone: each supervisor watches the stop
                (BACKOFF_MIN, BACKOFF_MAX),
                // A restarted task starts a new turn (a new waiter).
                move |s| tokio::spawn(run(queue.turn(n), s)),
            ))
        })
        .collect();
    QueueTasks { stop, supervisors }
}

/// `train_lint` and `train_lint_budget_ms` of the start line: `off` and 0
/// when no lint is injected, whatever the limits say.
fn train_lint_fields(lints: bool, budget: Duration) -> (&'static str, u128) {
    if lints {
        ("on", budget.as_millis())
    } else {
        ("off", 0)
    }
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

async fn run(mut turn: QueueTurn, mut stop: watch::Receiver<bool>) {
    let mut streak = Streak::default();
    loop {
        if *stop.borrow() {
            break;
        }
        // Never cut by the stop: a landing in progress runs to its end.
        let ticked = tokio::time::timeout(TICK_TIMEOUT, turn.tick()).await;
        let failed = match ticked {
            Ok(Ok(Some(_))) => {
                streak = Streak::default();
                continue;
            }
            Ok(Ok(None)) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some(
                ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("reason", "merge_queue_tick_timeout")
                    .with_context("timeout_ms", TICK_TIMEOUT.as_millis()),
            ),
        };
        let failed = match failed {
            Some(e) => Some(e),
            // No queue is due: a wake, the next due time or the safety net.
            None => tokio::select! {
                idled = turn.idle() => match idled {
                    Ok(_) => {
                        streak = Streak::default();
                        None
                    }
                    Err(e) => Some(e.with_context("step", "idle_wait")),
                },
                _ = stop.wait_for(|s| *s) => break,
            },
        };
        if let Some(e) = failed {
            let wait = streak.fail(e.with_context("worker_id", turn.worker()));
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                _ = stop.wait_for(|s| *s) => break,
            }
        }
    }
    tracing::info!(worker = turn.worker(), "forge.serve.merge_queue_stopped");
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

    #[test]
    fn the_start_line_says_whether_trains_are_linted() {
        // `merge_queue.train_lint_budget_ms = 0`: no lint is injected, and
        // the line says so whatever budget the limits still carry.
        assert_eq!(
            train_lint_fields(false, Duration::from_secs(10)),
            ("off", 0)
        );
        assert_eq!(
            train_lint_fields(true, Duration::from_secs(10)),
            ("on", 10_000)
        );
    }

    #[test]
    fn the_safety_nets_are_the_configured_ones_and_nothing_else_moves() {
        let base = QueueLimits {
            train_lint_budget: Duration::from_secs(3),
            ..QueueLimits::default()
        };
        let cfg = MergeQueueConfig {
            safety_poll_ms: 12_000,
            waiting_safety_poll_ms: 45_000,
            doc_batch_max: 16,
            ci_batch_max: 2,
            ..MergeQueueConfig::default()
        };
        let limits = limits_of(base, &cfg);
        assert_eq!(limits.idle, Duration::from_secs(12));
        assert_eq!(limits.poll, Duration::from_secs(45));
        // Plan 18 S7: the batch bounds are the keys'.
        assert_eq!((limits.doc_batch_max, limits.ci_batch_max), (16, 2));
        assert_eq!(
            QueueLimits {
                idle: base.idle,
                poll: base.poll,
                doc_batch_max: base.doc_batch_max,
                ci_batch_max: base.ci_batch_max,
                ..limits
            },
            base,
            "the lease, the test timeout and the lint budget stay"
        );
        // The defaults of the keys are the defaults of the limits.
        assert_eq!(limits_of(base, &MergeQueueConfig::default()), base);
        // The longest waiting period is still shorter than a lease is long
        // only by chance: nothing depends on it (a waiting queue holds no
        // lease), but a tick must end before its lease does.
        assert!(base.lease > TICK_TIMEOUT);
    }
}
