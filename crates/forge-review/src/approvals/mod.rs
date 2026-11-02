//! Approvals, approval rules and CODEOWNERS (M3 R5, ADR-0024 §8, §9).
//!
//! - [`codeowners`]: the CODEOWNERS grammar (pure).
//! - [`evaluate`]: how each approval counts and what blocks the merge
//!   (pure; `evaluate_merge()` calls it on [`MergeFacts::approvals`]).
//! - [`committers`]: who pushed the range's commits (pure, on the push
//!   records and git's answers).
//! - Here: [`ReviewGit`], the git reads the evaluation needs (the changed
//!   paths of `merge_base..head`, the range, the target tip's CODEOWNERS —
//!   **never** the merge request's own: ADR-0024 §9), and the commands
//!   [`Review::approve`], [`Review::unapprove`], [`Review::approval_state`]
//!   and the rule commands.
//!
//! A git read that fails or times out never means "nothing changed" / "no
//! owners" / "no committers": the fact is unknown and the merge blocked
//! (`changed_paths_unknown`, `codeowners_invalid reason=read_failed`,
//! `committers_unknown`), with one warn line carrying the git error's code.
//!
//! [`MergeFacts::approvals`]: crate::merges::MergeFacts::approvals

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use forge_core::{ObjectId, ProjectId, RepoId, RequestId};
use forge_db::review::{self, StoredMergeRequest};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::review_inputs::{ChangedPaths, CodeownersFile};
use forge_git::{Git, StorageRoot};

pub mod codeowners;
mod commands;
pub mod committers;
pub mod evaluate;

pub use codeowners::{Codeowners, CodeownersError, Owner};
pub use commands::{ApprovalAnswer, ApprovalState, RuleAsk};
pub use evaluate::{
    evaluate, is_config_path, ApprovalInputs, ApprovalReport, ApprovalStatus, ChangedPathsFact,
    CodeownerStatus, CodeownersFact, Committers, OwnerRef, RuleStatus,
};

/// The most changed paths evaluated (decided in R5; ADR-0024 未決): more is
/// `changed_paths_unknown reason=too_many_paths`.
pub const MAX_CHANGED_PATHS: usize = 10_000;

/// The bound of each git read of an evaluation (decided in R5): one
/// `merge-base`, `diff-tree`, `rev-list` or `ls-tree` / `cat-file`. Under
/// the merge lock the job cuts it to what is left of the lock's hold.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

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

/// The git half of the approval facts of one decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFacts {
    pub committers: Committers,
    pub changed: ChangedPathsFact,
    pub codeowners: CodeownersFact,
}

impl GitFacts {
    /// With the database half: the inputs of [`evaluate`].
    pub fn inputs(
        self,
        mr: &StoredMergeRequest,
        head: &ObjectId,
        facts: review::ApprovalFacts,
    ) -> ApprovalInputs {
        ApprovalInputs {
            author: mr.author,
            head: head.clone(), // clone: the inputs own their values
            target: mr.target_branch.clone(),
            facts,
            committers: self.committers,
            changed: self.changed,
            codeowners: self.codeowners,
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

fn warn_git(asker: Asker<'_>, mr: &StoredMergeRequest, what: &'static str, e: &ForgeError) {
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

impl ReviewGit {
    pub fn new(git: Git, storage: StorageRoot) -> Self {
        Self { git, storage }
    }

    /// The git facts of merge request `mr` at `head` into the target tip
    /// `target_tip` (`None`: the target branch is missing): the merge base,
    /// the changed paths, the committers (with the push records from the
    /// database) and the target tip's CODEOWNERS with its owners resolved.
    /// `timeout` bounds each git call. Only the database is an error;
    /// every git failure is an unknown fact (module docs).
    pub(crate) async fn facts(
        &self,
        db: &Db,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        head: &ObjectId,
        target_tip: Option<&ObjectId>,
        timeout: Duration,
    ) -> Result<GitFacts, ForgeError> {
        let repo = asker.repo;
        let Some(tip) = target_tip else {
            return Ok(GitFacts {
                committers: Committers::Unknown("target_missing"),
                changed: ChangedPathsFact::Unknown("target_missing"),
                codeowners: CodeownersFact::Invalid(CodeownersError::file("read_failed")),
            });
        };
        let base = match self
            .git
            .merge_base(&self.storage, repo, tip, head, timeout)
            .await
        {
            Ok(base) => base,
            Err(e) => {
                warn_git(asker, mr, "merge_base", &e);
                None
            }
        };
        let changed = match &base {
            None => ChangedPathsFact::Unknown("no_merge_base"),
            Some(base) => match self
                .git
                .changed_paths(&self.storage, repo, base, head, MAX_CHANGED_PATHS, timeout)
                .await
            {
                Ok(ChangedPaths::Complete(paths)) => ChangedPathsFact::Known(paths),
                Ok(ChangedPaths::TooMany) => ChangedPathsFact::Unknown("too_many_paths"),
                Ok(ChangedPaths::NonUtf8) => ChangedPathsFact::Unknown("non_utf8_path"),
                Err(e) => {
                    warn_git(asker, mr, "changed_paths", &e);
                    ChangedPathsFact::Unknown(if e.code() == ErrorCode::GitTimeout {
                        "timeout"
                    } else {
                        "git_failed"
                    })
                }
            },
        };
        let committers = self
            .committers(db, asker, mr, base.as_ref(), head, timeout)
            .await?;
        let codeowners = self.codeowners(db, asker, mr, tip, timeout).await?;
        Ok(GitFacts {
            committers,
            changed,
            codeowners,
        })
    }

    async fn committers(
        &self,
        db: &Db,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        base: Option<&ObjectId>,
        head: &ObjectId,
        timeout: Duration,
    ) -> Result<Committers, ForgeError> {
        let Some(base) = base else {
            return Ok(Committers::Unknown("no_merge_base"));
        };
        let range = match self
            .git
            .range_commits(
                &self.storage,
                asker.repo,
                Some(base),
                head,
                committers::MAX_RANGE_COMMITS,
                timeout,
            )
            .await
        {
            Ok(Some(range)) => range,
            Ok(None) => return Ok(Committers::Unknown("range_too_long")),
            Err(e) => {
                warn_git(asker, mr, "range_commits", &e);
                return Ok(Committers::Unknown("git_failed"));
            }
        };
        let coverage = review::push_coverage(db, asker.project, asker.repo, &range).await?;
        let Some(partial) = committers::partial_entries(&range, &coverage) else {
            return Ok(Committers::Unknown("too_many_partial_pushes"));
        };
        let range_set: BTreeSet<&ObjectId> = range.iter().collect();
        let mut reach = BTreeMap::new();
        for index in partial {
            let Some(new) = coverage.entries.get(index).and_then(|e| e.new.as_ref()) else {
                continue;
            };
            let reached = match self
                .git
                .range_commits(
                    &self.storage,
                    asker.repo,
                    Some(base),
                    new,
                    committers::MAX_RANGE_COMMITS,
                    timeout,
                )
                .await
            {
                Ok(Some(commits)) => Some(
                    commits
                        .into_iter()
                        .filter(|c| range_set.contains(c))
                        .collect::<BTreeSet<_>>(),
                ),
                Ok(None) => None,
                Err(e) => {
                    warn_git(asker, mr, "push_reach", &e);
                    None
                }
            };
            reach.insert(index, reached);
        }
        Ok(committers::committers_of(&range, &coverage, &reach))
    }

    async fn codeowners(
        &self,
        db: &Db,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        tip: &ObjectId,
        timeout: Duration,
    ) -> Result<CodeownersFact, ForgeError> {
        let read = self
            .git
            .codeowners_file(
                &self.storage,
                asker.repo,
                tip,
                codeowners::MAX_BYTES,
                timeout,
            )
            .await;
        let (path, parsed) = match read {
            Ok(CodeownersFile::Absent) => return Ok(CodeownersFact::Absent),
            Ok(CodeownersFile::Found { path, bytes }) => (path, codeowners::parse(&bytes)),
            Ok(CodeownersFile::TooLarge { path }) => {
                (path, Err(CodeownersError::file("too_large")))
            }
            Ok(CodeownersFile::NotAFile { path }) => {
                (path, Err(CodeownersError::file("not_a_file")))
            }
            Err(e) => {
                warn_git(asker, mr, "codeowners", &e);
                ("", Err(CodeownersError::file("read_failed")))
            }
        };
        let resolved = match parsed {
            Ok(file) => resolve(db, file).await?,
            Err(e) => Err(e),
        };
        Ok(match resolved {
            Ok((file, owners)) => CodeownersFact::Valid { path, file, owners },
            Err(e) => {
                tracing::warn!(
                    target: "forge_review",
                    event = "forge.review.codeowners_invalid",
                    error_code = ErrorCode::CodeownersInvalid.id(),
                    request_id = %asker.request_id,
                    merge_request_id = mr.id.get(),
                    project_id = asker.project.get(),
                    target_sha = tip.as_str(),
                    path,
                    line = e.line,
                    reason = e.reason,
                    "the target branch's CODEOWNERS cannot be used; the merge is blocked"
                );
                CodeownersFact::Invalid(e)
            }
        })
    }
}

/// Resolve every owner of `file`: a single name is a user, else a
/// top-level group of that path; `@a/b` a group by its full path. An
/// unknown owner is `unknown_owner` on the first line naming it.
async fn resolve(
    db: &Db,
    file: Codeowners,
) -> Result<Result<(Codeowners, BTreeMap<Owner, OwnerRef>), CodeownersError>, ForgeError> {
    let owners = file.owners();
    let names: Vec<String> = owners
        .iter()
        .filter_map(|o| match o {
            Owner::Name(n) => Some(n.clone()), // clone: the query binds owned names
            Owner::Group(_) => None,
        })
        .collect();
    let paths: Vec<String> = owners
        .iter()
        .map(|o| match o {
            Owner::Name(n) | Owner::Group(n) => n.clone(), // clone: as above
        })
        .collect();
    let found = review::owners(db, &names, &paths).await?;
    let users: BTreeMap<&str, forge_core::UserId> = found
        .users
        .iter()
        .map(|(n, id)| (n.as_str(), *id))
        .collect();
    let groups: BTreeMap<&str, forge_core::GroupId> = found
        .groups
        .iter()
        .map(|(p, id)| (p.as_str(), *id))
        .collect();
    let mut resolved = BTreeMap::new();
    for owner in owners {
        let r = match &owner {
            Owner::Name(n) => users
                .get(n.as_str())
                .map(|u| OwnerRef::User(*u))
                .or_else(|| groups.get(n.as_str()).map(|g| OwnerRef::Group(*g))),
            Owner::Group(p) => groups.get(p.as_str()).map(|g| OwnerRef::Group(*g)),
        };
        match r {
            Some(r) => {
                resolved.insert(owner, r);
            }
            None => {
                let line = file
                    .rules
                    .iter()
                    .find(|rule| rule.owners.contains(&owner))
                    .map_or(0, |rule| rule.line);
                return Ok(Err(CodeownersError {
                    line,
                    reason: "unknown_owner",
                }));
            }
        }
    }
    Ok(Ok((file, resolved)))
}
