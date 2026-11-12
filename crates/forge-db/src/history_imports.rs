//! The one-time history import allowance of an empty repository
//! (2026-11-12, ADR-0008 amendment "history import"; `forge_core::
//! history_import`). Table `forge.repository_history_imports` (migration
//! 20261210100000), audited on (`project`, project_id); audit only, no event.
//!
//! - [`allow`] — the operator CLI (`forge admin repo allow-import`): one
//!   row per repository, open until `expires_at`. The repository must be
//!   `ready`; that it has no refs is the CLI's check (git, outside the
//!   transaction). `repository.history_import_allowed`.
//! - [`import_allowed`] — is an allowance open (not claimed, not used, not
//!   expired; the database's clock)? The CLI's and the tests' view.
//! - [`claim_import`] — the push path, before its receive-pack runs: one
//!   atomic `UPDATE … RETURNING` takes the open allowance for the push's
//!   request id; of concurrent pushes exactly one gets `true` (review
//!   2026-11-12, HIGH). [`release_import`] gives it back when the push
//!   applied no ref. Claim and release write only the unaudited claim
//!   columns: no audit row (the consumption is audited).
//! - [`record_import`] — the push path, after an accepted push of the
//!   claimer: marks it used and writes
//!   `repository.history_imported` plus one
//!   `repository.history_secret_imported` row per history finding (at most
//!   [`MAX_AUDITED_FINDINGS`]), in one transaction, actor = the pusher.

use std::time::Duration;

use forge_core::history_import::{
    HistoryImportChange, HistoryImportRecorded, HistoryImportUse, MAX_AUDITED_FINDINGS, MAX_TTL,
};
use forge_core::ids::{ProjectId, RepoId};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{AuditDetails, Pusher, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::repositories::push_context;
use crate::{query_error, Db};

/// The audit resource of the allowance's changes (tables.toml).
const RESOURCE: &str = "project";

/// What [`allow`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedImport {
    pub project_id: ProjectId,
    /// RFC 3339, UTC.
    pub expires_at: String,
    /// A previous allowance of the repository (used or not) was replaced.
    pub replaced: bool,
}

fn refused(reason: &'static str, repo: RepoId) -> ForgeError {
    ForgeError::new(ErrorCode::HistoryImportRefused)
        .with_context("reason", reason)
        .with_context("repo_id", repo.get())
}

fn not_found(repo: RepoId) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "repository")
        .with_context("reason", "absent")
        .with_context("repo_id", repo.get())
}

fn entry(change: &HistoryImportChange) -> AuditEntry {
    let project = change.project();
    let details = AuditDetails::HistoryImport(change.clone()); // clone: the details own their copy
    AuditEntry {
        action: details.action(),
        outcome: Outcome::Succeeded,
        resource_type: RESOURCE,
        resource_id: Some(project.get()),
        project_id: Some(project),
        error_code: None,
        details: details.to_value(),
    }
}

/// Allow one history import into `repo` for `ttl` (1 s to
/// [`MAX_TTL`]; the CLI takes whole hours). Errors (nothing written):
/// `FG-GIT-0025` (`ttl_out_of_range`, `repository_not_ready`),
/// `FG-REPO-0005 resource=repository`, the change's own.
pub async fn allow(
    db: &Db,
    ctx: &ChangeContext,
    repo: RepoId,
    ttl: Duration,
) -> Result<AllowedImport, ForgeError> {
    if ttl < Duration::from_secs(1) || ttl > MAX_TTL {
        return Err(refused("ttl_out_of_range", repo).with_context("ttl_secs", ttl.as_secs()));
    }
    let ttl_secs = ttl.as_secs_f64();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let row: Option<(i64, String)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT project_id, state FROM forge.repositories WHERE id = $1 FOR UPDATE",
                )
                .bind(repo.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("history_imports.allow.repository", e))
            })
            .await?;
        let Some((project, state)) = row else {
            return Err(not_found(repo));
        };
        if state != "ready" {
            return Err(refused("repository_not_ready", repo).with_context("state", state));
        }
        let project = ProjectId::new(project);
        let (expires_at, replaced): (String, bool) = tx
            .step(|c| async move {
                sqlx::query_as(
                    "INSERT INTO forge.repository_history_imports \
                     (repository_id, project_id, expires_at) \
                     VALUES ($1, $2, now() + make_interval(secs => $3)) \
                     ON CONFLICT (repository_id) DO UPDATE SET \
                     project_id = EXCLUDED.project_id, allowed_at = now(), \
                     expires_at = EXCLUDED.expires_at, claimed_at = NULL, \
                     claimed_request_id = NULL, consumed_at = NULL, consumed_request_id = NULL \
                     RETURNING to_char(expires_at AT TIME ZONE 'UTC', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), (xmax::text <> '0')",
                )
                .bind(repo.get())
                .bind(project.get())
                .bind(ttl_secs)
                .fetch_one(c)
                .await
                .map_err(|e| query_error("history_imports.allow.upsert", e))
            })
            .await?;
        let change = HistoryImportChange::Allowed {
            project,
            repository: repo,
            expires_at: expires_at.clone(), // clone: the audit row and the answer
            ttl_secs: ttl.as_secs(),
        };
        let record = ChangeRecord {
            audit: entry(&change),
            affected: Vec::new(),
            events: Vec::new(),
        };
        Ok(Settle::Commit(
            record,
            AllowedImport {
                project_id: project,
                expires_at,
                replaced,
            },
        ))
    }
    .await;
    tx.settle("repository.history_import_allowed", outcome)
        .await
}

/// Whether `repo` has an open allowance (not claimed, not used,
/// `now() < expires_at`), read on the write pool.
pub async fn import_allowed(db: &Db, repo: RepoId) -> Result<bool, ForgeError> {
    db.within("history_imports.allowed", async {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.repository_history_imports \
             WHERE repository_id = $1 AND consumed_at IS NULL AND claimed_at IS NULL \
             AND expires_at > now())",
        )
        .bind(repo.get())
        .fetch_one(db.write_pool())
        .await
        .map_err(|e| query_error("history_imports.allowed", e))
    })
    .await
}

/// Claim `repo`'s open allowance for the push `request` (see the module
/// docs): `true` when this call took it.
pub async fn claim_import(db: &Db, repo: RepoId, request: &RequestId) -> Result<bool, ForgeError> {
    db.within("history_imports.claim", async {
        let claimed: Option<i64> = sqlx::query_scalar(
            "UPDATE forge.repository_history_imports SET claimed_at = now(), \
             claimed_request_id = $2 WHERE repository_id = $1 AND claimed_at IS NULL \
             AND consumed_at IS NULL AND expires_at > now() RETURNING repository_id",
        )
        .bind(repo.get())
        .bind(request.as_str())
        .fetch_optional(db.write_pool())
        .await
        .map_err(|e| query_error("history_imports.claim", e))?;
        Ok(claimed.is_some())
    })
    .await
}

/// Give back `request`'s claim (its push applied no ref). Nothing to give
/// back is not an error (an operator allowed anew meanwhile).
pub async fn release_import(db: &Db, repo: RepoId, request: &RequestId) -> Result<(), ForgeError> {
    db.within("history_imports.release", async {
        sqlx::query(
            "UPDATE forge.repository_history_imports SET claimed_at = NULL, \
             claimed_request_id = NULL WHERE repository_id = $1 AND claimed_request_id = $2 \
             AND consumed_at IS NULL",
        )
        .bind(repo.get())
        .bind(request.as_str())
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error("history_imports.release", e))?;
        Ok(())
    })
    .await
}

/// After an accepted push of the claimer (see the module docs).
/// `ctx.request_id` is the push's. Errors: `FG-REPO-0005
/// resource=repository`, the change's own.
pub async fn record_import(
    db: &Db,
    ctx: &ChangeContext,
    repo: RepoId,
    used: &HistoryImportUse,
) -> Result<HistoryImportRecorded, ForgeError> {
    let request_id = ctx.request_id.as_str().to_owned();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let project: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar("SELECT project_id FROM forge.repositories WHERE id = $1")
                    .bind(repo.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("history_imports.record.repository", e))
            })
            .await?;
        let project = ProjectId::new(project.ok_or_else(|| not_found(repo))?);
        // The claimer's only; used up even when it expired during the
        // push (the push was decided while it was open).
        let consumed: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "UPDATE forge.repository_history_imports SET consumed_at = now(), \
                     consumed_request_id = $2 WHERE repository_id = $1 AND consumed_at IS NULL \
                     AND claimed_request_id = $2 RETURNING repository_id",
                )
                .bind(repo.get())
                .bind(request_id)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("history_imports.record.consume", e))
            })
            .await?;
        let kept = used
            .findings
            .get(..MAX_AUDITED_FINDINGS)
            .unwrap_or(&used.findings);
        let change = HistoryImportChange::Imported {
            project,
            repository: repo,
            history_findings: used.total.max(used.findings.len()),
            audited_findings: kept.len(),
            consumed: consumed.is_some(),
        };
        let affected = kept
            .iter()
            .map(|finding| {
                entry(&HistoryImportChange::SecretImported {
                    project,
                    repository: repo,
                    finding: finding.clone(), // clone: each audit row owns its finding
                })
            })
            .collect();
        let record = ChangeRecord {
            audit: entry(&change),
            affected,
            events: Vec::new(),
        };
        let recorded = if consumed.is_some() {
            HistoryImportRecorded::Consumed
        } else {
            HistoryImportRecorded::AlreadyUsed
        };
        Ok(Settle::Commit(record, recorded))
    }
    .await;
    tx.settle("repository.history_imported", outcome).await
}

/// The push path's view (`forge serve` injects the `Db`).
impl forge_core::HistoryImports for Db {
    fn claim_import<'a>(&'a self, repo: RepoId, request: &'a RequestId) -> LookupFuture<'a, bool> {
        Box::pin(claim_import(self, repo, request))
    }

    fn release_import<'a>(&'a self, repo: RepoId, request: &'a RequestId) -> LookupFuture<'a, ()> {
        Box::pin(release_import(self, repo, request))
    }

    fn record_import<'a>(
        &'a self,
        pusher: &'a Pusher,
        repo: RepoId,
        used: &'a HistoryImportUse,
    ) -> LookupFuture<'a, HistoryImportRecorded> {
        Box::pin(async move { record_import(self, &push_context(pusher), repo, used).await })
    }
}

#[cfg(test)]
#[path = "history_imports_tests.rs"]
mod tests;
