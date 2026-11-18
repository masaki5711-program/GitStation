//! `[merge_queue]`: the merge queue's worker in `forge serve` (plan 18 S1
//! and S3, ADR-0037 (a) and (b)).
//!
//! `merge_queue.train_lint_budget_ms` (default 10000; `0`, or 100 ..=
//! 60000): the most the document lint of one train may take before it
//! lands. A train whose lint did not finish in time is not landed
//! (`FG-DOC-0002 scope=train reason=train_budget`; the batch is abandoned
//! and counted like a reap). `0` turns the train lint off: trains land as
//! they did before plan 18 S1, on the head's `forge/doc-lint` alone — the
//! switch back, logged once at start (`train_lint=off`). The upper bound
//! keeps the lint well inside one tick of the queue's worker.
//!
//! Plan 18 S3 — how the queue's turns wait. A turn is woken by the
//! `forge_merge_queue` notification the writers send (an entry enqueued,
//! dequeued or resumed; a train's pipeline finished); the notification is
//! a hint, so every period below is only the net under a lost one:
//!
//! * `merge_queue.safety_poll_ms` (default 30000; 100 ..= 300000): how
//!   long an idle turn waits before it looks by itself, and how far ahead
//!   a queue with nothing to do (paused, or every entry held) is looked at
//!   again. While the LISTEN connection of the process is down a turn
//!   looks every two seconds at most (the period before plan 18).
//! * `merge_queue.waiting_safety_poll_ms` (default 60000; 100 ..=
//!   600000): how far ahead a queue waiting for its train's pipeline is
//!   looked at again when no notification announces the result. `10000`
//!   is the poll period before plan 18 — the way back.
//! * `merge_queue.workers` (default 2; 1 ..= 8): the turn tasks of one
//!   process. Different queues progress at once; one queue is still
//!   driven by one turn at a time (its lease). `1` is the single loop
//!   before plan 18.
//!
//! The other key of the gate, `merge_queue.doc_lint_strict`, is per
//! repository and therefore not here: it is read from the base's
//! `.forge/repo.yml` (`forge_core::repo_config`).
//!
//! A value outside its range is `FG-SYS-0002` at load time.

use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, unsigned};

const BUDGET_KEY: &str = "merge_queue.train_lint_budget_ms";
const SAFETY_POLL_KEY: &str = "merge_queue.safety_poll_ms";
const WAITING_SAFETY_POLL_KEY: &str = "merge_queue.waiting_safety_poll_ms";
const WORKERS_KEY: &str = "merge_queue.workers";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeQueueConfig {
    /// `merge_queue.train_lint_budget_ms` (`0`: the train lint is off).
    pub train_lint_budget_ms: u32,
    /// `merge_queue.safety_poll_ms`.
    pub safety_poll_ms: u32,
    /// `merge_queue.waiting_safety_poll_ms`.
    pub waiting_safety_poll_ms: u32,
    /// `merge_queue.workers`.
    pub workers: u32,
}

impl Default for MergeQueueConfig {
    fn default() -> Self {
        Self {
            train_lint_budget_ms: Self::DEFAULT_TRAIN_LINT_BUDGET_MS,
            safety_poll_ms: Self::DEFAULT_SAFETY_POLL_MS,
            waiting_safety_poll_ms: Self::DEFAULT_WAITING_SAFETY_POLL_MS,
            workers: Self::DEFAULT_WORKERS,
        }
    }
}

impl MergeQueueConfig {
    pub const DEFAULT_TRAIN_LINT_BUDGET_MS: u32 = 10_000;
    pub const MIN_TRAIN_LINT_BUDGET_MS: u32 = 100;
    pub const MAX_TRAIN_LINT_BUDGET_MS: u32 = 60_000;
    pub const DEFAULT_SAFETY_POLL_MS: u32 = 30_000;
    pub const MIN_SAFETY_POLL_MS: u32 = 100;
    pub const MAX_SAFETY_POLL_MS: u32 = 300_000;
    pub const DEFAULT_WAITING_SAFETY_POLL_MS: u32 = 60_000;
    pub const MIN_WAITING_SAFETY_POLL_MS: u32 = 100;
    pub const MAX_WAITING_SAFETY_POLL_MS: u32 = 600_000;
    pub const DEFAULT_WORKERS: u32 = 2;
    pub const MIN_WORKERS: u32 = 1;
    pub const MAX_WORKERS: u32 = 8;

    /// The train lint's budget; `None`: the gate is off.
    pub fn train_lint_budget(&self) -> Option<Duration> {
        (self.train_lint_budget_ms > 0)
            .then(|| Duration::from_millis(u64::from(self.train_lint_budget_ms)))
    }

    /// The safety-net period of an idle turn (and of a queue with nothing
    /// to do).
    pub fn safety_poll(&self) -> Duration {
        Duration::from_millis(u64::from(self.safety_poll_ms))
    }

    /// The safety-net period of a queue waiting for its train's pipeline.
    pub fn waiting_safety_poll(&self) -> Duration {
        Duration::from_millis(u64::from(self.waiting_safety_poll_ms))
    }

    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let budget = unsigned(&get, BUDGET_KEY, Self::DEFAULT_TRAIN_LINT_BUDGET_MS)?;
        if budget != 0 && budget < Self::MIN_TRAIN_LINT_BUDGET_MS {
            return Err(invalid(BUDGET_KEY, "below_min"));
        }
        if budget > Self::MAX_TRAIN_LINT_BUDGET_MS {
            return Err(invalid(BUDGET_KEY, "above_max"));
        }
        let ranged = |key: &'static str, default: u32, min: u32, max: u32| {
            let value = unsigned(&get, key, default)?;
            if value < min {
                return Err(invalid(key, "below_min"));
            }
            if value > max {
                return Err(invalid(key, "above_max"));
            }
            Ok(value)
        };
        Ok(Self {
            train_lint_budget_ms: budget,
            safety_poll_ms: ranged(
                SAFETY_POLL_KEY,
                Self::DEFAULT_SAFETY_POLL_MS,
                Self::MIN_SAFETY_POLL_MS,
                Self::MAX_SAFETY_POLL_MS,
            )?,
            waiting_safety_poll_ms: ranged(
                WAITING_SAFETY_POLL_KEY,
                Self::DEFAULT_WAITING_SAFETY_POLL_MS,
                Self::MIN_WAITING_SAFETY_POLL_MS,
                Self::MAX_WAITING_SAFETY_POLL_MS,
            )?,
            workers: ranged(
                WORKERS_KEY,
                Self::DEFAULT_WORKERS,
                Self::MIN_WORKERS,
                Self::MAX_WORKERS,
            )?,
        })
    }
}
