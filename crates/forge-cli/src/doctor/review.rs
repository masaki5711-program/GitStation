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
