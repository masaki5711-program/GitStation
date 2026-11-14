//! The mergeability check of `forge doctor` (M3 R2, ADR-0024 §3,
//! `FG-REVIEW-0003`, runbook merge-request-mergeability).

use forge_db::review::mergeability_health;
use forge_db::Db;

use super::{fail, ok, skip, Check};

const NAME: &str = "review.mergeability";

/// `review.mergeability`: no row `checking` past the job's time limit with
/// no live job, and no open merge request `unchecked` that long with no
/// live job (the worker's sweep ends / re-requests both; a count here means
/// no worker is sweeping, or it fails).
pub(super) async fn mergeability_check(db: &Db) -> Check {
    match mergeability_health(db).await {
        Ok(Some(h)) => match h.check() {
            Ok(()) => ok(NAME, "no check stuck or waiting without a job"),
            Err(e) => fail(NAME, e),
        },
        Ok(None) => skip(NAME, "schema not migrated"),
        Err(e) => fail(NAME, e),
    }
}

const MERGES: &str = "review.merges";

/// How much later than its bound an attempt counts as overdue: two
/// reconciliation periods of `forge serve` (one may be in progress).
const MERGE_MARGIN: std::time::Duration = std::time::Duration::from_secs(180);

/// `review.merges` (M3 R4 review round 1, FG-REVIEW-0009, runbook
/// merge-execution): no live merge attempt past the reconciliation's bound
/// (a count means `forge serve`'s reconciliation is not running or fails
/// every pass), and no `failed` attempt of the last 24 hours waiting for a
/// person. The bounds are serve's own: `MergeTimeouts::default()` is the
/// only source (R4 has no configuration key for them; a key added later
/// must feed both serve and this check).
pub(super) async fn merges_check(db: &Db) -> Check {
    let t = forge_review::MergeTimeouts::default();
    match forge_db::review::merges::health(db, t.idle, t.executing_after(), MERGE_MARGIN).await {
        Ok(Some(h)) => match h.check() {
            Ok(()) => ok(MERGES, "no merge attempt overdue or failed"),
            Err(e) => fail(MERGES, e),
        },
        Ok(None) => skip(MERGES, "schema not migrated"),
        Err(e) => fail(MERGES, e),
    }
}

const QUEUE: &str = "review.merge_queue";

/// A queue's lease expired longer ago than this: its worker is gone (a
/// running worker takes an expired lease over within a few seconds).
const LEASE_GRACE: std::time::Duration = std::time::Duration::from_secs(600);

/// The line of `review.merge_queue` from its facts.
fn queue_line(h: &forge_db::review::merge_queue::QueueHealth) -> Check {
    use forge_errors::{ErrorCode, ForgeError};
    if h.stalled_batches > 0 {
        let ids: Vec<String> = h.stalled_ids.iter().map(i64::to_string).collect();
        return fail(
            QUEUE,
            ForgeError::new(ErrorCode::MergeQueueStalled)
                .with_context("stalled_batches", h.stalled_batches)
                .with_context("batch_ids", ids.join(",")),
        );
    }
    if h.orphaned_queues > 0 {
        let ids: Vec<String> = h.orphaned_ids.iter().map(i64::to_string).collect();
        return fail(
            QUEUE,
            ForgeError::new(ErrorCode::MergeQueueStalled)
                .with_context("reason", "worker_not_running")
                .with_context("queues", h.orphaned_queues)
                .with_context("queue_ids", ids.join(",")),
        );
    }
    ok(
        QUEUE,
        "no merge queue batch past its deadline, every queue worked",
    )
}

/// `review.merge_queue` (M4 C8, FG-CI-0024, runbook merge-queue): no live
/// batch past its deadline (the worker reaps it at its next step: a count
/// means no worker runs), and no queue whose lease expired long ago.
pub(super) async fn merge_queue_check(db: &Db) -> Check {
    match forge_db::review::merge_queue::health(db, LEASE_GRACE).await {
        Ok(h) => queue_line(&h),
        Err(e) => fail(QUEUE, e),
    }
}

#[cfg(test)]
mod queue_tests {
    use super::super::Outcome;
    use super::*;
    use forge_db::review::merge_queue::QueueHealth;

    #[test]
    fn a_stalled_batch_or_an_orphaned_queue_fails_with_the_stall_code() {
        assert!(matches!(
            queue_line(&QueueHealth::default()).outcome,
            Outcome::Ok(_)
        ));
        for h in [
            QueueHealth {
                stalled_batches: 1,
                stalled_ids: vec![4],
                ..QueueHealth::default()
            },
            QueueHealth {
                orphaned_queues: 2,
                orphaned_ids: vec![2, 1],
                ..QueueHealth::default()
            },
        ] {
            let Outcome::Fail(e) = queue_line(&h).outcome else {
                panic!("not a failure: {h:?}");
            };
            assert_eq!(e.code(), forge_errors::ErrorCode::MergeQueueStalled);
        }
    }
}
