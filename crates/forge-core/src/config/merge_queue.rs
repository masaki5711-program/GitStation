//! `[merge_queue]`: the merge queue's worker in `forge serve` (plan 18 S1,
//! ADR-0037 (b)).
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
//! The other key of the gate, `merge_queue.doc_lint_strict`, is per
//! repository and therefore not here: it is read from the base's
//! `.forge/repo.yml` (`forge_core::repo_config`).
//!
//! A value outside its range is `FG-SYS-0002` at load time.

use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, unsigned};

const BUDGET_KEY: &str = "merge_queue.train_lint_budget_ms";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeQueueConfig {
    /// `merge_queue.train_lint_budget_ms` (`0`: the train lint is off).
    pub train_lint_budget_ms: u32,
}

impl Default for MergeQueueConfig {
    fn default() -> Self {
        Self {
            train_lint_budget_ms: Self::DEFAULT_TRAIN_LINT_BUDGET_MS,
        }
    }
}

impl MergeQueueConfig {
    pub const DEFAULT_TRAIN_LINT_BUDGET_MS: u32 = 10_000;
    pub const MIN_TRAIN_LINT_BUDGET_MS: u32 = 100;
    pub const MAX_TRAIN_LINT_BUDGET_MS: u32 = 60_000;

    /// The train lint's budget; `None`: the gate is off.
    pub fn train_lint_budget(&self) -> Option<Duration> {
        (self.train_lint_budget_ms > 0)
            .then(|| Duration::from_millis(u64::from(self.train_lint_budget_ms)))
    }

    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let budget = unsigned(&get, BUDGET_KEY, Self::DEFAULT_TRAIN_LINT_BUDGET_MS)?;
        if budget != 0 && budget < Self::MIN_TRAIN_LINT_BUDGET_MS {
            return Err(invalid(BUDGET_KEY, "below_min"));
        }
        if budget > Self::MAX_TRAIN_LINT_BUDGET_MS {
            return Err(invalid(BUDGET_KEY, "above_max"));
        }
        Ok(Self {
            train_lint_budget_ms: budget,
        })
    }
}
