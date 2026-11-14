//! The `ci_ai_eval` task of `forge worker` (plan 17 P17-2, ADR-0034):
//! every [`PERIOD`] it writes the `forge/ai-eval` check of each pipeline
//! that declares the AI evaluation report (`eval-report.json`) and whose
//! status changed (`forge_pipelines::ai_eval::report_ai_eval_checks`, as
//! the system with the label `forge-ci`): the report artifact read from the
//! storage root, the baseline read with the browse API at the pipeline's
//! commit. A full batch runs again at once.
//!
//! A failed run (`FG-DB-*`, or one pipeline's read or write: storage,
//! git, a repository not ready) is returned and logged once per failure
//! streak by the supervisor with `error_code` and `task=ci_ai_eval`, then
//! retried after the backoff; one pipeline that fails does not stop the
//! others (each is logged as `forge.ai_eval.check_failed`). Runs only with
//! the storage (the report files and the repositories live under it).

use std::time::Duration;

use forge_core::RequestId;
use forge_db::Db;
use forge_errors::ForgeError;
use forge_git::browse::{BrowseLimits, Browser};
use forge_pipelines::ai_eval::{report_ai_eval_checks, AI_EVAL_BATCH};
use forge_pipelines::artifacts::ArtifactStore;

use super::provision::Storage;
use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "ci_ai_eval";
/// How often it runs (the check follows its pipeline within about this).
pub(crate) const PERIOD: Duration = Duration::from_secs(10);

/// One run.
pub(crate) async fn run_ci_ai_eval(db: &Db, storage: &Storage) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let store = ArtifactStore::new(storage.root.path());
    let browse = Browser::new(
        storage.git.clone(),  // clone: the browser owns its handles
        storage.root.clone(), // clone: as git
        BrowseLimits::default(),
    );
    let pass = report_ai_eval_checks(db, &store, &browse, &request_id).await?;
    if i64::try_from(pass.looked_at()).unwrap_or(i64::MAX) >= AI_EVAL_BATCH {
        return Ok(Next::Immediately);
    }
    Ok(Next::AfterPeriod)
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// A run with no declaring pipeline waits for the period.
    #[tokio::test]
    async fn a_run_with_nothing_to_report_waits_for_the_period() {
        let t = FreshDb::migrated().await;
        let root = tempfile::tempdir().expect("root");
        let storage = Storage::open(root.path()).expect("storage");
        assert_eq!(
            run_ci_ai_eval(&t.db, &storage).await.expect("run"),
            Next::AfterPeriod
        );
        t.drop_now().await;
    }
}
