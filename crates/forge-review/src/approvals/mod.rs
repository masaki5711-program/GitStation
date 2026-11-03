//! Approvals, approval rules and CODEOWNERS (M3 R5, ADR-0024 §8, §9).
//!
//! - [`codeowners`]: the CODEOWNERS grammar and matcher (pure).
//! - [`evaluate`]: how each approval counts and what blocks the merge
//!   (pure; `evaluate_merge()` calls it on [`MergeFacts::approvals`]).
//! - [`committers`]: who pushed the range's commits (pure, on the push
//!   records and git's answers).
//! - `load.rs`: [`ReviewGit`], the git reads the evaluation needs — the
//!   paths the merge changes on the target (the target tip against the
//!   trial merge's tree: right with criss-cross histories too), the range
//!   (`head ^<target tip>`), the target tip's CODEOWNERS (**never** the
//!   merge request's own: ADR-0024 §9) — all under one [`Budget`]
//!   (deadline and call count, R5 review).
//! - `commands.rs`: [`Review::approve`], [`Review::unapprove`],
//!   [`Review::approval_state`] and the rule commands.
//!
//! A git read that fails, times out or runs out of budget never means
//! "nothing changed" / "no owners" / "no committers": the fact is unknown
//! and the merge blocked (`changed_paths_unknown`, `codeowners_invalid
//! reason=read_failed|match_budget`, `committers_unknown`), with one warn
//! line carrying the error's code.
//!
//! [`MergeFacts::approvals`]: crate::merges::MergeFacts::approvals
//! [`Review::approve`]: crate::Review::approve
//! [`Review::unapprove`]: crate::Review::unapprove
//! [`Review::approval_state`]: crate::Review::approval_state

use std::time::{Duration, Instant};

use forge_core::{ObjectId, ProjectId, RepoId, RequestId};
use forge_db::review::StoredMergeRequest;
use forge_errors::ForgeError;
use forge_git::{Git, StorageRoot};

pub mod carry;
pub mod codeowners;
mod commands;
pub mod committers;
pub mod evaluate;
mod load;

pub use codeowners::{Codeowners, CodeownersError, Owner};
pub use commands::{ApprovalAnswer, ApprovalState, RuleAsk};
pub use evaluate::{
    evaluate, is_config_path, ApprovalInputs, ApprovalReport, ApprovalStatus, ChangedPathsFact,
    CodeownerStatus, CodeownersFact, Committers, OwnerRef, RuleStatus, PROTECTED_BRANCH_RULE,
};

/// The most changed paths evaluated (decided in R5; ADR-0024 未決): more is
/// `changed_paths_unknown reason=too_many_paths`.
pub const MAX_CHANGED_PATHS: usize = 10_000;

/// The bound of each git read of an evaluation (decided in R5), cut to
/// what is left of the evaluation's [`Budget`].
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The budget of one evaluation's reads on a request (merge, approve):
/// 20 s and 64 git calls (R5 review).
pub const REQUEST_BUDGET: (Duration, usize) = (Duration::from_secs(20), 64);
/// The budget of `GET …/approvals` (a read anyone with read access may
/// repeat): 10 s and 32 git calls.
pub const STATE_BUDGET: (Duration, usize) = (Duration::from_secs(10), 32);
/// The most git calls of one evaluation in the merge job (its deadline is
/// what is left of the merge lock's hold).
pub const JOB_CALLS: usize = 64;
/// The time CODEOWNERS matching may take (in a blocking task).
pub const MATCH_BUDGET: Duration = Duration::from_secs(5);

/// One deadline and a number of git calls for every read of one
/// evaluation (R5 review: ~21 sequential calls must not each take their
/// own full timeout).
#[derive(Debug, Clone)]
pub struct Budget {
    deadline: Instant,
    calls: usize,
}

impl Budget {
    /// `total` from now, at most `calls` git calls.
    pub fn new((total, calls): (Duration, usize)) -> Self {
        Self {
            deadline: Instant::now() + total,
            calls,
        }
    }

    /// Until `deadline`, at most `calls` git calls (the merge job: the
    /// lock's hold).
    pub fn until(deadline: Instant, calls: usize) -> Self {
        Self { deadline, calls }
    }

    /// The timeout of the next git call (`READ_TIMEOUT` at most), or the
    /// reason there is none left (`deadline` / `budget`).
    pub(crate) fn next(&mut self) -> Result<Duration, &'static str> {
        if self.calls == 0 {
            return Err("budget");
        }
        let left = self.left();
        if left.is_zero() {
            return Err("deadline");
        }
        self.calls -= 1;
        Ok(left.min(READ_TIMEOUT))
    }

    /// The time left.
    pub(crate) fn left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// The deadline.
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
}

/// Where the evaluation's git reads run (`forge serve`'s git and storage).
#[derive(Clone)]
pub struct ReviewGit {
    git: Git,
    storage: StorageRoot,
}

impl std::fmt::Debug for ReviewGit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewGit").finish_non_exhaustive()
    }
}

impl ReviewGit {
    pub fn new(git: Git, storage: StorageRoot) -> Self {
        Self { git, storage }
    }
}

/// The git half of the approval facts of one decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFacts {
    pub committers: Committers,
    pub changed: ChangedPathsFact,
    pub codeowners: CodeownersFact,
}

impl GitFacts {
    /// With the database half: the inputs of [`evaluate`] (M3 R6: the
    /// project's settings and the target's merge rule minimum come with
    /// the approval facts, read in the same transaction).
    pub fn inputs(
        self,
        mr: &StoredMergeRequest,
        head: &ObjectId,
        facts: forge_db::review::ApprovalFacts,
    ) -> ApprovalInputs {
        let committer_approvals_excluded = facts.settings.values.committer_approvals_excluded;
        let codeowners_enforced = facts.settings.codeowners.enforced();
        let minimum = facts
            .protection
            .merge_rule(&mr.target_branch)
            .required_approvals;
        ApprovalInputs {
            author: mr.author,
            head: head.clone(), // clone: the inputs own their values
            target: mr.target_branch.clone(),
            facts,
            committers: self.committers,
            changed: self.changed,
            codeowners: self.codeowners,
            committer_approvals_excluded,
            codeowners_enforced,
            minimum,
        }
    }
}

/// Who asks (for the log lines).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Asker<'a> {
    pub(crate) request_id: &'a RequestId,
    pub(crate) project: ProjectId,
    pub(crate) repo: RepoId,
}

pub(crate) fn warn_unread(
    asker: Asker<'_>,
    mr: &StoredMergeRequest,
    what: &'static str,
    e: &ForgeError,
) {
    tracing::warn!(
        target: "forge_review",
        event = "forge.review.approval_input_unread",
        error_code = e.code().id(),
        request_id = %asker.request_id,
        merge_request_id = mr.id.get(),
        project_id = asker.project.get(),
        repo_id = asker.repo.get(),
        input = what,
        error = %e,
        "an input of the approval evaluation could not be read; the merge is blocked"
    );
}
