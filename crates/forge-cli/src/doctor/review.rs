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
