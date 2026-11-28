//! The git (and push record) reads of one approval evaluation, under one
//! [`Budget`] (module docs of `approvals`).

use std::collections::BTreeMap;
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
    ReviewGit, SelectorTargets, MATCH_BUDGET, MAX_CHANGED_PATHS,
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

/// git's answer about the changed paths as the fact.
fn paths_fact(asker: Asker<'_>, mr: &StoredMergeRequest, paths: ChangedPaths) -> ChangedPathsFact {
    match paths {
        ChangedPaths::Complete(paths) => {
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
        ChangedPaths::TooMany => ChangedPathsFact::Unknown("too_many_paths"),
        ChangedPaths::NonUtf8 => ChangedPathsFact::Unknown("non_utf8_path"),
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
    /// or an exhausted budget is an unknown fact (module docs). Plan 18
    /// S10: [`ReviewGit::reads`] then [`ReviewGit::facts_from`].
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
                targets: SelectorTargets::default(),
            });
        };
        let reads = self.reads(db, asker, mr, head, tip, budget).await?;
        facts_from(db, asker, mr, reads, budget).await
    }

    /// Plan 18 S10: the git half of [`ReviewGit::facts`] — every git call
    /// of the evaluation, a function of (`head`, `tip`) only (objects do
    /// not change), so a merge-queue build can read them before the
    /// landing, and the landing use them when its shas are the same.
    pub(crate) async fn reads(
        &self,
        db: &Db,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        head: &ObjectId,
        tip: &ObjectId,
        budget: &mut Budget,
    ) -> Result<GitReads, ForgeError> {
        // Load test 2026-11-19: the merge result the mergeability check
        // wrote for exactly (head, tip) is the trial merge's tree as a
        // commit (`refs/merge-requests/<iid>/merge`): its paths against the
        // tip are the paths the merge changes, without a `merge-tree`.
        let stored = review::merge_result_of_read(db, mr.id, head, tip).await?;
        let changed = self
            .changed(asker, mr, (head, tip), stored.as_ref(), budget)
            .await;
        let range = self.range(asker, mr, head, tip, budget).await;
        let codeowners = self.codeowners_read(asker, tip, budget).await;
        Ok(GitReads {
            head: head.clone(), // clone: the reads name their shas
            tip: tip.clone(),   // clone: as above
            changed,
            range,
            codeowners,
        })
    }

    /// The paths the merge changes on the target: the target tip against
    /// the trial merge's tree (R5 review: a criss-cross history has more
    /// than one merge base; the result of the merge is what lands).
    ///
    /// `stored`: the merge result recorded for exactly (`head`, `tip`) —
    /// a commit of the clean trial merge's tree on the tip (the same
    /// `merge-tree` of the same pair), so its paths against the tip are
    /// the same paths. Read when given; a read that fails (the commit was
    /// pruned) falls back to the trial merge, never to "unknown".
    async fn changed(
        &self,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        (head, tip): (&ObjectId, &ObjectId),
        stored: Option<&ObjectId>,
        budget: &mut Budget,
    ) -> ChangedPathsFact {
        if let Some(result) = stored {
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
                    result,
                    MAX_CHANGED_PATHS,
                    timeout,
                )
                .await
            {
                Ok(paths) => return paths_fact(asker, mr, paths),
                Err(e) => tracing::info!(
                    target: "forge_review",
                    event = "forge.review.merge_result_unread",
                    error_code = e.code().id(),
                    request_id = %asker.request_id,
                    merge_request_id = mr.id.get(),
                    project_id = asker.project.get(),
                    result_sha = result.as_str(),
                    error = %e,
                    "the stored merge result could not be read; the trial merge is computed instead"
                ),
            }
        }
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
            Ok(paths) => paths_fact(asker, mr, paths),
            Err(e) => {
                warn_unread(asker, mr, "changed_paths", &e);
                ChangedPathsFact::Unknown(git_reason(&e))
            }
        }
    }

    /// The range's commits (newest first), or why they are unknown.
    async fn range(
        &self,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        head: &ObjectId,
        tip: &ObjectId,
        budget: &mut Budget,
    ) -> RangeRead {
        let Ok(timeout) = budget.next() else {
            spent(asker, mr, "range_commits", "budget");
            return RangeRead::Unknown("timeout");
        };
        match self
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
            Ok(Some(range)) => RangeRead::Commits(range.commits),
            Ok(None) => RangeRead::Unknown("range_too_long"),
            Err(e) => {
                warn_unread(asker, mr, "range_commits", &e);
                RangeRead::Unknown(git_reason(&e))
            }
        }
    }

    /// The target tip's CODEOWNERS file as git answers it (read and
    /// parsed; out of budget: unread).
    async fn codeowners_read(
        &self,
        asker: Asker<'_>,
        tip: &ObjectId,
        budget: &mut Budget,
    ) -> CodeownersRead {
        let Ok(timeout) = budget.next() else {
            // Out of budget: unread (the one warn of `codeowners_from`).
            return CodeownersRead::Unread {
                path: "",
                parsed: Err(CodeownersError::file("read_failed")),
                git_error: None,
                transient: true,
            };
        };
        let (path, parsed, git_error) = match self
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
            Ok(CodeownersFile::Absent) => return CodeownersRead::Absent,
            Ok(CodeownersFile::Found { path, bytes }) => (path, codeowners::parse(&bytes), None),
            Ok(CodeownersFile::TooLarge { path }) => {
                (path, Err(CodeownersError::file("too_large")), None)
            }
            Ok(CodeownersFile::NotAFile { path }) => {
                (path, Err(CodeownersError::file("not_a_file")), None)
            }
            Err(e) => ("", Err(CodeownersError::file("read_failed")), Some(e)),
        };
        let transient = git_error.is_some();
        CodeownersRead::Unread {
            path,
            parsed,
            git_error,
            transient,
        }
    }
}

/// The git reads of one approval evaluation (plan 18 S10,
/// [`ReviewGit::reads`]): what git answered for (`head`, `tip`).
#[derive(Debug)]
pub(crate) struct GitReads {
    pub(crate) head: ObjectId,
    pub(crate) tip: ObjectId,
    changed: ChangedPathsFact,
    range: RangeRead,
    codeowners: CodeownersRead,
}

impl GitReads {
    /// Whether every answer is git's own about these objects: none is a
    /// timeout, a spent budget or a failed git (those may answer otherwise
    /// when asked again, so such reads are never kept for later).
    pub(crate) fn settled(&self) -> bool {
        let transient = |reason: &str| matches!(reason, "timeout" | "git_failed");
        let changed = !matches!(&self.changed, ChangedPathsFact::Unknown(r) if transient(r));
        let range = !matches!(&self.range, RangeRead::Unknown(r) if transient(r));
        let codeowners = !matches!(
            &self.codeowners,
            CodeownersRead::Unread {
                transient: true,
                ..
            }
        );
        changed && range && codeowners
    }
}

/// The range's commits as git answered.
#[derive(Debug)]
enum RangeRead {
    Commits(Vec<ObjectId>),
    Unknown(&'static str),
}

/// The CODEOWNERS file as git answered.
#[derive(Debug)]
enum CodeownersRead {
    Absent,
    /// Read (or not: `parsed` says why), not yet resolved.
    Unread {
        path: &'static str,
        parsed: Result<Codeowners, CodeownersError>,
        git_error: Option<ForgeError>,
        /// Out of budget or a git failure.
        transient: bool,
    },
}

/// Plan 18 S10: the database half of [`ReviewGit::facts`] on `reads` (the
/// pushers of the range, the CODEOWNERS owners and their match, the
/// selectors' units) - no git process.
pub(crate) async fn facts_from(
    db: &Db,
    asker: Asker<'_>,
    mr: &StoredMergeRequest,
    reads: GitReads,
    budget: &Budget,
) -> Result<GitFacts, ForgeError> {
    let GitReads {
        tip,
        changed,
        range,
        codeowners,
        ..
    } = reads;
    let committers = committers_from(db, asker, mr, range, budget).await?;
    let codeowners = codeowners_from(db, asker, mr, (&tip, &changed), codeowners, budget).await?;
    let targets = selector_targets(db, mr).await?;
    Ok(GitFacts {
        committers,
        changed,
        codeowners,
        targets,
    })
}

/// The committers of the range: its commits' pushers, recorded at receive
/// time (M3 R5 round 4), bounded by the budget's time.
async fn committers_from(
    db: &Db,
    asker: Asker<'_>,
    mr: &StoredMergeRequest,
    range: RangeRead,
    budget: &Budget,
) -> Result<Committers, ForgeError> {
    let commits = match range {
        RangeRead::Commits(commits) => commits,
        RangeRead::Unknown(reason) => return Ok(Committers::Unknown(reason)),
    };
    let read = review::range_pushers(db, asker.project, asker.repo, &commits);
    let pushers = match tokio::time::timeout(budget.left(), read).await {
        Ok(pushers) => pushers?,
        Err(_) => {
            spent(asker, mr, "range_pushers", "deadline");
            return Ok(Committers::Unknown("timeout"));
        }
    };
    Ok(committers::committers_of(&commits, &pushers))
}

/// The target tip's CODEOWNERS, resolved and matched against the changed
/// paths (in a blocking task within [`MATCH_BUDGET`]). One warn
/// `forge.review.codeowners_invalid` (FG-REVIEW-0014) when it cannot be
/// used, also for a git failure (its code rides along).
async fn codeowners_from(
    db: &Db,
    asker: Asker<'_>,
    mr: &StoredMergeRequest,
    (tip, changed): (&ObjectId, &ChangedPathsFact),
    read: CodeownersRead,
    budget: &Budget,
) -> Result<CodeownersFact, ForgeError> {
    let (path, parsed, git_error) = match read {
        CodeownersRead::Absent => return Ok(CodeownersFact::Absent),
        CodeownersRead::Unread {
            path,
            parsed,
            git_error,
            ..
        } => (path, parsed, git_error),
    };
    let mut join_error: Option<String> = None;
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

/// The units the project's approval rules' selectors name for `mr`'s
/// author now (ADR-0038 §3.1): nothing is read when no rule has a
/// selector. Candidates are people with Developer and above, never the
/// author (`escalate: parent` when none is in the base unit).
pub(crate) async fn selector_targets(
    db: &Db,
    mr: &StoredMergeRequest,
) -> Result<SelectorTargets, ForgeError> {
    let rules = review::approval_rules(db, mr.project_id).await?;
    let mut selectors: Vec<forge_core::approver::ApproverSelector> = Vec::new();
    for s in rules.iter().flat_map(|r| r.selectors.iter()) {
        if !selectors.contains(s) {
            selectors.push(s.clone()); // clone: the resolution owns its selectors
        }
    }
    if selectors.is_empty() {
        return Ok(SelectorTargets::default());
    }
    let resolution = forge_db::org_master::resolve_selectors(
        db,
        mr.project_id,
        Some(mr.author),
        &selectors,
        forge_core::Role::Developer,
        &[mr.author],
    )
    .await?;
    Ok(SelectorTargets(
        resolution
            .selectors
            .into_iter()
            .map(|r| (r.selector, r.target))
            .collect(),
    ))
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
