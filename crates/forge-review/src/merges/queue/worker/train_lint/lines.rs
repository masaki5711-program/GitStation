//! The log lines of the train lint's gate (the module above decides; this
//! one says it): every line carries the batch's base and train commits and
//! what the lint's answer took, and a field that has nothing to say is
//! left out rather than logged empty.

use std::time::Duration;

use forge_core::repo_config::RepoConfigProblem;
use forge_core::train_lint::{
    TrainLintProblem, FAILED_REASON, REPO_CONFIG_INVALID_REASON, UNAVAILABLE_REASON,
};
use forge_core::RepoId;
use forge_db::review::merge_queue::{StoredBatch, StoredEntry};
use forge_errors::{ErrorCode, ForgeError};

use super::super::millis;

/// A train the lint was asked about: what every line of the gate says.
#[derive(Clone, Copy)]
pub(super) struct Asked<'a> {
    pub(super) batch: &'a StoredBatch,
    pub(super) repo: RepoId,
    /// What the answer took.
    pub(super) took: Duration,
}

impl Asked<'_> {
    /// Another worker ended the batch first: `verdict` is dropped, and
    /// this line is all that is left of it.
    pub(super) fn raced(&self, verdict: &str) {
        tracing::debug!(
            target: "forge_review",
            event = "forge.merge_queue.train_lint_raced",
            request_id = %self.batch.request_id,
            queue_id = self.batch.queue_id,
            batch_id = self.batch.id,
            verdict,
            base_sha = self.batch.base_sha.as_str(),
            train_sha = self.batch.train_sha.as_str(),
            elapsed_ms = millis(self.took),
            "the batch was ended by another worker: the train lint's verdict is dropped"
        );
    }

    /// The lint does not apply (no document changed, not strict): told
    /// apart from "the gate is off", which logs nothing.
    pub(super) fn skipped(&self) {
        tracing::debug!(
            target: "forge_review",
            event = "forge.merge_queue.train_lint_skipped",
            request_id = %self.batch.request_id,
            queue_id = self.batch.queue_id,
            batch_id = self.batch.id,
            base_sha = self.batch.base_sha.as_str(),
            train_sha = self.batch.train_sha.as_str(),
            elapsed_ms = millis(self.took),
            "the train changes no document: not linted"
        );
    }
}

/// Why entries leave on the lint's verdict.
pub(super) struct Red<'a> {
    /// The entries' reason.
    pub(super) reason: &'static str,
    pub(super) problems: &'a [TrainLintProblem],
    /// `.forge/repo.yml` is the cause (`FG-REPO-0024`), with what is
    /// wrong in it; `None`: the documents are (`FG-CI-0032`).
    pub(super) config: Option<RepoConfigProblem>,
}

impl Red<'_> {
    pub(super) fn code(&self) -> ErrorCode {
        match self.config {
            Some(_) => ErrorCode::RepoConfigInvalid,
            None => ErrorCode::MergeQueueDocLintFailed,
        }
    }

    /// The batch's reason: the config when it is the cause, else the
    /// lint's one word for both of its verdicts.
    pub(super) fn batch_reason(&self) -> &'static str {
        match self.config {
            Some(_) => REPO_CONFIG_INVALID_REASON,
            None => FAILED_REASON,
        }
    }

    /// One entry's removal (`step`: 1-based, when it is the cause).
    pub(super) fn log(
        &self,
        asked: &Asked<'_>,
        entry: &StoredEntry,
        step: Option<usize>,
        listed: &str,
    ) {
        let batch = asked.batch;
        match self.config {
            // An expected outcome of a member's change: info.
            None => tracing::info!(
                target: "forge_review",
                event = "forge.merge_queue.entry_removed",
                error_code = self.code().id(),
                request_id = %batch.request_id,
                queue_id = batch.queue_id,
                batch_id = batch.id,
                entry_id = entry.id,
                merge_request_id = entry.merge_request.get(),
                reason = self.reason,
                base_sha = batch.base_sha.as_str(),
                train_sha = batch.train_sha.as_str(),
                step,
                problems = %listed,
                elapsed_ms = millis(asked.took),
                "a merge request left the merge queue: the train's document lint"
            ),
            // Nobody's change caused it and nothing lands until the file
            // is repaired: warn.
            Some(config) => tracing::warn!(
                target: "forge_review",
                event = "forge.merge_queue.entry_removed",
                error_code = self.code().id(),
                request_id = %batch.request_id,
                queue_id = batch.queue_id,
                batch_id = batch.id,
                entry_id = entry.id,
                merge_request_id = entry.merge_request.get(),
                reason = self.reason,
                config_line = config.line,
                config_reason = config.reason,
                base_sha = batch.base_sha.as_str(),
                train_sha = batch.train_sha.as_str(),
                problems = %listed,
                elapsed_ms = millis(asked.took),
                "a merge request left the merge queue: the repository's .forge/repo.yml is \
                 not valid, so the target's old document problems stop every train"
            ),
        }
    }
}

/// What the lint said when it could not be completed (`FG-DOC-0002
/// scope=train`): on the batch's line and on each third-time removal's.
pub(super) struct Unavailable<'a> {
    pub(super) error: &'a ForgeError,
    /// `train_budget` or `read_failed`.
    pub(super) lint_reason: Option<&'a str>,
    /// The code of what failed, when something did.
    pub(super) cause: Option<&'a str>,
    /// The bound of the train's reading that was hit, when one was.
    pub(super) bound: Option<&'a str>,
    /// The budget that ran out, when it did.
    pub(super) budget_ms: Option<u64>,
}

impl Unavailable<'_> {
    /// Once per batch.
    pub(super) fn log_batch(&self, asked: &Asked<'_>) {
        let batch = asked.batch;
        let source = std::error::Error::source(self.error).map(ToString::to_string);
        tracing::warn!(
            target: "forge_review",
            event = "forge.merge_queue.train_lint_unavailable",
            error_code = self.error.code().id(),
            request_id = %batch.request_id,
            queue_id = batch.queue_id,
            batch_id = batch.id,
            base_sha = batch.base_sha.as_str(),
            train_sha = batch.train_sha.as_str(),
            scope = "train",
            reason = self.lint_reason,
            cause = self.cause,
            bound = self.bound,
            budget_ms = self.budget_ms,
            source = source.as_deref(),
            elapsed_ms = millis(asked.took),
            "the train's document lint could not be completed: the batch does not land"
        );
    }

    /// The third time for `entry`: it leaves, and the line says why the
    /// document check could not be completed.
    pub(super) fn log_removed(&self, asked: &Asked<'_>, entry: &StoredEntry) {
        let batch = asked.batch;
        tracing::warn!(
            target: "forge_review",
            event = "forge.merge_queue.entry_removed",
            error_code = ErrorCode::MergeQueueStalled.id(),
            request_id = %batch.request_id,
            queue_id = batch.queue_id,
            batch_id = batch.id,
            entry_id = entry.id,
            merge_request_id = entry.merge_request.get(),
            reason = UNAVAILABLE_REASON,
            lint_reason = self.lint_reason,
            cause = self.cause,
            bound = self.bound,
            budget_ms = self.budget_ms,
            base_sha = batch.base_sha.as_str(),
            train_sha = batch.train_sha.as_str(),
            elapsed_ms = millis(asked.took),
            "a merge request left the merge queue: the document lint of its train could \
             not be completed three times"
        );
    }
}
