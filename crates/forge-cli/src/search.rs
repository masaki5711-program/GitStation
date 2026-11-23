//! `forge search reindex [--project <id>]` (plan 20 K2, ADR-0040): rebuild
//! the derived search index from the source tables — one project, or every
//! project (rows of projects that no longer exist are removed too).
//!
//! An operator command that changes only derived data (runbook
//! search-index): it connects with the application's configuration and
//! calls `forge_db::search::reindex` (one transaction per project,
//! idempotent). A project that fails is logged once by forge-db
//! (`FG-KNOW-0002 reason=write_failed`, the cause chained), recorded in
//! forge.search_index_state and listed; the command then fails with
//! `FG-KNOW-0002` so a script sees it.

use forge_core::config::DatabaseConfig;
use forge_core::{ProjectId, RequestId};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use crate::runtime::block_on;

pub fn reindex(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    project: Option<i64>,
) -> Result<String, ForgeError> {
    let project = match project {
        Some(id) if id > 0 => Some(ProjectId::new(id)),
        Some(_) => {
            return Err(ForgeError::new(ErrorCode::RequestInvalid)
                .with_context("reason", "param_invalid")
                .with_context("param", "project")
                .with_context("rule", "not_positive"))
        }
        None => None,
    };
    block_on(async {
        let db = Db::connect(cfg).await?;
        let report = forge_db::search::reindex(&db, request_id, project).await;
        db.close().await;
        let report = report?;
        tracing::info!(
            target: "forge_cli",
            event = "forge.search.reindexed",
            request_id = %request_id,
            projects = report.projects,
            merge_requests = report.merge_requests,
            orphans_removed = report.orphans_removed,
            failed = report.failed.len(),
            "search index rebuilt"
        );
        let summary = format!(
            "search index rebuilt: {} project(s), {} merge request(s), {} orphan row(s) removed\n",
            report.projects, report.merge_requests, report.orphans_removed
        );
        if report.failed.is_empty() {
            return Ok(summary);
        }
        let failed: Vec<String> = report.failed.iter().map(|p| p.get().to_string()).collect();
        Err(ForgeError::new(ErrorCode::SearchIndexItemFailed)
            .with_context("reason", "write_failed")
            .with_context("failed_projects", failed.join(",")))
    })
}
