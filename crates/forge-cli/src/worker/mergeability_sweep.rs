//! The `mergeability_sweep` task of `forge worker` (M3 R2, ADR-0024 §3,
//! review): every 30 s it
//!
//! 1. ends mergeability rows of open merge requests left `checking` longer
//!    than the job's time limit with no live job
//!    (`forge_db::review::reap_stale_checks`: `check_failed
//!    reason=job_stale`, each logged with FG-REVIEW-0003);
//! 2. re-requests open merge requests that are `unchecked` with no live job
//!    (`forge_db::review::sweep_unchecked`, at most
//!    [`forge_db::review::SWEEP_BATCH`] per run, the longest waiting
//!    first): deferred past `review.mergeability_fanout_max`, or whose
//!    request's job died;
//! 3. verifies decided rows (re-review): at most
//!    [`forge_db::review::SWEEP_BATCH`] per run, the least recently
//!    verified first (`decided_candidates`), it reads each target's live tip
//!    with git; a pair that moved (a dead-lettered push event, a target push
//!    past the deferral bound, a head the version job never downgraded) is
//!    re-requested, a live one is marked verified (`confirm_live`). A git
//!    failure on one row skips it (logged at warn with its code; the next
//!    run tries again) — it never counts as "still live".
//!
//! A full batch runs again at once. A failed run (the database) is returned
//! and logged once per failure streak by the supervisor with `error_code`
//! and `task=mergeability_sweep`.

use std::time::Duration;

use forge_core::RequestId;
use forge_db::review::{
    confirm_live, decided_candidates, reap_stale_checks, request_check, sweep_unchecked,
    STALE_AFTER, SWEEP_BATCH,
};
use forge_db::Db;
use forge_errors::ForgeError;

use super::provision::Storage;
use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "mergeability_sweep";
/// The task's period.
pub(crate) const PERIOD: Duration = Duration::from_secs(30);
/// Stale rows ended per run at most.
const REAP_BATCH: i64 = 100;
/// One target tip read.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// One run (module docs). Without `storage` (the loop tests) step 3 is
/// skipped.
pub(crate) async fn run_mergeability_sweep(
    db: &Db,
    storage: Option<&Storage>,
) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let reaped = reap_stale_checks(db, STALE_AFTER, REAP_BATCH).await?;
    let swept = sweep_unchecked(db, SWEEP_BATCH, &request_id).await?;
    let (verified, moved) = match storage {
        Some(storage) => verify_decided(db, storage, &request_id).await?,
        None => (0, 0),
    };
    if !swept.is_empty() || moved > 0 {
        tracing::info!(
            target: "forge_cli",
            task = NAME,
            request_id = %request_id,
            requested = swept.len(),
            moved,
            "forge.worker.mergeability_swept"
        );
    }
    let full = |n: usize, max: i64| i64::try_from(n).unwrap_or(i64::MAX) >= max;
    Ok(
        if full(swept.len(), SWEEP_BATCH)
            || full(reaped.len(), REAP_BATCH)
            || full(verified, SWEEP_BATCH)
        {
            Next::Immediately
        } else {
            Next::AfterPeriod
        },
    )
}

/// Step 3: `(rows visited, rows re-requested)`.
async fn verify_decided(
    db: &Db,
    storage: &Storage,
    request_id: &RequestId,
) -> Result<(usize, usize), ForgeError> {
    let rows = decided_candidates(db, SWEEP_BATCH).await?;
    let mut live = Vec::new();
    let mut moved = 0usize;
    for row in &rows {
        let tip = match storage
            .git
            .read_ref(
                &storage.root,
                row.repository,
                &row.target_branch,
                READ_TIMEOUT,
            )
            .await
        {
            Ok(tip) => tip,
            Err(e) => {
                tracing::warn!(
                    target: "forge_cli",
                    task = NAME,
                    event = "forge.worker.mergeability_verify_skipped",
                    request_id = %request_id,
                    error_code = e.code().id(),
                    merge_request_id = row.merge_request.get(),
                    error = %e,
                    "could not read the target tip; verified again next run"
                );
                continue;
            }
        };
        if row.is_live(tip.as_ref()) {
            live.push(row.merge_request);
        } else {
            // clone: each job keeps the sweep's request id
            request_check(db, row.merge_request, request_id.clone()).await?;
            moved += 1;
        }
    }
    confirm_live(db, &live).await?;
    Ok((rows.len(), moved))
}
