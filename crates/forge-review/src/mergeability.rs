//! Whether a merge request can be merged (M3 R2, ADR-0024 §3): the body of
//! the job `merge_request.mergeability` (`forge worker` runs it).
//!
//! 1. [`forge_db::review::begin_check`]: a run whose `check_seq` is no
//!    longer the latest stops (`superseded`, nothing written — the newer
//!    job decides); a closed or merged merge request is not checked;
//!    otherwise the status is `checking`.
//! 2. With forge-git, against the merge request's RECORDED head (the last
//!    version, also when the source branch was deleted since — its hidden
//!    ref keeps it) and the target branch's current tip:
//!    - no version yet → `check_failed reason=no_version`;
//!    - the target branch is gone → `check_failed reason=target_missing`;
//!    - no common history (`merge-base` exit 1) → `check_failed
//!      reason=unrelated_histories` (never "mergeable");
//!    - `merge-tree --write-tree`: clean → `can_be_merged`, conflict (or an
//!      output past its cap) → `cannot_be_merged reason=conflict` with the
//!      paths (at most [`MAX_STORED_FILES`] and [`MAX_STORED_BYTES`];
//!      `conflicts_truncated` beyond).
//! 3. [`forge_db::review::record_check`] stores it while the seq is still
//!    the latest; a can ⇄ cannot flip writes `merge_request.conflict_*`.
//!
//! A git failure (`FG-GIT-0002` timeout, `0004` busy, `0003` process) is
//! stored as `check_failed` with `reason=git_timeout|git_busy|git_failed`
//! and returned: the worker logs it once with its code and retries with
//! backoff (never "mergeable" meanwhile). Success is logged once here
//! (`forge.review.mergeability_checked`), with the conflicted paths — the
//! only place they appear besides the status.

use std::time::{Duration, Instant};

use forge_core::{MergeabilityStatus, ObjectId, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::review::{self, CheckResult, CheckStart, MergeabilityRequest, Recorded};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::mergeability::{MergeTree, MergeTreeLimits};
use forge_git::{Git, StorageRoot};

/// Conflicted paths stored at most (the rest only in the log line).
pub const MAX_STORED_FILES: usize = 100;
/// Bytes of stored paths at most (one path is cut to fit, never stored
/// half; the table's CHECK allows 64 KiB).
pub const MAX_STORED_BYTES: usize = 16 * 1024;
/// The ref read and `merge-base` of a check.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The audit label of the job's flips.
const ACTOR_LABEL: &str = "forge-worker";

/// Where the check runs: git, the storage, the trial merge's limits.
#[derive(Clone)]
pub struct Mergeability {
    git: Git,
    storage: StorageRoot,
    limits: MergeTreeLimits,
}

impl std::fmt::Debug for Mergeability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mergeability")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// What a run did (the job's log line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// `checked`, `superseded`, `not_open` or `gone`.
    pub outcome: &'static str,
    pub status: Option<MergeabilityStatus>,
    pub reason: Option<&'static str>,
    pub flip: Option<forge_core::EventType>,
}

impl CheckReport {
    fn skipped(outcome: &'static str) -> Self {
        Self {
            outcome,
            status: None,
            reason: None,
            flip: None,
        }
    }
}

/// The stored paths of a conflict: at most [`MAX_STORED_FILES`] and
/// [`MAX_STORED_BYTES`]; `true` when some were left out.
pub fn stored_files(files: &[String]) -> (Vec<String>, bool) {
    let mut kept = Vec::new();
    let mut bytes = 0usize;
    for f in files {
        if kept.len() >= MAX_STORED_FILES || bytes + f.len() > MAX_STORED_BYTES {
            return (kept, true);
        }
        bytes += f.len();
        kept.push(f.clone()); // clone: the stored list owns its paths
    }
    (kept, false)
}

/// The `reason` a git failure is stored with.
fn git_reason(e: &ForgeError) -> &'static str {
    let code = e.code();
    if code == ErrorCode::GitTimeout {
        "git_timeout"
    } else if code == ErrorCode::GitBusy {
        "git_busy"
    } else {
        "git_failed"
    }
}

/// What the git part decided, or the git failure.
enum Computed {
    Done(CheckResult),
    Failed(CheckResult, ForgeError),
}

fn failed(reason: &'static str, head: Option<&ObjectId>, target: Option<&ObjectId>) -> CheckResult {
    CheckResult {
        status: MergeabilityStatus::CheckFailed,
        reason: Some(reason),
        head: head.cloned(),     // clone: the result owns its ids
        target: target.cloned(), // clone: as above
        conflicts: Vec::new(),
        conflicts_truncated: false,
    }
}

impl Mergeability {
    pub fn new(git: Git, storage: StorageRoot) -> Self {
        Self {
            git,
            storage,
            limits: MergeTreeLimits::default(),
        }
    }

    /// Other trial-merge limits (tests).
    #[must_use]
    pub fn with_limits(mut self, limits: MergeTreeLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The git part (module docs, step 2) for `head` against `target`.
    async fn compute(
        &self,
        repo: forge_core::RepoId,
        head: Option<&ObjectId>,
        target_branch: &forge_core::RefName,
        all_files: &mut Vec<String>,
    ) -> Computed {
        let Some(head) = head else {
            return Computed::Done(failed("no_version", None, None));
        };
        let tip = match self
            .git
            .read_ref(&self.storage, repo, target_branch, READ_TIMEOUT)
            .await
        {
            Ok(tip) => tip,
            Err(e) => return Computed::Failed(failed(git_reason(&e), Some(head), None), e),
        };
        let Some(tip) = tip else {
            return Computed::Done(failed("target_missing", Some(head), None));
        };
        match self
            .git
            .merge_base(&self.storage, repo, &tip, head, READ_TIMEOUT)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Computed::Done(failed("unrelated_histories", Some(head), Some(&tip)));
            }
            Err(e) => {
                return Computed::Failed(failed(git_reason(&e), Some(head), Some(&tip)), e);
            }
        }
        match self
            .git
            .merge_tree(&self.storage, repo, &tip, head, self.limits)
            .await
        {
            Ok(MergeTree::Clean { .. }) => Computed::Done(CheckResult {
                status: MergeabilityStatus::CanBeMerged,
                reason: None,
                head: Some(head.clone()), // clone: the result owns its ids
                target: Some(tip),
                conflicts: Vec::new(),
                conflicts_truncated: false,
            }),
            Ok(MergeTree::Conflicted {
                files,
                files_truncated,
            }) => {
                let (conflicts, cut) = stored_files(&files);
                *all_files = files;
                Computed::Done(CheckResult {
                    status: MergeabilityStatus::CannotBeMerged,
                    reason: Some("conflict"),
                    head: Some(head.clone()), // clone: as above
                    target: Some(tip),
                    conflicts,
                    conflicts_truncated: files_truncated || cut,
                })
            }
            Err(e) => Computed::Failed(failed(git_reason(&e), Some(head), Some(&tip)), e),
        }
    }
}

/// The mergeability job (module docs). Errors: forge-db's (`FG-DB-0001` /
/// `0002`), `FG-REPO-0016` for a repository not ready, and the git failure
/// after it was stored as `check_failed`; each is the job's failure (the
/// worker logs it once with the job's request id).
pub async fn check(
    db: &Db,
    checker: &Mergeability,
    request: MergeabilityRequest,
    request_id: &RequestId,
) -> Result<CheckReport, ForgeError> {
    let started = Instant::now();
    let mr = request.merge_request;
    let seq = request.check_seq;
    let subject = match review::begin_check(db, mr, seq).await? {
        CheckStart::Superseded { .. } => return Ok(CheckReport::skipped("superseded")),
        CheckStart::NotOpen(None) => return Ok(CheckReport::skipped("gone")),
        CheckStart::NotOpen(Some(_)) => return Ok(CheckReport::skipped("not_open")),
        CheckStart::Check(subject) => subject,
    };
    let Some(target) = review::refresh_target(db, mr).await? else {
        return Ok(CheckReport::skipped("gone"));
    };
    if !target.repository_ready {
        return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
            .with_context("repo_id", target.repository.get())
            .with_context("merge_request_id", mr.get()));
    }
    let repo = target.repository;
    let mut all_files = Vec::new();
    let computed = checker
        .compute(
            repo,
            subject.head.as_ref(),
            &subject.target_branch,
            &mut all_files,
        )
        .await;
    let (result, git_error) = match computed {
        Computed::Done(result) => (result, None),
        Computed::Failed(result, e) => (result, Some(e)),
    };
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    ctx.actor_label = Some(ACTOR_LABEL.to_owned());
    let recorded = review::record_check(db, &ctx, mr, seq, &result).await;
    let recorded = match (recorded, git_error) {
        (Ok(_), Some(e)) => {
            return Err(e
                .with_context("merge_request_id", mr.get())
                .with_context("check_seq", seq)
                .with_context("stored", "check_failed"));
        }
        (Err(db_error), Some(e)) => {
            return Err(db_error.with_context("git_error", e.code().id()));
        }
        (Err(db_error), None) => return Err(db_error),
        (Ok(recorded), None) => recorded,
    };
    let (outcome, flip) = match recorded {
        Recorded::Superseded => ("superseded", None),
        Recorded::Stored { flip } => ("checked", flip),
    };
    tracing::info!(
        target: "forge_review",
        event = "forge.review.mergeability_checked",
        request_id = %request_id,
        merge_request_id = mr.get(),
        project_id = subject.project.get(),
        iid = subject.iid,
        check_seq = seq,
        outcome,
        status = result.status.as_str(),
        reason = result.reason,
        head_sha = result.head.as_ref().map(ObjectId::as_str),
        target_sha = result.target.as_ref().map(ObjectId::as_str),
        conflict_files = all_files.len(),
        conflicts_truncated = result.conflicts_truncated,
        files = ?all_files,
        flip = flip.map(forge_core::EventType::as_str),
        duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "merge request mergeability checked"
    );
    Ok(CheckReport {
        outcome,
        status: Some(result.status),
        reason: result.reason,
        flip,
    })
}
