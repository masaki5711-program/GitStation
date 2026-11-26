//! The recent changes an incident diagnosis reads (plan 19 §3.2, M5 L5-7):
//! deployments and migrations within a window, in one transaction on the
//! read pool. The transaction first reads its own role and read-only flag
//! and refuses to go on unless they are `forge_ro` / `on` (else
//! `FG-SYS-0001 reason=diagnosis_not_read_only`), so `forge diagnose`
//! reads its facts as the read-only role whatever the configuration
//! (`read_only_db_role`).

use forge_errors::{ErrorCode, ForgeError};

use crate::{read_query_error, Db};

/// The most rows of each kind one diagnosis reads.
pub const MAX_ROWS: i64 = 50;

/// One deployment forge made (to an internal app server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentChange {
    pub id: i64,
    pub project_id: i64,
    pub tier: String,
    pub status: String,
    pub sha: String,
    pub error_code: Option<String>,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ` (the last update).
    pub at: String,
}

/// One applied migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationChange {
    pub version: i64,
    pub description: String,
    pub success: bool,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub installed_on: String,
}

/// What [`recent_changes`] read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecentChanges {
    /// The session's role (always `forge_ro`).
    pub role: String,
    pub deployments: Vec<DeploymentChange>,
    pub migrations: Vec<MigrationChange>,
}

/// The deployments and migrations of the last `since_secs` seconds (module
/// docs), newest first, at most [`MAX_ROWS`] of each.
pub async fn recent_changes(db: &Db, since_secs: i64) -> Result<RecentChanges, ForgeError> {
    const QUERY: &str = "llm.diagnosis_recent_changes";
    let since = since_secs.max(0) as f64;
    db.within(QUERY, async {
        let mut tx = db
            .pool()
            .executor()
            .begin()
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        let (role, read_only): (String, String) =
            sqlx::query_as("SELECT current_user::text, current_setting('transaction_read_only')")
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
        if role != "forge_ro" || read_only != "on" {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "diagnosis_not_read_only")
                .with_context("role", role)
                .with_context("transaction_read_only", read_only));
        }
        type DeployRow = (i64, i64, String, String, String, Option<String>, String);
        let deployments: Vec<DeployRow> = sqlx::query_as(
            "SELECT id, project_id, tier, status, sha, error_code, \
             to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
             FROM forge.deployments WHERE updated_at > now() - make_interval(secs => $1) \
             ORDER BY updated_at DESC, id DESC LIMIT $2",
        )
        .bind(since)
        .bind(MAX_ROWS)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let migrations: Vec<(i64, String, bool, String)> = sqlx::query_as(
            "SELECT version, description, success, \
             to_char(installed_on AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
             FROM forge._sqlx_migrations WHERE installed_on > now() - make_interval(secs => $1) \
             ORDER BY installed_on DESC, version DESC LIMIT $2",
        )
        .bind(since)
        .bind(MAX_ROWS)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        // Nothing was written; ending the read-only transaction either way.
        tx.rollback()
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        Ok(RecentChanges {
            role,
            deployments: deployments
                .into_iter()
                .map(
                    |(id, project_id, tier, status, sha, error_code, at)| DeploymentChange {
                        id,
                        project_id,
                        tier,
                        status,
                        sha,
                        error_code,
                        at,
                    },
                )
                .collect(),
            migrations: migrations
                .into_iter()
                .map(
                    |(version, description, success, installed_on)| MigrationChange {
                        version,
                        description,
                        success,
                        installed_on,
                    },
                )
                .collect(),
        })
    })
    .await
}

#[cfg(test)]
#[path = "diagnosis_tests.rs"]
mod tests;
