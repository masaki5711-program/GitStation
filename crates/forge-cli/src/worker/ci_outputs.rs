//! The `ci_outputs` task of `forge worker` (M4 C5, plan 14 §5 C5,
//! ADR-0029 decision 7, plan 04 P-6 / P-7): every [`PERIOD`] it writes the
//! `ci/pipeline` check of each pipeline whose status changed
//! (`forge_pipelines::outputs::report_pipeline_checks`, as the system with
//! the label `forge-ci`), then fingerprints the failed jobs not
//! fingerprinted yet (`fingerprint_failures`). A full batch of either runs
//! again at once.
//!
//! A failed run (`FG-DB-0001` / `FG-DB-0002`, or one pipeline's write) is
//! returned and logged once per failure streak by the supervisor with
//! `error_code` and `task=ci_outputs`, then retried after the backoff. One
//! pipeline whose check cannot be written does not stop the others (each is
//! logged as `forge.ci.pipeline_check_failed`); the fingerprints still run
//! in that pass.

use std::time::Duration;

use forge_core::RequestId;
use forge_db::Db;
use forge_errors::ForgeError;
use forge_pipelines::outputs::{
    fingerprint_failures, report_pipeline_checks, CHECK_BATCH, FINGERPRINT_BATCH,
};

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "ci_outputs";
/// How often it runs (a pipeline's check follows its status within about
/// this much).
pub(crate) const PERIOD: Duration = Duration::from_secs(5);

/// One run: the checks, then the fingerprints.
pub(crate) async fn run_ci_outputs(db: &Db) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let checks = report_pipeline_checks(db, &request_id).await;
    let fingerprinted = fingerprint_failures(db, &request_id).await?;
    let checks = checks?;
    let full_checks = i64::try_from(checks.written + checks.superseded + checks.skipped)
        .unwrap_or(i64::MAX)
        >= CHECK_BATCH;
    let full_prints = i64::try_from(fingerprinted.len()).unwrap_or(i64::MAX) >= FINGERPRINT_BATCH;
    if full_checks || full_prints {
        return Ok(Next::Immediately);
    }
    Ok(Next::AfterPeriod)
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// A run with no pipeline waits for the period.
    #[tokio::test]
    async fn a_run_with_nothing_to_report_waits_for_the_period() {
        let t = FreshDb::migrated().await;
        assert_eq!(run_ci_outputs(&t.db).await.expect("run"), Next::AfterPeriod);
        t.drop_now().await;
    }
}
