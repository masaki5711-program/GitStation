//! The document lint of a train before it lands (plan 18 S1, ADR-0037
//! (b)): the queue's side of `forge_core::train_lint`.
//!
//! Two merge requests can each pass `forge/doc-lint` alone and break a
//! reference together. Once a train's pipeline is green (or it has none)
//! and before anything else decides its landing, the worker asks the
//! injected [`TrainDocLint`] about the train's steps:
//!
//! - **not applicable / clean** → the train goes on to land; the time the
//!   lint took is the landing's `lint_ms` (not applicable: the time to
//!   find that out, and one debug line,
//!   `forge.merge_queue.train_lint_skipped`).
//! - **new problems** → the entry of the first bad step is `removed`
//!   (`FG-CI-0032 reason=doc_lint_failed`), the batch is `failed`, the
//!   other entries go back to the queue and are rebuilt in order.
//! - **the base is not clean** (a strict repository) → every entry is
//!   `removed` (`FG-CI-0032 reason=doc_lint_base_not_clean`): none of them
//!   is the cause and none may land.
//! - **the base is not clean and `.forge/repo.yml` is not valid** → every
//!   entry is `removed` with the config as the cause (`FG-REPO-0024
//!   reason=doc_lint_repo_config_invalid`), not their documents.
//! - **could not be linted** (an error, or no answer within the budget
//!   and [`GRACE`]) → never a pass: logged once
//!   (`forge.merge_queue.train_lint_unavailable`, `FG-DOC-0002
//!   scope=train`), the batch is `abandoned`
//!   (`reason=doc_lint_unavailable`) and counted like a reaped one — the
//!   third time an entry is `removed` with `FG-CI-0024
//!   reason=doc_lint_unavailable` (not `stalled`: its test did finish),
//!   and that line carries what the lint said.
//!
//! The two `FG-CI-0032` removals are expected outcomes of a member's
//! change and log at info; the other two are not and log at warn.
//!
//! No lint injected (`merge_queue.train_lint_budget_ms = 0`): the gate is
//! off, trains land as before it existed, and nothing is logged per train.

use std::time::{Duration, Instant};

use forge_core::merge_queue::{BatchState, MAX_REAPS};
use forge_core::train_lint::{
    self, TrainDocLint, TrainLintInput, TrainLintVerdict, BASE_NOT_CLEAN_REASON, FAILED_REASON,
    REASON_BUDGET, REASON_READ_FAILED, REPO_CONFIG_INVALID_REASON, UNAVAILABLE_REASON,
};
use forge_core::{ObjectId, RepoId};
use forge_db::review::merge_queue::{self as db, BatchEnd, StoredBatch, StoredEntry};
use forge_errors::{ErrorCode, ForgeError};

use super::MergeQueue;

mod lines;

use lines::{Asked, Red, Unavailable};

/// What the worker waits past the lint's own budget before it stops
/// waiting itself (the implementation bounds its work; this bounds the
/// implementation).
const GRACE: Duration = Duration::from_secs(2);

/// What the gate decided for a green train.
pub(super) enum TrainLintGate {
    /// The train may go on to land; the lint took this long (zero: none
    /// ran).
    Land(Duration),
    /// The batch ended here; the tick's outcome.
    Ended(&'static str),
}

/// `FG-DOC-0002 scope=train` for a failure of the lint: its own when it
/// already is one, else `reason=read_failed` with the cause's code.
fn unavailable(cause: ForgeError) -> ForgeError {
    if cause.code() == ErrorCode::DocIndexBounded {
        return cause;
    }
    ForgeError::new(ErrorCode::DocIndexBounded)
        .with_context("scope", "train")
        .with_context("reason", REASON_READ_FAILED)
        .with_context("cause", cause.code().id())
        .with_source(cause)
}

/// The value of `key` in `error`'s context.
fn context_of<'a>(error: &'a ForgeError, key: &str) -> Option<&'a str> {
    error
        .context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

impl MergeQueue {
    /// The gate of the module docs for `batch`, whose `testing` entries
    /// are its train's steps in order.
    pub(super) async fn train_lint_gate(
        &self,
        batch: &StoredBatch,
        testing: &[StoredEntry],
        repo: RepoId,
    ) -> Result<TrainLintGate, ForgeError> {
        let Some(lint) = self.train_lint.as_ref() else {
            return Ok(TrainLintGate::Land(Duration::ZERO));
        };
        let mut steps: Vec<ObjectId> = Vec::with_capacity(testing.len());
        for entry in testing {
            let Some(step) = entry.train_sha.as_ref() else {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "merge_queue_entry_without_train")
                    .with_context("entry_id", entry.id));
            };
            steps.push(step.clone()); // clone: the lint's input borrows the list
        }
        let started = Instant::now();
        let verdict = self.ask(lint.as_ref(), batch, &steps, repo).await;
        let asked = Asked {
            batch,
            repo,
            took: started.elapsed(),
        };
        // Every entry, none of them the cause (the last two verdicts).
        let all: Vec<(&StoredEntry, Option<usize>)> = testing.iter().map(|e| (e, None)).collect();
        let outcome = match verdict {
            Ok(TrainLintVerdict::NotApplicable) => {
                asked.skipped();
                return Ok(TrainLintGate::Land(asked.took));
            }
            Ok(TrainLintVerdict::Clean) => return Ok(TrainLintGate::Land(asked.took)),
            Ok(TrainLintVerdict::NewProblems {
                first_bad_step,
                problems,
            }) => {
                let Some(entry) = testing.get(first_bad_step) else {
                    let cause = ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "train_lint_step_out_of_range")
                        .with_context("step", first_bad_step)
                        .with_context("steps", testing.len());
                    return self
                        .train_unlintable(&asked, testing, unavailable(cause))
                        .await
                        .map(TrainLintGate::Ended);
                };
                let step = first_bad_step.saturating_add(1);
                let red = Red {
                    reason: FAILED_REASON,
                    problems: &problems,
                    config: None,
                };
                self.train_lint_red(&asked, &[(entry, Some(step))], &red)
                    .await?
            }
            Ok(TrainLintVerdict::BaseNotClean { problems }) => {
                let red = Red {
                    reason: BASE_NOT_CLEAN_REASON,
                    problems: &problems,
                    config: None,
                };
                self.train_lint_red(&asked, &all, &red).await?
            }
            Ok(TrainLintVerdict::RepoConfigInvalid { problems, config }) => {
                let red = Red {
                    reason: REPO_CONFIG_INVALID_REASON,
                    problems: &problems,
                    config: Some(config),
                };
                self.train_lint_red(&asked, &all, &red).await?
            }
            Err(e) => {
                self.train_unlintable(&asked, testing, unavailable(e))
                    .await?
            }
        };
        Ok(TrainLintGate::Ended(outcome))
    }

    /// The lint's answer within its budget and [`GRACE`].
    async fn ask(
        &self,
        lint: &dyn TrainDocLint,
        batch: &StoredBatch,
        steps: &[ObjectId],
        repo: RepoId,
    ) -> Result<TrainLintVerdict, ForgeError> {
        let budget = self.limits.train_lint_budget;
        let input = TrainLintInput {
            project: batch.project_id,
            repo,
            base: &batch.base_sha,
            steps,
            budget,
            request_id: &batch.request_id,
        };
        let limit = budget.saturating_add(GRACE);
        match tokio::time::timeout(limit, lint.lint_train(input)).await {
            Ok(verdict) => verdict,
            Err(_) => Err(ForgeError::new(ErrorCode::DocIndexBounded)
                .with_context("scope", "train")
                .with_context("reason", REASON_BUDGET)
                .with_context("budget_ms", budget.as_millis())),
        }
    }

    /// `leaving` (each with its 1-based step when it is the cause) leave
    /// the queue as `red` says; the batch is `failed` and its other
    /// entries go back in order.
    async fn train_lint_red(
        &self,
        asked: &Asked<'_>,
        leaving: &[(&StoredEntry, Option<usize>)],
        red: &Red<'_>,
    ) -> Result<&'static str, ForgeError> {
        let code = red.code();
        let end = BatchEnd {
            from: BatchState::Testing,
            state: BatchState::Failed,
            reason: red.batch_reason(),
            error_code: Some(code),
            removed: leaving
                .iter()
                .map(|(e, _)| (e.id, red.reason.to_owned(), code))
                .collect(),
            reaped: false,
            batch_limit: None,
        };
        if !db::end_batch(&self.merger.db, asked.batch.id, &end).await? {
            asked.raced(red.reason);
            return Ok("raced");
        }
        self.retire_train(asked.repo, asked.batch).await;
        let listed = train_lint::listed(red.problems);
        for (entry, step) in leaving {
            red.log(asked, entry, *step, &listed);
        }
        Ok("removed")
    }

    /// The train could not be linted (`error`: `FG-DOC-0002 scope=train`):
    /// logged once, the batch abandoned and counted like a reaped one. An
    /// entry this happens to for the third time leaves, and is told that
    /// the document check could not be completed — `FG-CI-0024` with
    /// `UNAVAILABLE_REASON`, not `stalled` (its test did finish in time).
    async fn train_unlintable(
        &self,
        asked: &Asked<'_>,
        testing: &[StoredEntry],
        error: ForgeError,
    ) -> Result<&'static str, ForgeError> {
        let batch = asked.batch;
        let leaving: Vec<&StoredEntry> = testing
            .iter()
            .filter(|e| e.reaps + 1 >= MAX_REAPS)
            .collect();
        let end = BatchEnd {
            from: BatchState::Testing,
            state: BatchState::Abandoned,
            reason: UNAVAILABLE_REASON,
            error_code: Some(ErrorCode::DocIndexBounded),
            // Named here, they are removed before the reap's own removal
            // (`stalled`) looks for them; the others get one more reap.
            removed: leaving
                .iter()
                .map(|e| {
                    (
                        e.id,
                        UNAVAILABLE_REASON.to_owned(),
                        ErrorCode::MergeQueueStalled,
                    )
                })
                .collect(),
            reaped: true,
            batch_limit: None,
        };
        // What the lint said: on the batch's line and on each removal's.
        let lint_reason = context_of(&error, "reason");
        let cause = context_of(&error, "cause");
        let bound = context_of(&error, "bound");
        // Written by `ask` as a number; anything else is not logged.
        let budget_ms = context_of(&error, "budget_ms").and_then(|ms| ms.parse::<u64>().ok());
        if !db::end_batch(&self.merger.db, batch.id, &end).await? {
            asked.raced(lint_reason.unwrap_or(UNAVAILABLE_REASON));
            return Ok("raced");
        }
        self.retire_train(asked.repo, batch).await;
        let said = Unavailable {
            error: &error,
            lint_reason,
            cause,
            bound,
            budget_ms,
        };
        said.log_batch(asked);
        for entry in leaving {
            said.log_removed(asked, entry);
        }
        Ok("reaped")
    }
}
