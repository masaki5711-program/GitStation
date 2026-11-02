//! The git (and push record) reads of one approval evaluation, under one
//! [`Budget`] (module docs of `approvals`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use forge_core::ObjectId;
use forge_db::review::{self, StoredMergeRequest};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::mergeability::{MergeTree, MergeTreeLimits};
use forge_git::review_inputs::{ChangedPaths, CodeownersFile};

use super::codeowners::{self, Codeowners, CodeownersError, Owner};
use super::committers;
use super::{
    warn_unread, Asker, Budget, ChangedPathsFact, CodeownersFact, Committers, GitFacts, OwnerRef,
    ReviewGit, MATCH_BUDGET, MAX_CHANGED_PATHS,
};

/// The longest changed path evaluated (a CODEOWNERS line is at most 4 KiB):
/// longer is `changed_paths_unknown reason=path_too_long` (R5 re-review).
pub const MAX_PATH_BYTES: usize = 4096;

/// A git error's reason for an unknown fact.
fn git_reason(e: &ForgeError) -> &'static str {
    if e.code() == ErrorCode::GitTimeout {
        "timeout"
    } else {
        "git_failed"
    }
}

/// The budget ran out before a call (logged like a git timeout).
fn spent(asker: Asker<'_>, mr: &StoredMergeRequest, what: &'static str, reason: &'static str) {
    let e = ForgeError::new(ErrorCode::GitTimeout)
        .with_context("reason", "approval_budget")
        .with_context("detail", reason);
    warn_unread(asker, mr, what, &e);
}

impl ReviewGit {
    /// The git facts of merge request `mr` at `head` into the target tip
    /// `target_tip` (`None`: the target branch is missing), every call
    /// within `budget`. Only the database is an error; every git failure
    /// or an exhausted budget is an unknown fact (module docs).
    pub(crate) async fn facts(
        &self,
        db: &Db,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        head: &ObjectId,
        target_tip: Option<&ObjectId>,
        budget: &mut Budget,
    ) -> Result<GitFacts, ForgeError> {
        let Some(tip) = target_tip else {
            // R5 re-review round 2: one warn for the evaluation that cannot
            // run (the target branch is gone).
            let e = ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                .with_context("reason", "ref_not_found")
                .with_context("branch", "target_branch");
            warn_unread(asker, mr, "target_tip", &e);
            return Ok(GitFacts {
                committers: Committers::Unknown("target_missing"),
                changed: ChangedPathsFact::Unknown("target_missing"),
                codeowners: CodeownersFact::Invalid(CodeownersError::file("read_failed")),
            });
        };
        let changed = self.changed(asker, mr, head, tip, budget).await;
        let committers = self.committers(db, asker, mr, head, tip, budget).await?;
        let codeowners = self
            .codeowners(db, asker, mr, tip, &changed, budget)
            .await?;
        Ok(GitFacts {
            committers,
            changed,
            codeowners,
        })
    }

    /// The paths the merge changes on the target: the target tip against
    /// the trial merge's tree (R5 review: a criss-cross history has more
    /// than one merge base; the result of the merge is what lands).
    async fn changed(
        &self,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        head: &ObjectId,
        tip: &ObjectId,
        budget: &mut Budget,
    ) -> ChangedPathsFact {
        let timeout = match budget.next() {
            Ok(t) => t,
            Err(reason) => {
                spent(asker, mr, "changed_paths", reason);
                return ChangedPathsFact::Unknown("timeout");
            }
        };
        let limits = MergeTreeLimits {
            timeout,
            ..MergeTreeLimits::default()
        };
        let tree = match self
            .git
            .merge_tree(&self.storage, asker.repo, tip, head, limits)
            .await
        {
            Ok(MergeTree::Clean { tree }) => tree,
            // A conflict blocks the merge on its own; what it would change
            // is not known.
            Ok(MergeTree::Conflicted { .. }) => return ChangedPathsFact::Conflict,
            Err(e) => {
                warn_unread(asker, mr, "merge_tree", &e);
                return ChangedPathsFact::Unknown(git_reason(&e));
            }
        };
        let timeout = match budget.next() {
            Ok(t) => t,
            Err(reason) => {
                spent(asker, mr, "changed_paths", reason);
                return ChangedPathsFact::Unknown("timeout");
            }
        };
        match self
            .git
            .changed_paths(
                &self.storage,
                asker.repo,
                tip,
                &tree,
                MAX_CHANGED_PATHS,
                timeout,
            )
            .await
        {
            Ok(ChangedPaths::Complete(paths)) => {
                // R5 re-review round 2: a path longer than a CODEOWNERS line
                // could hold is never matched; fail closed, once.
                if let Some(long) = paths.iter().find(|p| p.len() > MAX_PATH_BYTES) {
                    let e = ForgeError::new(ErrorCode::GitOutputTooLarge)
                        .with_context("reason", "path_too_long")
                        .with_context("path_bytes", long.len())
                        .with_context("limit", MAX_PATH_BYTES);
                    warn_unread(asker, mr, "changed_paths", &e);
                    return ChangedPathsFact::Unknown("path_too_long");
                }
                ChangedPathsFact::Known(paths)
            }
            Ok(ChangedPaths::TooMany) => ChangedPathsFact::Unknown("too_many_paths"),
            Ok(ChangedPaths::NonUtf8) => ChangedPathsFact::Unknown("non_utf8_path"),
            Err(e) => {
                warn_unread(asker, mr, "changed_paths", &e);
                ChangedPathsFact::Unknown(git_reason(&e))
            }
        }
    }

    async fn committers(
        &self,
        db: &Db,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        head: &ObjectId,
        tip: &ObjectId,
        budget: &mut Budget,
    ) -> Result<Committers, ForgeError> {
        let Ok(timeout) = budget.next() else {
            spent(asker, mr, "range_commits", "budget");
            return Ok(Committers::Unknown("timeout"));
        };
        let range = match self
            .git
            .range_commits(
                &self.storage,
                asker.repo,
                Some(tip),
                head,
                committers::MAX_RANGE_COMMITS,
                timeout,
            )
            .await
        {
            Ok(Some(range)) => range,
            Ok(None) => return Ok(Committers::Unknown("range_too_long")),
            Err(e) => {
                warn_unread(asker, mr, "range_commits", &e);
                return Ok(Committers::Unknown(git_reason(&e)));
            }
        };
        // The push records, bounded by the budget's time (the merge job:
        // the lock's hold).
        let read = review::push_coverage(db, asker.project, asker.repo, &range.commits);
        let coverage = match tokio::time::timeout(budget.left(), read).await {
            Ok(coverage) => coverage?,
            Err(_) => {
                spent(asker, mr, "push_coverage", "deadline");
                return Ok(Committers::Unknown("timeout"));
            }
        };
        let cut = committers::cut_entries(&coverage);
        if cut.is_empty() {
            return Ok(committers::committers_of(
                &range.commits,
                &coverage,
                &BTreeMap::new(),
            ));
        }
        // One git call for every cut update (R5 re-review round 2): what
        // their tips reach beyond the target tip, decided in memory.
        let tips: Vec<ObjectId> = cut
            .iter()
            .filter_map(|i| coverage.entries.get(*i).and_then(|e| e.new.clone())) // clone: the call owns its tips
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // R5 round 3: a cut tip no longer in the repository (pruned) that no
        // ref points at reaches nothing now; a missing tip a ref points at is
        // a broken repository (unknown). Otherwise the graph call would fail
        // on every evaluation once one historical tip is pruned.
        let tips = match self.present_tips(asker, mr, tips, budget).await {
            Ok(tips) => tips,
            Err(reason) => return Ok(Committers::Unknown(reason)),
        };
        let Ok(timeout) = budget.next() else {
            spent(asker, mr, "push_reach", "budget");
            return Ok(Committers::Unknown("timeout"));
        };
        let graph = match self
            .git
            .commit_graph(
                &self.storage,
                asker.repo,
                &tips,
                tip,
                committers::MAX_GRAPH_COMMITS,
                timeout,
            )
            .await
        {
            Ok(Some(graph)) => graph,
            Ok(None) => return Ok(Committers::Unknown("push_graph_too_large")),
            Err(e) => {
                warn_unread(asker, mr, "push_reach", &e);
                return Ok(Committers::Unknown(git_reason(&e)));
            }
        };
        let reach = committers::reach_in(&graph, &range.commits, &coverage);
        Ok(committers::committers_of(&range.commits, &coverage, &reach))
    }

    /// `tips` without the ones not in the repository that no ref points at
    /// (two git calls, only when some are missing); `Err(reason)` when a
    /// call fails or a missing tip is still referenced.
    async fn present_tips(
        &self,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        tips: Vec<ObjectId>,
        budget: &mut Budget,
    ) -> Result<Vec<ObjectId>, &'static str> {
        let Ok(timeout) = budget.next() else {
            spent(asker, mr, "push_reach", "budget");
            return Err("timeout");
        };
        let missing = match self
            .git
            .missing_objects(&self.storage, asker.repo, &tips, timeout)
            .await
        {
            Ok(missing) => missing,
            Err(e) => {
                warn_unread(asker, mr, "push_reach", &e);
                return Err(git_reason(&e));
            }
        };
        if missing.is_empty() {
            return Ok(tips);
        }
        let Ok(timeout) = budget.next() else {
            spent(asker, mr, "push_reach", "budget");
            return Err("timeout");
        };
        let referenced = match self
            .git
            .referenced_objects(&self.storage, asker.repo, timeout)
            .await
        {
            Ok(referenced) => referenced,
            Err(e) => {
                warn_unread(asker, mr, "push_reach", &e);
                return Err(git_reason(&e));
            }
        };
        if missing.iter().any(|m| referenced.contains(m)) {
            return Err("push_tip_missing");
        }
        Ok(tips.into_iter().filter(|t| !missing.contains(t)).collect())
    }

    /// The target tip's CODEOWNERS, resolved and matched against the
    /// changed paths (in a blocking task within [`MATCH_BUDGET`]). One warn
    /// `forge.review.codeowners_invalid` (FG-REVIEW-0014) when it cannot be
    /// used — also for a git failure (its code rides along).
    async fn codeowners(
        &self,
        db: &Db,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        tip: &ObjectId,
        changed: &ChangedPathsFact,
        budget: &mut Budget,
    ) -> Result<CodeownersFact, ForgeError> {
        let mut git_error: Option<ForgeError> = None;
        let mut join_error: Option<String> = None;
        let (path, parsed) = match budget.next() {
            // Out of budget: unread (the one warn below says so).
            Err(_) => ("", Err(CodeownersError::file("read_failed"))),
            Ok(timeout) => match self
                .git
                .codeowners_file(
                    &self.storage,
                    asker.repo,
                    tip,
                    codeowners::MAX_BYTES,
                    timeout,
                )
                .await
            {
                Ok(CodeownersFile::Absent) => return Ok(CodeownersFact::Absent),
                Ok(CodeownersFile::Found { path, bytes }) => (path, codeowners::parse(&bytes)),
                Ok(CodeownersFile::TooLarge { path }) => {
                    (path, Err(CodeownersError::file("too_large")))
                }
                Ok(CodeownersFile::NotAFile { path }) => {
                    (path, Err(CodeownersError::file("not_a_file")))
                }
                Err(e) => {
                    git_error = Some(e);
                    ("", Err(CodeownersError::file("read_failed")))
                }
            },
        };
        let resolved = match parsed {
            Ok(file) => resolve(db, file).await?,
            Err(e) => Err(e),
        };
        let fact = match resolved {
            Ok((file, owners)) => {
                let paths = match changed {
                    ChangedPathsFact::Known(paths) => paths.clone(), // clone: moved to the blocking task
                    ChangedPathsFact::Conflict | ChangedPathsFact::Unknown(_) => Vec::new(),
                };
                match matched(file.clone(), paths, budget.deadline()).await {
                    // clone: the task owns its copy
                    Ok(matched) => CodeownersFact::Valid {
                        path,
                        file,
                        owners,
                        matched,
                    },
                    Err((reason, join)) => {
                        join_error = join;
                        CodeownersFact::Invalid(CodeownersError::file(reason))
                    }
                }
            }
            Err(e) => CodeownersFact::Invalid(e),
        };
        if let CodeownersFact::Invalid(e) = &fact {
            tracing::warn!(
                target: "forge_review",
                event = "forge.review.codeowners_invalid",
                error_code = ErrorCode::CodeownersInvalid.id(),
                git_error_code = git_error.as_ref().map(|g| g.code().id()),
                git_error = git_error.as_ref().map(|g| g.to_string()),
                join_error = join_error.as_deref(),
                request_id = %asker.request_id,
                merge_request_id = mr.id.get(),
                project_id = asker.project.get(),
                target_sha = tip.as_str(),
                path,
                line = e.line,
                reason = e.reason,
                "the target branch's CODEOWNERS cannot be used; the merge is blocked"
            );
        }
        Ok(fact)
    }
}

/// The CODEOWNERS lines the changed paths need (line → paths), computed in
/// a blocking task until the earlier of [`MATCH_BUDGET`] and `deadline`
/// (checked inside the matching, and the await itself bounded). Past it:
/// `match_budget`; the task panicked: `match_failed` with the join message
/// (R5 re-review round 2) — both fail closed.
async fn matched(
    file: Codeowners,
    paths: Vec<String>,
    deadline: Instant,
) -> Result<BTreeMap<u32, usize>, (&'static str, Option<String>)> {
    let deadline = deadline.min(Instant::now() + MATCH_BUDGET);
    let task =
        tokio::task::spawn_blocking(move || codeowners::owned_lines(&file, &paths, Some(deadline)));
    // A little past the deadline: the task checks it itself and returns.
    let bound = deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(500);
    match tokio::time::timeout(bound, task).await {
        Ok(Ok(Some(lines))) => Ok(lines),
        Ok(Ok(None)) | Err(_) => Err(("match_budget", None)),
        Ok(Err(join)) => Err(("match_failed", Some(join.to_string()))),
    }
}

/// Resolve every owner of `file`: a single name is an active user, else a
/// top-level group of that path; `@a/b` a group by its full path. An
/// owner that does not resolve (missing, disabled, or not visible) is
/// `unknown_owner` on the first line naming it — the same answer for every
/// kind of absence (R5 review: no existence oracle).
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
