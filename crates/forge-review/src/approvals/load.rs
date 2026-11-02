//! The git (and push record) reads of one approval evaluation, under one
//! [`Budget`] (module docs of `approvals`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use forge_core::ObjectId;
use forge_db::review::{self, StoredMergeRequest};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::mergeability::{MergeTree, MergeTreeLimits};
use forge_git::review_inputs::{ChangedPaths, CodeownersFile};

use super::codeowners::{self, Codeowners, CodeownersError, Owner};
use super::committers::{self, Reach};
use super::{
    warn_unread, Asker, Budget, ChangedPathsFact, CodeownersFact, Committers, GitFacts, OwnerRef,
    ReviewGit, MATCH_BUDGET, MAX_CHANGED_PATHS,
};

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
            Ok(MergeTree::Conflicted { .. }) => return ChangedPathsFact::Unknown("conflict"),
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
            Ok(ChangedPaths::Complete(paths)) => ChangedPathsFact::Known(paths),
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
        let in_range: BTreeSet<&ObjectId> = range.commits.iter().collect();
        let need_sets = !committers::unnamed(&range.commits, &coverage).is_empty();
        let mut reach = BTreeMap::new();
        let mut reaching = 0;
        for index in committers::cut_entries(&coverage) {
            let Some(new) = coverage.entries.get(index).and_then(|e| e.new.as_ref()) else {
                continue;
            };
            let answer = self
                .reaches(asker, mr, new, &range.bottoms, &in_range, budget)
                .await;
            if matches!(answer, Reach::Yes(_)) {
                reaching += 1;
                if reaching > committers::MAX_PARTIAL_CHECKS {
                    return Ok(Committers::Unknown("too_many_partial_pushes"));
                }
            }
            let answer = if need_sets && answer == Reach::Yes(None) {
                self.reached(asker, mr, new, tip, &in_range, budget).await
            } else {
                answer
            };
            reach.insert(index, answer);
        }
        Ok(committers::committers_of(&range.commits, &coverage, &reach))
    }

    /// Whether `new` reaches the range: it is in it, or a bottom of the
    /// range is its ancestor.
    async fn reaches(
        &self,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        new: &ObjectId,
        bottoms: &[ObjectId],
        in_range: &BTreeSet<&ObjectId>,
        budget: &mut Budget,
    ) -> Reach {
        if in_range.contains(new) {
            return Reach::Yes(None);
        }
        for bottom in bottoms {
            let Ok(timeout) = budget.next() else {
                spent(asker, mr, "push_reach", "budget");
                return Reach::Unknown;
            };
            match self
                .git
                .is_ancestor(&self.storage, asker.repo, bottom, new, timeout)
                .await
            {
                Ok(true) => return Reach::Yes(None),
                Ok(false) => {}
                Err(e) => {
                    warn_unread(asker, mr, "push_reach", &e);
                    return Reach::Unknown;
                }
            }
        }
        Reach::No
    }

    /// The range commits `new` reaches (`head`-side commits not on the
    /// target).
    async fn reached(
        &self,
        asker: Asker<'_>,
        mr: &StoredMergeRequest,
        new: &ObjectId,
        tip: &ObjectId,
        in_range: &BTreeSet<&ObjectId>,
        budget: &mut Budget,
    ) -> Reach {
        let Ok(timeout) = budget.next() else {
            spent(asker, mr, "push_reach", "budget");
            return Reach::Unknown;
        };
        match self
            .git
            .range_commits(
                &self.storage,
                asker.repo,
                Some(tip),
                new,
                committers::MAX_RANGE_COMMITS,
                timeout,
            )
            .await
        {
            Ok(Some(r)) => Reach::Yes(Some(
                r.commits
                    .into_iter()
                    .filter(|c| in_range.contains(c))
                    .collect(),
            )),
            Ok(None) => Reach::Unknown,
            Err(e) => {
                warn_unread(asker, mr, "push_reach", &e);
                Reach::Unknown
            }
        }
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
                    ChangedPathsFact::Unknown(_) => Vec::new(),
                };
                match matched(file.clone(), paths, budget.deadline()).await {
                    // clone: the task owns its copy
                    Some(matched) => CodeownersFact::Valid {
                        path,
                        file,
                        owners,
                        matched,
                    },
                    None => CodeownersFact::Invalid(CodeownersError::file("match_budget")),
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
/// a blocking task until the earlier of [`MATCH_BUDGET`] and `deadline`;
/// `None` past it (fail closed).
async fn matched(
    file: Codeowners,
    paths: Vec<String>,
    deadline: Instant,
) -> Option<BTreeMap<u32, usize>> {
    let deadline = deadline.min(Instant::now() + MATCH_BUDGET);
    tokio::task::spawn_blocking(move || codeowners::owned_lines(&file, &paths, Some(deadline)))
        .await
        .ok()
        .flatten()
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
