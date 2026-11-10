//! The confidential mark of a CI pipeline's commit (M4 C2, ADR-0029
//! decision 5): measured when the pipeline is created, with the rule of
//! every read of file content and of FG-AUTH-0022
//! (`forge_checks::confidential::confidential_at`). forge-pipelines asks
//! through its `MarkMeter` (a feature crate never depends on another).
//!
//! A commit with no confidential file is `open`; one with any (or a bound
//! that left documents unread) `confidential`; a measurement that did not
//! complete (its 20 s timeout, a git error) `undecided`, logged at warn —
//! the CI outputs of an undecided pipeline are refused to an agent (C5).

use forge_checks::confidential::{confidential_at, ConfidentialLimits};
use forge_core::{ObjectId, RepoId};
use forge_git::browse::Browser;
use forge_pipelines::{CopyMark, MarkFuture, MarkMeter};

/// The meter forge serve and forge worker hand to forge-pipelines.
pub(crate) struct ConfidentialMeter {
    browse: Browser,
    limits: ConfidentialLimits,
}

impl ConfidentialMeter {
    pub(crate) fn new(browse: Browser) -> Self {
        Self {
            browse,
            limits: ConfidentialLimits::default(),
        }
    }
}

impl MarkMeter for ConfidentialMeter {
    fn measure<'a>(&'a self, repo: RepoId, commit: &'a ObjectId) -> MarkFuture<'a> {
        Box::pin(async move {
            match confidential_at(&self.browse, repo, commit.as_str(), &self.limits).await {
                // A bound left documents unread: not known open.
                Ok(found) if found.is_partial() => CopyMark::Undecided,
                Ok(found) if found.is_empty() => CopyMark::Open,
                Ok(_) => CopyMark::Confidential,
                Err(e) => {
                    tracing::warn!(
                        target: "forge_cli",
                        event = "forge.ci.copy_mark_undecided",
                        error_code = e.code().id(),
                        repo_id = repo.get(),
                        commit = commit.as_str(),
                        context = ?e.context(),
                        "the confidential marks of a pipeline's commit could not be measured; undecided"
                    );
                    CopyMark::Undecided
                }
            }
        })
    }
}
