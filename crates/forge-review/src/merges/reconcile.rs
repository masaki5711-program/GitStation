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
        for listed in &stuck {
            match self.reconcile_one(listed).await {
                Ok(Settled::Applied) => report.applied += 1,
                Ok(Settled::Superseded) => report.superseded += 1,
                Ok(Settled::Failed) => report.failed += 1,
                Ok(Settled::Skipped) => report.skipped += 1,
                Err(e) => {
                    report.skipped += 1;
                    telemetry::log_failure(
                        &e.with_context("merge_id", listed.id)
                            .with_context("operation", "merge_reconcile"),
                        request_id,
                    );
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

    async fn reconcile_one(&self, listed: &StoredMerge) -> Result<Settled, ForgeError> {
        let Some(subject) = self.subject(listed).await? else {
            // The merge request is gone (purged with its attempts): nothing to settle.
            return Ok(Settled::Skipped);
        };
        let lock = match merges::lock_merges(&self.db, subject.repo, LOCK_WAIT).await {
            Ok(lock) => lock,
            Err(e) if e.code() == ErrorCode::DbQueryFailed => return Ok(Settled::Skipped),
            Err(e) => return Err(e),
        };
        let settled = self.reconcile_locked(listed.id, &subject).await;
        lock.release().await;
        if matches!(settled, Ok(Settled::Applied)) {
            // The job stopped before the source deletion (ADR-0024 §4.2
            // step 10 comes after the record): do it now.
            if let Some(done) = merges::merge_attempt(&self.db, listed.id)
                .await?
                .filter(wants_source_deletion)
            {
                self.delete_source(&done, &subject).await;
            }
        }
        settled
    }

    async fn reconcile_locked(
        &self,
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
            MergeState::Executing => self.ask_git(&attempt, subject).await,
            MergeState::Applied | MergeState::Superseded | MergeState::Failed => {
                Ok(Settled::Skipped)
            }
        }
    }

    /// Settle an `executing` attempt by asking git (module docs).
    async fn ask_git(
        &self,
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
                self.timeouts.read,
            )
            .await?;
        let landed = match &tip {
            Some(t) if t == new => true,
            Some(t) if t == old => false,
            Some(t) => {
                self.git
                    .is_ancestor(&self.storage, subject.repo, new, t, self.timeouts.read)
                    .await?
            }
            None => false,
        };
        if landed {
            self.record(attempt, subject, &[]).await?;
            return Ok(Settled::Applied);
        }
        if tip.as_ref() == Some(old) {
            merges::end_merge(
                &self.db,
                attempt.id,
                &[MergeState::Executing],
                MergeState::Superseded,
                "not_applied",
                None,
            )
            .await?;
            return Ok(Settled::Superseded);
        }
        let e = execution_failed("ref_state_unknown", attempt, subject.repo).with_context(
            "actual",
            tip.as_ref().map_or("", ObjectId::as_str).to_owned(),
        );
        merges::end_merge(
            &self.db,
            attempt.id,
            &[MergeState::Executing],
            MergeState::Failed,
            "ref_state_unknown",
            Some(ErrorCode::MergeExecutionFailed),
        )
        .await?;
        telemetry::log_failure(&e, &attempt.request_id);
        Ok(Settled::Failed)
    }
}

/// How one attempt was settled.
enum Settled {
    Applied,
    Superseded,
    Failed,
    Skipped,
}
