//! The `codeowners_restore` task of `forge worker` (M3 R6, ADR-0024 §9):
//! every minute it records each suspension of CODEOWNERS enforcement past
//! its expiry (forge-db `review::restore_expired_codeowners`: one change
//! per project, actor `system`, the audit row and the event
//! `merge_settings.codeowners_enforcement_restored`) and logs one info line
//! per run that restored any (`forge.worker.codeowners_enforcement_restored`,
//! runbook codeowners). For the record only: the evaluation reads an
//! expired suspension as enforced by itself, so a stopped worker never
//! leaves CODEOWNERS off. A full batch runs again at once.
//!
//! A project whose record fails is logged once with its `error_code`
//! (`forge.worker.codeowners_restore_failed`) and moved to the back of the
//! next scan; the batch goes on (R6 review). A failed scan (the database:
//! `FG-DB-0001` / `FG-DB-0002`) is returned
//! and logged once per failure streak by the supervisor with `error_code`
//! and `task=codeowners_restore`, then retried after the backoff.

use std::time::Duration;

use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::review::{restore_expired_codeowners, RESTORE_MAX_BATCH};
use forge_db::Db;
use forge_errors::ForgeError;

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "codeowners_restore";
/// How often it looks.
pub(crate) const PERIOD: Duration = Duration::from_secs(60);
/// The audit label of the worker's changes.
const ACTOR_LABEL: &str = "forge-worker";

/// One run: one batch; a full one asks to run again at once.
pub(crate) async fn run_codeowners_restore(db: &Db) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: logged below
    ctx.actor_label = Some(ACTOR_LABEL.to_owned());
    let report = restore_expired_codeowners(db, &ctx).await?;
    // R6 review: one row that cannot be recorded is logged once here and
    // moved to the back of the next run; the others are recorded.
    for (project, e) in &report.failed {
        tracing::warn!(
            target: "forge_cli",
            task = NAME,
            event = "forge.worker.codeowners_restore_failed",
            error_code = e.code().id(),
            request_id = %request_id,
            project_id = project.get(),
            error = %e,
            "an expired CODEOWNERS suspension could not be recorded; enforcement is back anyway (runbook codeowners)"
        );
    }
    if report.restored > 0 {
        tracing::info!(
            target: "forge_cli",
            task = NAME,
            event = "forge.worker.codeowners_enforcement_restored",
            request_id = %request_id,
            restored = report.restored,
            found = report.found,
            "CODEOWNERS enforcement came back after its suspension expired (runbook codeowners)"
        );
    }
    Ok(if i64::from(report.found) >= RESTORE_MAX_BATCH {
        Next::Immediately
    } else {
        Next::AfterPeriod
    })
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// A run against a migrated database with nothing expired waits for
    /// the period and writes nothing.
    #[tokio::test]
    async fn a_run_with_nothing_expired_waits_for_the_period() {
        let t = FreshDb::migrated().await;
        let next = run_codeowners_restore(&t.db).await.expect("run");
        assert!(matches!(next, Next::AfterPeriod));
        t.drop_now().await;
    }
}
