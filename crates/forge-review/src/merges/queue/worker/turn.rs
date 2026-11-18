//! One turn task of the merge queue (plan 18 S3, ADR-0037 (a)).
//!
//! `forge serve` runs `merge_queue.workers` of these. Each claims one due
//! queue, does one step and releases it ([`MergeQueue::tick`]) until no
//! queue is due, then waits ([`QueueTurn::idle`]) for the first of:
//!
//! - **a wake** — `forge_merge_queue`, sent in the transaction of whoever
//!   made a queue actionable (an enqueue, a dequeue, a resume, a train
//!   pipeline that ended, a create job that left no pipeline), or the
//!   in-process nudge of the same change ([`MergeQueue::wake`]);
//! - **the next due time** — the deadline a waiting queue was released
//!   with (`merge_queue.waiting_safety_poll_ms`), or the end of a lease
//!   nobody released (a turn that died);
//! - **the safety net** — `merge_queue.safety_poll_ms`
//!   ([`QueueLimits::idle`](super::QueueLimits::idle)).
//!
//! A wake is a hint: the turn that wakes claims again and reads the
//! tables; nothing is decided from a payload. **One queue, one turn at a
//! time** is the claim's, not the wake's: a queue is taken with
//! `FOR UPDATE SKIP LOCKED` and a lease only its owner releases, so every
//! turn of every process may wake for the same queue and exactly one
//! steps it.
//!
//! **Listener down** (no hub, or its LISTEN connection is not up): the
//! wait is at most [`QueueTurn::FALLBACK_POLL`] — the poll of before plan 18.
//!
//! **`FG-DB-0024 WakeupMissed`** (the S2 rule): a wait that ran its whole
//! safety-net period with the listener up at both ends, followed by a
//! claim of a queue that had been claimable for at least that period (the
//! database's clock, `oldest_ready_ms`) and whose step did something. The
//! hub logs it at most once a minute.

use std::time::Duration;

use forge_core::RequestId;
use forge_db::review::merge_queue as db;
use forge_db::wake::{Channel, WakeRx, Woken};
use forge_errors::{ErrorCode, ForgeError};

use super::{millis, MergeQueue, TickReport};

/// The longest idle wait while no hub listens for this process.
const FALLBACK_POLL: Duration = Duration::from_secs(2);
/// Added to a due time, so the claim that follows finds the queue due.
const DUE_MARGIN: Duration = Duration::from_millis(20);
/// The wait when a queue is due now and the claim still found none (a
/// writer's open transaction holds its row; another turn took it): it
/// doubles up to [`DUE_WAIT_MAX`] until a claim succeeds.
const DUE_WAIT_MIN: Duration = Duration::from_millis(50);
const DUE_WAIT_MAX: Duration = Duration::from_secs(1);

/// Why an idle wait ended (the tests and the serve loop's log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Roused {
    /// A wake arrived (or the listener reconnected: look at everything).
    Wake,
    /// A queue is due by itself.
    Due,
    /// The safety-net period ended without a wake.
    SafetyNet,
    /// No live listener: the short poll.
    Poll,
}

/// One turn task (module docs). Not shared: each task has its own.
#[derive(Debug)]
pub struct QueueTurn {
    queue: MergeQueue,
    /// This turn's waiter on `forge_merge_queue`; `None` without a hub.
    rx: Option<WakeRx>,
    /// The last idle wait ran its whole safety-net period with the
    /// listener up and no wake came.
    after_safety_net: bool,
    /// Idle waits in a row that found a queue due at once.
    due_now: u32,
}

impl MergeQueue {
    /// Turn task `n` of this worker: the same queue worker under the lease
    /// owner `<worker>-<n>`, waiting on the wake hub of its database when
    /// one is attached (module docs).
    pub fn turn(&self, n: usize) -> QueueTurn {
        let mut queue = self.clone(); // clone: handles; the turns share the failure counts
        queue.worker = format!("{}-{n}", self.worker);
        let rx = queue
            .merger
            .db
            .wake()
            .map(|hub| hub.subscribe(Channel::MergeQueue));
        QueueTurn {
            queue,
            rx,
            after_safety_net: false,
            due_now: 0,
        }
    }

    /// The turn tasks `forge serve` runs for `workers`
    /// (`merge_queue.workers`; at least one).
    pub fn turns(&self, workers: usize) -> Vec<QueueTurn> {
        (0..workers.max(1)).map(|n| self.turn(n)).collect()
    }

    /// Wake the turns of THIS process for queue `queue_id`: a change that
    /// made it actionable is committed (see [`super::super::wake_queue`]).
    pub fn wake(&self, queue_id: i64) {
        super::super::wake_queue(&self.merger.db, queue_id);
    }
}

impl QueueTurn {
    /// The longest idle wait while no hub listens for this process (the
    /// poll of before plan 18).
    pub const FALLBACK_POLL: Duration = FALLBACK_POLL;

    /// The lease owner of this turn's claims.
    pub fn worker(&self) -> &str {
        &self.queue.worker
    }

    /// Whether this turn waits on a wake hub (false: it polls).
    pub fn waits_on_wake(&self) -> bool {
        self.rx.is_some()
    }

    fn live(&self) -> bool {
        self.rx.is_some()
            && self
                .queue
                .merger
                .db
                .wake()
                .is_some_and(|hub| hub.is_live(Channel::MergeQueue))
    }

    /// [`MergeQueue::tick`] for this turn. A step that found work after a
    /// whole safety-net wait no wake ended reports `FG-DB-0024` (module
    /// docs).
    pub async fn tick(&mut self) -> Result<Option<TickReport>, ForgeError> {
        let after_safety_net = std::mem::take(&mut self.after_safety_net);
        let Some((report, ready_for)) = self.queue.tick_timed().await? else {
            return Ok(None);
        };
        self.due_now = 0;
        if after_safety_net {
            self.found_by_the_safety_net(&report, ready_for);
        }
        Ok(Some(report))
    }

    fn found_by_the_safety_net(&self, report: &TickReport, ready_for: Duration) {
        let safety = self.queue.limits.idle;
        // Nothing to announce: the queue only wanted its periodic look.
        let nothing = matches!(report.outcome, "idle" | "paused" | "waiting" | "retry");
        if nothing || ready_for < safety || !self.live() {
            return;
        }
        let Some(hub) = self.queue.merger.db.wake() else {
            return;
        };
        let error = ForgeError::new(ErrorCode::WakeupMissed)
            .with_context("oldest_ready_ms", millis(ready_for))
            .with_context("safety_poll_ms", millis(safety))
            .with_context("queue_id", report.queue_id)
            .with_context("outcome", report.outcome)
            .with_context("worker_id", self.queue.worker.as_str());
        hub.missed(Channel::MergeQueue, error, &RequestId::generate());
    }

    /// Wait until a queue may be due (module docs); the caller ticks
    /// again. Cancel-safe: a dropped wait loses no wake. A failure to read
    /// the next due time is returned (the caller backs off as for a
    /// failed tick).
    pub async fn idle(&mut self) -> Result<Roused, ForgeError> {
        self.after_safety_net = false;
        let safety = self.queue.limits.idle;
        let live = self.live();
        let period = if live {
            safety
        } else {
            safety.min(FALLBACK_POLL)
        };
        let due = db::next_due(&self.queue.merger.db).await?;
        let (wait, whole) = match due {
            Some(due) if due.is_zero() => {
                // Due now, and the claim before this wait found none.
                let wait = DUE_WAIT_MIN
                    .saturating_mul(1 << self.due_now.min(8))
                    .min(DUE_WAIT_MAX);
                self.due_now = self.due_now.saturating_add(1);
                (wait.min(period), false)
            }
            Some(due) if due + DUE_MARGIN < period => (due + DUE_MARGIN, false),
            Some(_) | None => (period, true),
        };
        let Some(rx) = self.rx.as_mut() else {
            tokio::time::sleep(wait).await;
            return Ok(if whole { Roused::Poll } else { Roused::Due });
        };
        match rx.wait(wait).await {
            Woken::Notified(_) | Woken::Reconnected => Ok(Roused::Wake),
            Woken::SafetyNet if !whole => Ok(Roused::Due),
            Woken::SafetyNet if live && self.live() => {
                self.after_safety_net = true;
                Ok(Roused::SafetyNet)
            }
            Woken::SafetyNet => Ok(Roused::Poll),
        }
    }
}
