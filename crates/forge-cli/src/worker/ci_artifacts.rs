//! The `ci_artifact_expiry` task of `forge worker` (M4 C6, plan 14 §9 #6):
//! every [`PERIOD`] the CI artifacts past their 30 days (not `keep`, a
//! deployed version) lose their file, then their row
//! (`forge_pipelines::artifacts::expire_artifacts`); then the artifact
//! directories of projects that no longer exist (the purge deleted their
//! rows) are removed. A full batch runs again at once.
//!
//! A failed run is returned and logged once per failure streak by the
//! supervisor with `error_code` and `task=ci_artifact_expiry`; one artifact
//! that cannot be removed keeps its row (the next pass tries again).

use std::time::Duration;

use forge_core::RequestId;
use forge_db::Db;
use forge_errors::ForgeError;
use forge_pipelines::artifacts::{expire_artifacts, ArtifactStore, EXPIRY_BATCH};

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "ci_artifact_expiry";
/// How often it runs.
pub(crate) const PERIOD: Duration = Duration::from_secs(600);

/// One run.
pub(crate) async fn run_ci_artifact_expiry(
    db: &Db,
    store: &ArtifactStore,
) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let removed = expire_artifacts(db, store, &request_id).await?;
    store.remove_purged(db, &request_id).await?;
    if i64::try_from(removed).unwrap_or(i64::MAX) >= EXPIRY_BATCH {
        return Ok(Next::Immediately);
    }
    Ok(Next::AfterPeriod)
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// A run with nothing expired waits for the period, and leaves the
    /// directory of a project that exists.
    #[tokio::test]
    async fn an_expiry_run_with_nothing_due_waits_for_the_period() {
        let t = FreshDb::migrated().await;
        let root = tempfile::tempdir().expect("root");
        let store = ArtifactStore::new(root.path());
        std::fs::create_dir_all(root.path().join("ci-artifacts").join("999999"))
            .expect("orphan dir");
        assert_eq!(
            run_ci_artifact_expiry(&t.db, &store).await.expect("run"),
            Next::AfterPeriod
        );
        assert!(
            !root.path().join("ci-artifacts").join("999999").exists(),
            "a purged project's directory is removed"
        );
        t.drop_now().await;
    }
}
