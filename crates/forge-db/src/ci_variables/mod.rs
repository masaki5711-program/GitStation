//! CI variables (M4 C6, plan 14 §4.1; migration 20261211100000): the typed
//! commands and reads of `forge.ci_variables`.
//!
//! - [`put`] / [`delete`]: a Maintainer's change, audited on the project in
//!   the same transaction (`ci_variable.created` / `.updated` /
//!   `.deleted`; the table's audit trigger refuses a write without it).
//!   The value arrives sealed (`forge_core::ci_secret::CiSecretKeys::seal`,
//!   by the caller that holds the keys): this crate never sees a value.
//! - [`list`]: the metadata of a project's variables (the read pool, which
//!   cannot read the ciphertext).
//! - [`sealed_for`]: the sealed values of some names of a project, on the
//!   write pool — for the claim of a job (forge-pipelines opens them) and
//!   the masking of its log.
//! - [`key_use`]: how many values each key sealed (doctor
//!   `ci.secret_keys`).

use forge_core::ci_secret::{SealedValue, MAX_VARIABLES_PER_PROJECT};
use forge_core::pipeline::CiVariableChange;
use forge_core::{AuditDetails, ProjectId};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::pipelines::{decided_facts, pipeline_of, PipelineRow, PIPELINE_COLUMNS};
use crate::projects::ProjectUpdateFacts;
use crate::{query_error, read_query_error, Db};

/// The audit resource of a variable's change (tables.toml).
pub const RESOURCE: &str = "project";

const TS: &str = "YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"";

/// A variable as read: its metadata, never its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVariable {
    pub name: String,
    pub protected_only: bool,
    pub masked: bool,
    pub value_bytes: i32,
    pub key_id: i16,
    pub created_by: i64,
    pub updated_by: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// A value to store (sealed by the caller).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewValue {
    pub name: String,
    pub sealed: SealedValue,
    pub value_bytes: i32,
    pub protected_only: bool,
    pub masked: bool,
}

/// One stored value as [`sealed_for`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedVariable {
    pub name: String,
    pub sealed: SealedValue,
    pub masked: bool,
}

type VariableRow = (String, bool, bool, i32, i16, i64, i64, String, String);

fn columns() -> String {
    format!(
        "name, protected_only, masked, value_bytes, key_id, created_by, updated_by, \
         to_char(created_at AT TIME ZONE 'UTC', '{TS}'), \
         to_char(updated_at AT TIME ZONE 'UTC', '{TS}')"
    )
}

fn variable_of(r: VariableRow) -> StoredVariable {
    StoredVariable {
        name: r.0,
        protected_only: r.1,
        masked: r.2,
        value_bytes: r.3,
        key_id: r.4,
        created_by: r.5,
        updated_by: r.6,
        created_at: r.7,
        updated_at: r.8,
    }
}

fn record(change: &CiVariableChange) -> ChangeRecord {
    let project = change.project();
    let details = AuditDetails::CiVariable(change.clone()); // clone: the caller logs it too
    ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

/// The variables of `project`, by name (metadata only; the read pool).
pub async fn list(db: &Db, project: ProjectId) -> Result<Vec<StoredVariable>, ForgeError> {
    const QUERY: &str = "ci_variables.list";
    let rows: Vec<VariableRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ci_variables WHERE project_id = $1 ORDER BY name LIMIT $2",
                columns()
            ))
            .bind(project.get())
            .bind(MAX_VARIABLES_PER_PROJECT)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows.into_iter().map(variable_of).collect())
}

/// Set (create or replace) a variable of `project`, decided again on the
/// locked project and caller (`decide`: forge-pipelines reconfirms
/// `authorize()`); `true` with it when it was created. Errors (nothing
/// written): `decide`'s, `FG-WEB-0006 reason=too_many_variables` past
/// [`MAX_VARIABLES_PER_PROJECT`] (two writers at the same moment may pass
/// it by one: the bound is a guard against runaway scripts, not a quota),
/// the database's.
pub async fn put_decided(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    new: &NewValue,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<(), ForgeError>,
) -> Result<(StoredVariable, bool), ForgeError> {
    const QUERY: &str = "ci_variables.put";
    let user = ctx.actor.user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "ci_variable_without_user")
    })?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        decide(&decided_facts(&mut tx, project, forge_core::UserId::new(user)).await?)?;
        let (exists, count): (bool, i64) = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT coalesce(bool_or(name = $2), false), count(*) \
                     FROM forge.ci_variables WHERE project_id = $1",
                )
                .bind(project.get())
                .bind(&new.name)
                .fetch_one(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        if !exists && count >= MAX_VARIABLES_PER_PROJECT {
            return Err(ForgeError::new(ErrorCode::RequestInvalid)
                .with_context("reason", "too_many_variables")
                .with_context("project_id", project.get())
                .with_context("limit", MAX_VARIABLES_PER_PROJECT));
        }
        let sql = format!(
            "INSERT INTO forge.ci_variables (project_id, name, key_id, ciphertext, value_bytes, \
               protected_only, masked, created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8) \
             ON CONFLICT (project_id, name) DO UPDATE SET key_id = EXCLUDED.key_id, \
               ciphertext = EXCLUDED.ciphertext, value_bytes = EXCLUDED.value_bytes, \
               protected_only = EXCLUDED.protected_only, masked = EXCLUDED.masked, \
               updated_by = EXCLUDED.updated_by, updated_at = now() \
             RETURNING {}",
            columns()
        );
        let row: VariableRow = tx
            .step(|c| async move {
                sqlx::query_as(&sql)
                    .bind(project.get())
                    .bind(&new.name)
                    .bind(new.sealed.key_id)
                    .bind(&new.sealed.ciphertext)
                    .bind(new.value_bytes)
                    .bind(new.protected_only)
                    .bind(new.masked)
                    .bind(user)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let stored = variable_of(row);
        let change = CiVariableChange::Set {
            project,
            name: stored.name.clone(), // clone: the audit row owns it
            created: !exists,
            protected_only: stored.protected_only,
            masked: stored.masked,
            key_id: stored.key_id,
        };
        Ok(Settle::Commit(record(&change), (stored, !exists)))
    }
    .await;
    tx.settle("ci_variable.set", outcome).await
}

/// Delete a variable of `project`, decided again as [`put_decided`];
/// `false`: there was none (nothing written).
pub async fn delete_decided(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    name: &str,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<(), ForgeError>,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "ci_variables.delete";
    let user = ctx.actor.user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "ci_variable_without_user")
    })?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        decide(&decided_facts(&mut tx, project, forge_core::UserId::new(user)).await?)?;
        let gone: Option<String> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "DELETE FROM forge.ci_variables WHERE project_id = $1 AND name = $2 \
                     RETURNING name",
                )
                .bind(project.get())
                .bind(name)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some(name) = gone else {
            return Ok(Settle::Discard(false));
        };
        let change = CiVariableChange::Deleted { project, name };
        Ok(Settle::Commit(record(&change), true))
    }
    .await;
    tx.settle("ci_variable.deleted", outcome).await
}

/// The sealed values of `names` in `project` (those that exist), on the
/// write pool (the read pool cannot read a ciphertext).
pub async fn sealed_for(
    db: &Db,
    project: ProjectId,
    names: &[String],
) -> Result<Vec<SealedVariable>, ForgeError> {
    const QUERY: &str = "ci_variables.sealed_for";
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, i16, Vec<u8>, bool)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT name, key_id, ciphertext, masked FROM forge.ci_variables \
                 WHERE project_id = $1 AND name = ANY ($2) ORDER BY name",
            )
            .bind(project.get())
            .bind(names)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(name, key_id, ciphertext, masked)| SealedVariable {
            name,
            sealed: SealedValue { key_id, ciphertext },
            masked,
        })
        .collect())
}

/// What the log of `job` must be masked for: its project and the sealed
/// masked values it may have been given — the names it asks for, only when
/// its pipeline may release secrets (`source = 'push'` and protected,
/// ADR-0029 obligation 5). `None` without such a job.
pub async fn masked_for_job(
    db: &Db,
    job: i64,
) -> Result<Option<(ProjectId, Vec<SealedVariable>)>, ForgeError> {
    const QUERY: &str = "ci_variables.masked_for_job";
    let row: Option<(Vec<String>, PipelineRow)> = db
        .within(QUERY, async {
            let found: Option<(i64, Vec<String>)> =
                sqlx::query_as("SELECT pipeline_id, secrets FROM forge.ci_jobs WHERE id = $1")
                    .bind(job)
                    .fetch_optional(db.write_pool())
                    .await
                    .map_err(|e| query_error(QUERY, e))?;
            let Some((pipeline, names)) = found else {
                return Ok(None);
            };
            let row: PipelineRow = sqlx::query_as(&format!(
                "SELECT {PIPELINE_COLUMNS} FROM forge.ci_pipelines WHERE id = $1"
            ))
            .bind(pipeline)
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))?;
            Ok(Some((names, row)))
        })
        .await?;
    let Some((names, row)) = row else {
        return Ok(None);
    };
    let pipeline = pipeline_of(row)?;
    let project = pipeline.project;
    // ADR-0029 obligation 5: the stored pipeline's answer alone.
    if !pipeline.secrets_allowed() || names.is_empty() {
        return Ok(Some((project, Vec::new())));
    }
    let mut sealed = sealed_for(db, project, &names).await?;
    sealed.retain(|v| v.masked);
    Ok(Some((project, sealed)))
}

/// How many stored values each key sealed, by key id (doctor
/// `ci.secret_keys`; the read pool).
pub async fn key_use(db: &Db) -> Result<Vec<(i16, i64)>, ForgeError> {
    const QUERY: &str = "ci_variables.key_use";
    db.within(QUERY, async {
        sqlx::query_as(
            "SELECT key_id, count(*) FROM forge.ci_variables GROUP BY key_id ORDER BY key_id",
        )
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

#[cfg(test)]
mod tests;
