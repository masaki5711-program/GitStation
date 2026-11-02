//! The reconciliation (ADR-0024 §4.2 照合): `forge serve` runs
//! [`Merger::reconcile`] every minute. Each attempt it looks at is settled
//! under its repository's merge lock (so no job is mid-run on it), after
//! reading the attempt again:
//!
//! - `queued` / `prepared` idle past `idle` → `superseded`
//!   (`reason=stale`): it never passed the fence, so it never moved a ref.
//! - `executing` past `grace + update_ref` after `executing_at` → ask git:
//!   the target at `new_sha` or a descendant of it → tx2 (`applied`, the
//!   events deduplicated by the merge id); still at `expected_old` →
//!   `superseded` (`reason=not_applied`); anything else → `failed`
//!   (`FG-REVIEW-0009 reason=ref_state_unknown`, alert: a person decides,
//!   runbook `merge-execution`).
//!
//! A busy lock skips the attempt for this run; an error is logged once per
//! attempt and the others go on.

use std::time::Duration;

use forge_core::{telemetry, MergeState, ObjectId, RequestId};
use forge_db::review::merges::{self, StoredMerge};
use forge_errors::{ErrorCode, ForgeError};

use super::execute::{execution_failed, wants_source_deletion, RECONCILE_BATCH};
use super::Merger;

/// The wait for a repository's lock during a reconciliation (a busy one is
/// skipped until the next run).
const LOCK_WAIT: Duration = Duration::from_secs(2);
/// The most one pass works (the rest keep their place for the next pass;
/// `forge serve` bounds the pass with its own timeout too).
pub const PASS_BUDGET: Duration = Duration::from_secs(45);
/// The most one attempt may take inside a pass (the lock wait, two git
/// reads and the record): past it the attempt is logged with its merge id
/// (FG-SYS-0005 `reconcile_attempt_timeout`) and goes to the back of the
/// next scan (review round 2).
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);

/// What one run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub looked: usize,
    pub applied: usize,
    pub superseded: usize,
    pub failed: usize,
    /// Busy, already settled, or an error (logged).
    pub skipped: usize,
}

impl Merger {
    /// One reconciliation run (module docs). `request_id` is the run's own.
    pub async fn reconcile(&self, request_id: &RequestId) -> Result<ReconcileReport, ForgeError> {
        let stuck = merges::stuck_merges(
            &self.db,
            self.timeouts.idle,
            self.timeouts.executing_after(),
            RECONCILE_BATCH,
        )
        .await?;
        let mut report = ReconcileReport {
            looked: stuck.len(),
            ..ReconcileReport::default()
        };
        let deadline = tokio::time::Instant::now() + PASS_BUDGET;
        for listed in &stuck {
            if tokio::time::Instant::now() >= deadline {
                // The rest waits for the next pass (they keep their place).
                report.skipped += 1;
                continue;
            }
            // Review round 2: one slow attempt never uses up the pass.
            let one = tokio::time::timeout(ATTEMPT_TIMEOUT, self.reconcile_one(listed)).await;
            let one = one.unwrap_or_else(|_| {
                Err(ForgeError::new(ErrorCode::ServeFailed)
                    .with_context("reason", "reconcile_attempt_timeout")
                    .with_context("timeout_ms", ATTEMPT_TIMEOUT.as_millis()))
            });
            match one {
                Ok(Settled::Applied) => report.applied += 1,
                Ok(Settled::Superseded) => report.superseded += 1,
                Ok(Settled::Failed) => report.failed += 1,
                Ok(Settled::Skipped) => report.skipped += 1,
                Ok(Settled::Busy) => {
                    report.skipped += 1;
                    self.seen(listed, request_id).await;
                }
                Err(e) => {
                    // Logged once here, with its code; the attempt goes to the
                    // back of the next scan (no head-of-line blocking).
                    report.skipped += 1;
                    telemetry::log_failure(
                        &e.with_context("merge_id", listed.id)
                            .with_context("operation", "merge_reconcile"),
                        request_id,
                    );
                    self.seen(listed, request_id).await;
                }
            }
        }

        if report.looked > 0 {
            tracing::info!(
                target: "forge_review",
                event = "forge.review.merge_reconciled",
                request_id = %request_id,
                looked = report.looked,
                applied = report.applied,
                superseded = report.superseded,
                failed = report.failed,
                skipped = report.skipped,
            );
        }
        Ok(report)
    }

    /// Move `listed` to the back of the next scan (a failure to do so is
    /// logged: the attempt only keeps its place).
    async fn seen(&self, listed: &StoredMerge, request_id: &RequestId) {
        if let Err(e) = merges::mark_reconcile_seen(&self.db, listed.id).await {
            telemetry::log_failure(
                &e.with_context("merge_id", listed.id)
                    .with_context("operation", "merge_reconcile_seen"),
                request_id,
            );
        }
    }

    async fn reconcile_one(&self, listed: &StoredMerge) -> Result<Settled, ForgeError> {
        let Some(subject) = self.subject(listed).await? else {
            // The merge request is gone (purged with its attempts): nothing to settle.
            return Ok(Settled::Skipped);
        };
        let lock = match merges::lock_merges(&self.db, subject.repo, LOCK_WAIT).await {
            Ok(lock) => lock,
            // Another merge of the repository runs: the only skip (review
            // round 1); every other failure is returned and logged.
            Err(e) if merges::is_lock_busy(&e) => return Ok(Settled::Busy),
            Err(e) => return Err(e),
        };
        let settled = self.reconcile_locked(&lock, listed.id, &subject).await;
        lock.release().await;
        if matches!(settled, Ok(Settled::Applied)) {
            // The job stopped before the source deletion (ADR-0024 §4.2
            // step 10 comes after the record): do it now. A failure to read
            // the attempt keeps the merge applied and is logged.
            match merges::merge_attempt(&self.db, listed.id).await {
                Ok(Some(done)) if wants_source_deletion(&done) => {
                    self.delete_source(&done, &subject).await;
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    target: "forge_review",
                    event = "forge.review.merge_source_unchecked",
                    error_code = e.code().id(),
                    request_id = %listed.request_id,
                    merge_id = listed.id,
                    error = %e,
                    "the merge is recorded; its source deletion is left to the job's retry"
                ),
            }
        }
        settled
    }

    async fn reconcile_locked(
        &self,
        lock: &merges::MergeLock,
        id: i64,
        subject: &super::execute::Subject,
    ) -> Result<Settled, ForgeError> {
        let Some(attempt) = merges::merge_attempt(&self.db, id).await? else {
            return Ok(Settled::Skipped);
        };
        match attempt.state {
            MergeState::Queued | MergeState::Prepared => {
                let ended = merges::end_merge(
                    &self.db,
                    id,
                    &[attempt.state],
                    MergeState::Superseded,
                    "stale",
                    None,
                )
                .await?;
                Ok(if ended {
                    Settled::Superseded
                } else {
                    Settled::Skipped
                })
            }
            MergeState::Executing => self.ask_git(lock, &attempt, subject).await,
            MergeState::Applied | MergeState::Superseded | MergeState::Failed => {
                Ok(Settled::Skipped)
            }
        }
    }

    /// Settle an `executing` attempt by asking git (module docs).
    async fn ask_git(
        &self,
        lock: &merges::MergeLock,
        attempt: &StoredMerge,
        subject: &super::execute::Subject,
    ) -> Result<Settled, ForgeError> {
        let (Some(old), Some(new)) = (&attempt.expected_old, &attempt.new_sha) else {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "merge_attempt_unprepared")
                .with_context("merge_id", attempt.id));
        };
        let tip: Option<ObjectId> = self
            .git
            .read_ref(
                &self.storage,
                subject.repo,
                &attempt.target_branch,
                self.bounded(self.timeouts.read, lock)?,
            )
            .await?;
        let landed = match &tip {
            Some(t) if t == new => true,
            Some(t) if t == old => false,
            Some(t) => {
                self.git
                    .is_ancestor(
                        &self.storage,
                        subject.repo,
                        new,
                        t,
                        self.bounded(self.timeouts.read, lock)?,
                    )
                    .await?
            }
            None => false,
        };
        if landed {
            // The ref moved under a run that stopped: drop the browse caches.
            self.invalidate(subject.repo);
            self.record(lock, attempt, subject, &[]).await?;
            return Ok(Settled::Applied);
        }
        if tip.as_ref() == Some(old) {
            let ended = merges::end_merge(
                &self.db,
                attempt.id,
                &[MergeState::Executing],
                MergeState::Superseded,
                "not_applied",
                None,
            )
            .await?;
            return Ok(if ended {
                Settled::Superseded
            } else {
                Settled::Skipped
            });
        }
        let e = execution_failed("ref_state_unknown", attempt, subject.repo).with_context(
            "actual",
            tip.as_ref().map_or("", ObjectId::as_str).to_owned(),
        );
        let ended = merges::end_merge(
            &self.db,
            attempt.id,
            &[MergeState::Executing],
            MergeState::Failed,
            "ref_state_unknown",
            Some(ErrorCode::MergeExecutionFailed),
        )
        .await?;
        if !ended {
            // Someone else settled it first: no alert for this one.
            return Ok(Settled::Skipped);
        }
        telemetry::log_failure(&e, &attempt.request_id);
        Ok(Settled::Failed)
    }
}

/// How one attempt was settled.
enum Settled {
    Applied,
    Superseded,
    Failed,
    /// Nothing to do (settled meanwhile, or gone).
    Skipped,
    /// Its repository's merge lock was busy: next pass.
    Busy,
}
