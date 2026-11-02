//! Whether a merge request can be merged (M3 R2, ADR-0024 §3): the body of
//! the job `merge_request.mergeability` (`forge worker` runs it), its
//! end-of-life path, and the version job's request after a version.
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
//!    the latest, the merge request open and its head the one checked; a
//!    can ⇄ cannot flip writes `merge_request.conflict_*`.
//!
//! A git failure (`FG-GIT-0002` timeout, `0004` busy, `0003` process) is
//! stored as `check_failed` with `reason=git_timeout|git_busy|git_failed`
//! and returned: the worker logs it once with its code and retries with
//! backoff. A run that fails for good ([`fail`]: a permanent failure or
//! the last attempt) ends its request as `check_failed reason=job_failed`
//! and logs `FG-REVIEW-0003`; a run that never returns (timeout, crash) is
//! ended by the worker's sweep (`reap_stale_checks`, `job_stale`). A
//! decision is logged once at info, an undecided result at warn
//! (`forge.review.mergeability_checked`, with the stored — capped — paths
//! and the full count).

use std::time::{Duration, Instant};

use forge_core::{CheckReason, MergeabilityStatus, ObjectId, RefName, RepoId, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::review::{self, CheckResult, CheckStart, MergeabilityRequest, Recorded};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::mergeability::{MergeTree, MergeTreeLimits};
use forge_git::{Git, StorageRoot};

use crate::versions::ACTOR_LABEL;

pub(crate) use crate::after_version::after_version;

/// Conflicted paths stored at most (the log line carries the count).
pub const MAX_STORED_FILES: usize = 100;
/// Bytes of stored paths at most (one path is cut to fit, never stored
/// half; the table's CHECK allows 64 KiB).
pub const MAX_STORED_BYTES: usize = 16 * 1024;
/// The ref read and `merge-base` of a check.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

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
    pub reason: Option<CheckReason>,
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

/// The reason a git failure is stored with.
pub fn git_reason(e: &ForgeError) -> CheckReason {
    let code = e.code();
    if code == ErrorCode::GitTimeout {
        CheckReason::GitTimeout
    } else if code == ErrorCode::GitBusy {
        CheckReason::GitBusy
    } else {
        CheckReason::GitFailed
    }
}

/// What the git part decided (with how many paths git listed in all), or
/// the git failure.
enum Computed {
    Done { result: CheckResult, files: usize },
    Failed(CheckResult, ForgeError),
}

fn failed(reason: CheckReason, head: Option<&ObjectId>, target: Option<&ObjectId>) -> CheckResult {
    CheckResult {
        status: MergeabilityStatus::CheckFailed,
        reason: Some(reason),
        head: head.cloned(),     // clone: the result owns its ids
        target: target.cloned(), // clone: as above
        conflicts: Vec::new(),
        conflicts_truncated: false,
    }
}

fn done(result: CheckResult) -> Computed {
    Computed::Done { result, files: 0 }
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
    async fn compute(&self, repo: RepoId, head: Option<&ObjectId>, target: &RefName) -> Computed {
        let Some(head) = head else {
            return done(failed(CheckReason::NoVersion, None, None));
        };
        let tip = match self
            .git
            .read_ref(&self.storage, repo, target, READ_TIMEOUT)
            .await
        {
            Ok(tip) => tip,
            Err(e) => return Computed::Failed(failed(git_reason(&e), Some(head), None), e),
        };
        let Some(tip) = tip else {
            return done(failed(CheckReason::TargetMissing, Some(head), None));
        };
        match self
            .git
            .merge_base(&self.storage, repo, &tip, head, READ_TIMEOUT)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                return done(failed(
                    CheckReason::UnrelatedHistories,
                    Some(head),
                    Some(&tip),
                ));
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
            Ok(MergeTree::Clean { .. }) => done(CheckResult {
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
                Computed::Done {
                    files: files.len(),
                    result: CheckResult {
                        status: MergeabilityStatus::CannotBeMerged,
                        reason: Some(CheckReason::Conflict),
                        head: Some(head.clone()), // clone: as above
                        target: Some(tip),
                        conflicts,
                        conflicts_truncated: files_truncated || cut,
                    },
                }
            }
            Err(e) => Computed::Failed(failed(git_reason(&e), Some(head), Some(&tip)), e),
        }
    }
}

pub(crate) fn system(request_id: &RequestId) -> ChangeContext {
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    ctx.actor_label = Some(ACTOR_LABEL.to_owned());
    ctx
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
    let computed = checker
        .compute(
            target.repository,
            subject.head.as_ref(),
            &subject.target_branch,
        )
        .await;
    let (result, files, git_error) = match computed {
        Computed::Done { result, files } => (result, files, None),
        Computed::Failed(result, e) => (result, 0, Some(e)),
    };
    let recorded = review::record_check(db, &system(request_id), mr, seq, &result).await;
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
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let reason = result.reason.map(CheckReason::as_str);
    let head_sha = result.head.as_ref().map(ObjectId::as_str);
    let target_sha = result.target.as_ref().map(ObjectId::as_str);
    if result.status.is_decided() || outcome == "superseded" {
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
            reason,
            head_sha,
            target_sha,
            conflict_files = files,
            conflicts_truncated = result.conflicts_truncated,
            files = ?result.conflicts,
            flip = flip.map(forge_core::EventType::as_str),
            duration_ms,
            "merge request mergeability checked"
        );
    } else {
        // Undecided (target missing, unrelated, no version): the merge is
        // blocked on it; say why at warn.
        tracing::warn!(
            target: "forge_review",
            event = "forge.review.mergeability_checked",
            request_id = %request_id,
            merge_request_id = mr.get(),
            project_id = subject.project.get(),
            iid = subject.iid,
            check_seq = seq,
            outcome,
            status = result.status.as_str(),
            reason,
            head_sha,
            target_sha,
            duration_ms,
            "merge request mergeability could not be decided"
        );
    }
    Ok(CheckReport {
        outcome,
        status: Some(result.status),
        reason: result.reason,
        flip,
    })
}

/// The job failed for good (`cause`: its last error): end its request as
/// `check_failed reason=job_failed` and log `FG-REVIEW-0003` once (the
/// worker logs `cause` itself). A request already superseded or decided is
/// left alone. A failure to write is logged here and not returned: the
/// sweep's reaper ends the row later (`job_stale`).
pub async fn fail(
    db: &Db,
    request: MergeabilityRequest,
    request_id: &RequestId,
    cause: &ForgeError,
) {
    let MergeabilityRequest {
        merge_request: mr,
        check_seq: seq,
    } = request;
    match review::fail_check(db, mr, seq).await {
        Ok(true) => {
            let e = review::not_checked("job_failed");
            tracing::warn!(
                target: "forge_review",
                event = "forge.review.mergeability_not_checked",
                error_code = e.code().id(),
                request_id = %request_id,
                reason = "job_failed",
                merge_request_id = mr.get(),
                check_seq = seq,
                cause_code = cause.code().id(),
                "the mergeability job failed for good; stored check_failed"
            );
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(
            target: "forge_review",
            event = "forge.review.mergeability_fail_unrecorded",
            error_code = e.code().id(),
            request_id = %request_id,
            merge_request_id = mr.get(),
            check_seq = seq,
            error = %e,
            "could not store job_failed; the sweep ends the row as job_stale"
        ),
    }
}

#[cfg(test)]
#[path = "mergeability_tests.rs"]
mod tests;
