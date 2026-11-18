//! The document lint of a merge-queue train (plan 18 S1, ADR-0037 (b)).
//!
//! Two merge requests can each pass `forge/doc-lint` alone and break a
//! reference together (one deletes a document, the other starts to cite
//! it). The merge queue therefore lints the tree it is about to land — the
//! train's last step — before the target moves. forge-checks implements
//! [`TrainDocLint`] (the same reading and the same lint as the head's
//! `forge/doc-lint`); the merge queue in forge-review calls it — two
//! feature crates meet here, as for
//! [`crate::auto_merge::MergeResultAssessor`]. `forge serve` injects it.
//!
//! The verdict is "no new problem against the base" (the target's tip the
//! train was built on): a `main` that already has problems still moves
//! when a train adds none. A problem is the same one when its path, rule
//! and referenced id are the same (never its line). A repository whose
//! `.forge/repo.yml` on the base says `merge_queue.doc_lint_strict: true`
//! must be clean instead: nothing lands while its base has problems. A
//! member of the train never weakens a valid base value. A base whose
//! file is not valid (`FG-REPO-0024`) is decided by the train's last
//! step, so the train that repairs the file lands; while both are not
//! valid the repository is strict, and what stops is told as the config
//! ([`TrainLintVerdict::RepoConfigInvalid`]), never as the documents.
//!
//! A train that cannot be linted never lands: the implementation answers
//! an error (`FG-DOC-0002` with `scope=train` and `reason`), the queue
//! abandons the batch and counts it like a reap; the third time the entry
//! leaves with `FG-CI-0024` and [`UNAVAILABLE_REASON`] — the document
//! check could not be completed, no test was late.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use forge_errors::ForgeError;

use crate::repo_config::RepoConfigProblem;
use crate::{ObjectId, ProjectId, RepoId, RequestId};

/// The reason of a batch, and of the entry removed from it, whose train
/// brought a new document problem (`FG-CI-0032`).
pub const FAILED_REASON: &str = "doc_lint_failed";

/// The reason of the entries of a strict repository's batch that adds no
/// problem but whose base already has some (`FG-CI-0032`).
pub const BASE_NOT_CLEAN_REASON: &str = "doc_lint_base_not_clean";

/// The reason of the entries, and of the batch, of a train that adds no
/// problem while the base has some and `.forge/repo.yml` is not valid on
/// both the base and the last step (`FG-REPO-0024`): the config is the
/// cause, not a member's documents.
pub const REPO_CONFIG_INVALID_REASON: &str = "doc_lint_repo_config_invalid";

/// The reason of a batch abandoned because its train could not be linted
/// (`FG-DOC-0002 scope=train`), and of the entry that leaves the third
/// time (`FG-CI-0024`).
pub const UNAVAILABLE_REASON: &str = "doc_lint_unavailable";

/// `reason` of `FG-DOC-0002 scope=train`: the lint ran out of its budget.
pub const REASON_BUDGET: &str = "train_budget";

/// `reason` of `FG-DOC-0002 scope=train`: a tree, the repository's
/// setting or the check run could not be read or written, or a bound of
/// the reading was hit (the tree was read only in part).
pub const REASON_READ_FAILED: &str = "read_failed";

/// The most problems a verdict lists (the removal's log line).
pub const MAX_LISTED_PROBLEMS: usize = 20;

/// The most bytes of one listed problem.
const MAX_LISTED_BYTES: usize = 200;

/// What the train lint reads.
#[derive(Debug, Clone, Copy)]
pub struct TrainLintInput<'a> {
    pub project: ProjectId,
    pub repo: RepoId,
    /// The target's tip the train was built on.
    pub base: &'a ObjectId,
    /// The train's commits in order: step `i` is every entry up to and
    /// including the `i`-th, merged on `base`. The last one lands.
    pub steps: &'a [ObjectId],
    /// The most the whole lint may take.
    pub budget: Duration,
    /// The batch's request id (the check run's record, the logs).
    pub request_id: &'a RequestId,
}

/// One problem of a verdict: what makes it the same problem on two trees.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrainLintProblem {
    pub path: String,
    pub rule: String,
    /// The referenced id (a broken reference's target), when the rule has
    /// one.
    pub reference: Option<String>,
}

impl TrainLintProblem {
    /// `rule:path:id` for a log line: one word, no control character, at
    /// most [`MAX_LISTED_BYTES`] bytes (the path and the id are the
    /// repository's content).
    pub fn listed(&self) -> String {
        let whole = match &self.reference {
            Some(id) => format!("{}:{}:{id}", self.rule, self.path),
            None => format!("{}:{}:", self.rule, self.path),
        };
        let mut out = String::with_capacity(whole.len().min(MAX_LISTED_BYTES));
        for c in whole.chars() {
            let c = if c.is_control() || c.is_whitespace() || c == ',' {
                '_'
            } else {
                c
            };
            if out.len().saturating_add(c.len_utf8()) > MAX_LISTED_BYTES {
                break;
            }
            out.push(c);
        }
        out
    }
}

/// The first [`MAX_LISTED_PROBLEMS`] of `problems` as one log field
/// (`rule:path:id,…`).
pub fn listed(problems: &[TrainLintProblem]) -> String {
    problems
        .iter()
        .take(MAX_LISTED_PROBLEMS)
        .map(TrainLintProblem::listed)
        .collect::<Vec<_>>()
        .join(",")
}

/// What the train lint found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrainLintVerdict {
    /// No member changes a document or a type definition (and the
    /// repository is not strict): the train's lint is the base's. Nothing
    /// was read or recorded.
    NotApplicable,
    /// The last step adds no problem (strict: has none). The train lands.
    Clean,
    /// Step `first_bad_step` (an index of
    /// [`TrainLintInput::steps`]) is the first whose tree has a problem
    /// the base does not have while the step before it has none: its
    /// entry leaves the queue, the rest are rebuilt in order. `problems`
    /// are the new ones at that step (at most [`MAX_LISTED_PROBLEMS`]).
    NewProblems {
        first_bad_step: usize,
        problems: Vec<TrainLintProblem>,
    },
    /// A strict repository: no step adds a problem, but the base's own
    /// are still there at the last step. No member is the cause; nothing
    /// lands until the base is clean. `problems`: the ones left (at most
    /// [`MAX_LISTED_PROBLEMS`]).
    BaseNotClean { problems: Vec<TrainLintProblem> },
    /// As [`Self::BaseNotClean`], but the repository is strict only
    /// because `.forge/repo.yml` is not valid on the base and on the last
    /// step (`config`: the base's first broken rule and its line). The
    /// cause is the config: a train that repairs the file (or leaves no
    /// problem) lands.
    RepoConfigInvalid {
        problems: Vec<TrainLintProblem>,
        config: RepoConfigProblem,
    },
}

/// The answer of a [`TrainDocLint`].
pub type TrainLintFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TrainLintVerdict, ForgeError>> + Send + 'a>>;

/// The document lint of a train (module docs). The implementation bounds
/// its own work by [`TrainLintInput::budget`] and records the last step's
/// `forge/doc-lint` run on that commit whenever it linted it. An error is
/// "could not be linted" — never a pass: `FG-DOC-0002` with `scope=train`,
/// `reason` ([`REASON_BUDGET`], [`REASON_READ_FAILED`]) and the cause's
/// code.
pub trait TrainDocLint: Send + Sync {
    fn lint_train<'a>(&'a self, input: TrainLintInput<'a>) -> TrainLintFuture<'a>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(path: &str, rule: &str, reference: Option<&str>) -> TrainLintProblem {
        TrainLintProblem {
            path: path.to_owned(),
            rule: rule.to_owned(),
            reference: reference.map(str::to_owned),
        }
    }

    #[test]
    fn a_listed_problem_is_rule_path_id_in_one_safe_word() {
        assert_eq!(
            problem("qms/QM-001.md", "ref_broken", Some("QM-002")).listed(),
            "ref_broken:qms/QM-001.md:QM-002"
        );
        assert_eq!(
            problem("a b.md", "no_title", None).listed(),
            "no_title:a_b.md:"
        );
        // The repository's content never breaks the log line.
        let hostile = problem("x\n.md", "ref_broken", Some("A,B\u{7}"));
        assert_eq!(hostile.listed(), "ref_broken:x_.md:A_B_");
        let long = problem(&"é".repeat(400), "ref_broken", None).listed();
        assert!(long.len() <= MAX_LISTED_BYTES);
        assert!(long.is_char_boundary(long.len()));
    }

    #[test]
    fn only_the_first_twenty_problems_are_listed() {
        let many: Vec<TrainLintProblem> = (0..30)
            .map(|i| problem(&format!("d/{i}.md"), "ref_broken", Some("X")))
            .collect();
        let line = listed(&many);
        assert_eq!(line.split(',').count(), MAX_LISTED_PROBLEMS);
        assert!(line.starts_with("ref_broken:d/0.md:X,"));
        assert_eq!(listed(&[]), "");
    }
}
