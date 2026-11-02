//! The `mergeability_sweep` task of `forge worker` (M3 R2, ADR-0024 §3,
//! review): every 30 s it
//!
//! 1. ends mergeability rows left `checking` longer than the job's time
//!    limit with no live job (`forge_db::review::reap_stale_checks`:
//!    `check_failed reason=job_stale`, each logged with FG-REVIEW-0003) — a
//!    job that timed out and died, or a worker that crashed mid-run, never
//!    leaves a row `checking` for ever;
//! 2. re-requests open merge requests that are `unchecked` with no live job
//!    (`forge_db::review::sweep_unchecked`, at most
//!    [`forge_db::review::SWEEP_BATCH`] per run, the longest waiting first):
//!    the ones a target push deferred past `review.mergeability_fanout_max`,
//!    or whose request's job died. A full batch runs again at once.
//!
//! A failed run (the database) is returned and logged once per failure
//! streak by the supervisor with `error_code` and `task=mergeability_sweep`.

use forge_core::RequestId;
use forge_db::review::{reap_stale_checks, sweep_unchecked, STALE_AFTER, SWEEP_BATCH};
use forge_db::Db;
use forge_errors::ForgeError;

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "mergeability_sweep";
/// The task's period.
pub(crate) const PERIOD: std::time::Duration = std::time::Duration::from_secs(30);
/// Stale rows ended per run at most.
const REAP_BATCH: i64 = 100;

/// One run (module docs).
pub(crate) async fn run_mergeability_sweep(db: &Db) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let reaped = reap_stale_checks(db, STALE_AFTER, REAP_BATCH).await?;
    let swept = sweep_unchecked(db, SWEEP_BATCH, &request_id).await?;
    if !swept.is_empty() {
        tracing::info!(
            target: "forge_cli",
            task = NAME,
            request_id = %request_id,
            requested = swept.len(),
            "forge.worker.mergeability_swept"
        );
    }
    let full = |n: usize, max: i64| i64::try_from(n).unwrap_or(i64::MAX) >= max;
    Ok(
        if full(swept.len(), SWEEP_BATCH) || full(reaped.len(), REAP_BATCH) {
            Next::Immediately
        } else {
            Next::AfterPeriod
        },
    )
}
