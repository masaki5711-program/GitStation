//! The document lint of a train before it lands (plan 18 S1, ADR-0037
//! (b)): the queue's side of `forge_core::train_lint`.
//!
//! Two merge requests can each pass `forge/doc-lint` alone and break a
//! reference together. Once a train's pipeline is green (or it has none)
//! and before anything else decides its landing, the worker asks the
//! injected [`TrainDocLint`] about the train's steps:
//!
//! - **not applicable / clean** → the train goes on to land; the time the
//!   lint took is the landing's `lint_ms`.
//! - **new problems** → the entry of the first bad step is `removed`
//!   (`FG-CI-0032 reason=doc_lint_failed`), the batch is `failed`, the
//!   other entries go back to the queue and are rebuilt in order.
//! - **the base is not clean** (a strict repository) → every entry is
//!   `removed` (`FG-CI-0032 reason=doc_lint_base_not_clean`): none of them
//!   is the cause and none may land.
//! - **could not be linted** (an error, or no answer within the budget
//!   and [`GRACE`]) → never a pass: logged once
//!   (`forge.merge_queue.train_lint_unavailable`, `FG-DOC-0002
//!   scope=train`), the batch is `abandoned`
//!   (`reason=doc_lint_unavailable`) and counted like a reaped one — the
//!   third time an entry is `removed` with `FG-CI-0024`.
//!
//! No lint injected (`merge_queue.train_lint_budget_ms = 0`): the gate is
//! off and trains land as before it existed.

use std::time::{Duration, Instant};

use forge_core::merge_queue::{BatchState, MAX_REAPS};
use forge_core::train_lint::{
    self, TrainDocLint, TrainLintInput, TrainLintProblem, TrainLintVerdict, BASE_NOT_CLEAN_REASON,
    FAILED_REASON, REASON_BUDGET, REASON_READ_FAILED, UNAVAILABLE_REASON,
};
use forge_core::{ObjectId, RepoId};
use forge_db::review::merge_queue::{self as db, BatchEnd, StoredBatch, StoredEntry};
use forge_errors::{ErrorCode, ForgeError};

use super::{log_removed, MergeQueue};

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

/// The value of `key` in `error`'s context (`""`: absent).
fn context_of<'a>(error: &'a ForgeError, key: &str) -> &'a str {
    error
        .context()
        .iter()
        .find(|(k, _)| *k == key)
        .map_or("", |(_, v)| v.as_str())
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
        let took = started.elapsed();
        let outcome = match verdict {
            Ok(TrainLintVerdict::NotApplicable | TrainLintVerdict::Clean) => {
                return Ok(TrainLintGate::Land(took));
            }
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
                        .train_unlintable(batch, testing, repo, unavailable(cause))
                        .await
                        .map(TrainLintGate::Ended);
                };
                let step = first_bad_step.saturating_add(1);
                self.train_lint_red(
                    batch,
                    &[(entry, Some(step))],
                    FAILED_REASON,
                    &problems,
                    repo,
                )
                .await?
            }
            Ok(TrainLintVerdict::BaseNotClean { problems }) => {
                let all: Vec<(&StoredEntry, Option<usize>)> =
                    testing.iter().map(|e| (e, None)).collect();
                self.train_lint_red(batch, &all, BASE_NOT_CLEAN_REASON, &problems, repo)
                    .await?
            }
            Err(e) => {
                self.train_unlintable(batch, testing, repo, unavailable(e))
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
    /// the queue with `reason` (`FG-CI-0032`); the batch is `failed` and
    /// its other entries go back in order.
    async fn train_lint_red(
        &self,
        batch: &StoredBatch,
        leaving: &[(&StoredEntry, Option<usize>)],
        reason: &'static str,
        problems: &[TrainLintProblem],
        repo: RepoId,
    ) -> Result<&'static str, ForgeError> {
        let code = ErrorCode::MergeQueueDocLintFailed;
        let end = BatchEnd {
            from: BatchState::Testing,
            state: BatchState::Failed,
            reason: FAILED_REASON,
            error_code: Some(code),
            removed: leaving
                .iter()
                .map(|(e, _)| (e.id, reason.to_owned(), code))
                .collect(),
            reaped: false,
            batch_limit: None,
        };
        if !db::end_batch(&self.merger.db, batch.id, &end).await? {
            return Ok("raced");
        }
        self.retire_train(repo, batch).await;
        let listed = train_lint::listed(problems);
        for (entry, step) in leaving {
            tracing::warn!(
                target: "forge_review",
                event = "forge.merge_queue.entry_removed",
                error_code = code.id(),
                request_id = %batch.request_id,
                queue_id = batch.queue_id,
                batch_id = batch.id,
                entry_id = entry.id,
                merge_request_id = entry.merge_request.get(),
                reason,
                train_sha = batch.train_sha.as_str(),
                step = *step,
                problems = %listed,
                "a merge request left the merge queue: the train's document lint"
            );
        }
        Ok("removed")
    }

    /// The train could not be linted (`error`: `FG-DOC-0002 scope=train`):
    /// logged once, the batch abandoned and counted like a reaped one.
    async fn train_unlintable(
        &self,
        batch: &StoredBatch,
        testing: &[StoredEntry],
        repo: RepoId,
        error: ForgeError,
    ) -> Result<&'static str, ForgeError> {
        let end = BatchEnd {
            from: BatchState::Testing,
            state: BatchState::Abandoned,
            reason: UNAVAILABLE_REASON,
            error_code: Some(ErrorCode::DocIndexBounded),
            removed: Vec::new(),
            reaped: true,
            batch_limit: None,
        };
        if !db::end_batch(&self.merger.db, batch.id, &end).await? {
            return Ok("raced");
        }
        self.retire_train(repo, batch).await;
        let source = std::error::Error::source(&error)
            .map(ToString::to_string)
            .unwrap_or_default();
        tracing::warn!(
            target: "forge_review",
            event = "forge.merge_queue.train_lint_unavailable",
            error_code = error.code().id(),
            request_id = %batch.request_id,
            queue_id = batch.queue_id,
            batch_id = batch.id,
            train_sha = batch.train_sha.as_str(),
            scope = "train",
            reason = context_of(&error, "reason"),
            cause = context_of(&error, "cause"),
            bound = context_of(&error, "bound"),
            budget_ms = context_of(&error, "budget_ms"),
            source = %source,
            "the train's document lint could not be completed: the batch does not land"
        );
        for entry in testing.iter().filter(|e| e.reaps + 1 >= MAX_REAPS) {
            log_removed(
                entry,
                None,
                "stalled",
                ErrorCode::MergeQueueStalled,
                &batch.request_id,
            );
        }
        Ok("reaped")
    }
}
