//! Step 1 of the train lint: is the repository strict
//! (`merge_queue.doc_lint_strict` of `.forge/repo.yml`)?
//!
//! - The base's file decides whenever it is valid or absent, and the last
//!   step is not read: a member of the train never weakens (or sets) the
//!   rule for its own train.
//! - A base whose file cannot be read as a valid one (`FG-REPO-0024`) says
//!   nothing, so the train's last step decides: a valid file there — the
//!   repair — counts with its own value, and no file there is "not
//!   strict". Otherwise every train of a repository with old problems
//!   would be removed, the repair included, and nothing could ever land.
//! - Not valid on both: strict (an unreadable setting never relaxes the
//!   rule), and the verdict names the config as the cause
//!   ([`Strictness::ConfigInvalid`]).

use forge_core::repo_config::RepoConfigProblem;
use forge_core::{ObjectId, RepoId, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;

use super::read_failed;
use crate::spec_refs::{read_repo_config, RepoConfigRead};

/// What step 1 decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Strictness {
    NotStrict,
    /// A valid file says so.
    Strict,
    /// Strict because the file is not valid on the base and on the last
    /// step: the base's first broken rule and its line.
    ConfigInvalid(RepoConfigProblem),
}

impl Strictness {
    pub(super) fn is_strict(self) -> bool {
        match self {
            Self::NotStrict => false,
            Self::Strict | Self::ConfigInvalid(_) => true,
        }
    }

    fn of(strict: bool) -> Self {
        if strict {
            Self::Strict
        } else {
            Self::NotStrict
        }
    }
}

/// What the base's file decides alone; `Err`: it is not valid (why), and
/// the last step is asked ([`by_tip`]).
fn by_base(base: &RepoConfigRead) -> Result<Strictness, RepoConfigProblem> {
    match base {
        RepoConfigRead::Absent => Ok(Strictness::NotStrict),
        RepoConfigRead::Valid(config) => Ok(Strictness::of(config.merge_queue.doc_lint_strict)),
        RepoConfigRead::Invalid(problem) => Err(*problem),
    }
}

/// The decision for a base that is not valid (`problem`), by the last
/// step's file, and the word the log line says it with.
fn by_tip(problem: RepoConfigProblem, tip: &RepoConfigRead) -> (Strictness, &'static str) {
    match tip {
        RepoConfigRead::Absent => (Strictness::NotStrict, "train_removes_the_file"),
        RepoConfigRead::Valid(config) => (
            Strictness::of(config.merge_queue.doc_lint_strict),
            "train_repairs_the_file",
        ),
        RepoConfigRead::Invalid(_) => (Strictness::ConfigInvalid(problem), "invalid_on_both"),
    }
}

/// Step 1 for a train built on `base` whose last step is `tip` (module
/// docs). Errors: `FG-DOC-0002 scope=train reason=read_failed` (the file
/// could not be read at all — never "not strict").
pub(super) async fn strictness(
    browse: &Browser,
    repo: RepoId,
    (base, tip): (&ObjectId, &ObjectId),
    request_id: &RequestId,
) -> Result<Strictness, ForgeError> {
    let at_base = read_repo_config(browse, repo, base.as_str())
        .await
        .map_err(read_failed)?;
    let problem = match by_base(&at_base) {
        Ok(decided) => return Ok(decided),
        Err(problem) => problem,
    };
    let at_tip = read_repo_config(browse, repo, tip.as_str())
        .await
        .map_err(read_failed)?;
    let (decided, decided_by) = by_tip(problem, &at_tip);
    // Info: the removal this may lead to is the queue's line (warn, with
    // the same code); this one says how strictness was decided.
    tracing::info!(
        target: "forge_checks",
        event = "forge.train_lint.repo_config_invalid",
        error_code = ErrorCode::RepoConfigInvalid.id(),
        request_id = %request_id,
        repo_id = repo.get(),
        base_sha = base.as_str(),
        train_sha = tip.as_str(),
        line = problem.line,
        reason = problem.reason,
        decided_by,
        strict = decided.is_strict(),
        "the base's .forge/repo.yml is not valid: the train's last step decides whether the \
         repository is strict"
    );
    Ok(decided)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

    use forge_core::repo_config::parse_repo_config;

    use super::*;

    fn valid(strict: bool) -> RepoConfigRead {
        let text = format!("merge_queue:\n  doc_lint_strict: {strict}\n");
        RepoConfigRead::Valid(parse_repo_config(text.as_bytes()).expect("a valid file"))
    }

    const BROKEN: RepoConfigProblem = RepoConfigProblem {
        line: 2,
        reason: "doc_lint_strict_invalid",
    };

    #[test]
    fn a_valid_or_absent_base_decides_alone() {
        assert_eq!(by_base(&RepoConfigRead::Absent), Ok(Strictness::NotStrict));
        assert_eq!(by_base(&valid(false)), Ok(Strictness::NotStrict));
        assert_eq!(by_base(&valid(true)), Ok(Strictness::Strict));
        assert_eq!(by_base(&RepoConfigRead::Invalid(BROKEN)), Err(BROKEN));
    }

    #[test]
    fn an_invalid_base_is_decided_by_the_last_step() {
        assert_eq!(
            by_tip(BROKEN, &valid(false)),
            (Strictness::NotStrict, "train_repairs_the_file")
        );
        assert_eq!(
            by_tip(BROKEN, &valid(true)),
            (Strictness::Strict, "train_repairs_the_file")
        );
        assert_eq!(
            by_tip(BROKEN, &RepoConfigRead::Absent),
            (Strictness::NotStrict, "train_removes_the_file")
        );
        // Not valid on both: strict, and the cause is the base's problem
        // (not the last step's own).
        let other = RepoConfigProblem {
            line: 9,
            reason: "too_large",
        };
        assert_eq!(
            by_tip(BROKEN, &RepoConfigRead::Invalid(other)),
            (Strictness::ConfigInvalid(BROKEN), "invalid_on_both")
        );
        assert!(Strictness::ConfigInvalid(BROKEN).is_strict());
        assert!(Strictness::Strict.is_strict());
        assert!(!Strictness::NotStrict.is_strict());
    }
}
