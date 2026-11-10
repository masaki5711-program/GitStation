//! The document lint's migration setting (plan 13 Q13, ADR-0026; migration
//! 20261203100000): a project may lower its broken document references
//! (`ref_broken`) to warnings in `forge/doc-lint` until a date.
//!
//! - [`read`]: the setting (if any) with the database's date (UTC) and
//!   whether it is still in force — **the date is compared in SQL**, with
//!   the database's clock, never a web server's or a client's. The
//!   `check.spec_index` job reads it through [`in_force`].
//! - [`set_doc_lint_migration_decided`] / [`clear_doc_lint_migration_decided`]: a person's change, decided again
//!   inside the transaction (forge-checks reconfirms `authorize()` on the
//!   locked facts; only it calls them — `forge-checks/tests/decided_callers.rs`).
//!   `doc_lint_migration.set` / `.cleared`, audit only, on the project.
//!
//! A date is in force through its own day (UTC): `warn_until >= today`.
//! Setting one needs `today < warn_until <= today + MAX_DAYS`
//! (`FG-DOC-0005`, the table's CHECK is the backstop). An expired row is
//! not deleted: it simply stops applying.

use forge_core::doc_lint_migration::{DocLintMigrationChange, MAX_DAYS};
use forge_core::{AuditDetails, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::{query_error, read_query_error, Db};

/// The audit resource type (the project).
const RESOURCE: &str = "project";

const TS: &str = "YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"";

/// A stored migration date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocLintMigration {
    /// `YYYY-MM-DD`: the last day (UTC) broken references are warnings.
    pub warn_until: String,
    pub set_by: UserId,
    /// RFC 3339, UTC.
    pub set_at: String,
    /// `warn_until` is today or later (the database's date).
    pub in_force: bool,
}

/// The setting of a project as read now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRead {
    pub project: ProjectId,
    /// `None`: never set, or cleared.
    pub setting: Option<DocLintMigration>,
    /// The database's date (UTC), `YYYY-MM-DD`.
    pub today: String,
    /// The latest date a new setting may name (`today + MAX_DAYS`).
    pub max_until: String,
}

impl MigrationRead {
    /// The date in force, if any.
    pub fn in_force(&self) -> Option<&str> {
        self.setting
            .as_ref()
            .filter(|s| s.in_force)
            .map(|s| s.warn_until.as_str())
    }
}

/// What [`set_doc_lint_migration_decided`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOutcome {
    Created,
    Updated,
    /// The same date was in force already: nothing written.
    Unchanged,
}

type ReadRow = (
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<bool>,
);

fn read_sql() -> String {
    format!(
        "SELECT to_char(t.d, 'YYYY-MM-DD'), to_char(t.d + {MAX_DAYS}, 'YYYY-MM-DD'), \
         to_char(m.warn_until, 'YYYY-MM-DD'), m.set_by, \
         to_char(m.set_at AT TIME ZONE 'UTC', '{TS}'), m.warn_until >= t.d \
         FROM (SELECT (now() AT TIME ZONE 'UTC')::date AS d) t \
         LEFT JOIN forge.doc_lint_migrations m ON m.project_id = $1"
    )
}

fn read_of(project: ProjectId, row: ReadRow) -> Result<MigrationRead, ForgeError> {
    let (today, max_until, until, set_by, set_at, in_force) = row;
    let setting = match (until, set_by, set_at, in_force) {
        (Some(warn_until), Some(set_by), Some(set_at), Some(in_force)) => Some(DocLintMigration {
            warn_until,
            set_by: UserId::new(set_by),
            set_at,
            in_force,
        }),
        (None, None, None, None) => None,
        _ => {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "doc_lint_migration_row_partial")
                .with_context("project_id", project.get()))
        }
    };
    Ok(MigrationRead {
        project,
        setting,
        today,
        max_until,
    })
}

/// The setting of `project` now (the read pool).
pub async fn read(db: &Db, project: ProjectId) -> Result<MigrationRead, ForgeError> {
    const QUERY: &str = "doc_lint_migration.read";
    let sql = read_sql();
    let row: ReadRow = db
        .within(QUERY, async {
            sqlx::query_as(&sql)
                .bind(project.get())
                .fetch_one(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    read_of(project, row)
}

/// The date in force for `project`, if any (the `check.spec_index` job).
pub async fn in_force(db: &Db, project: ProjectId) -> Result<Option<String>, ForgeError> {
    Ok(read(db, project).await?.in_force().map(str::to_owned))
}

fn record(change: DocLintMigrationChange, project: ProjectId) -> ChangeRecord {
    let details = AuditDetails::DocLintMigration(change);
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

async fn decided_facts(
    tx: &mut ChangeTx,
    project: ProjectId,
    caller: UserId,
) -> Result<ProjectUpdateFacts, ForgeError> {
    let locked = lock_shared(tx, project).await?;
    locked_access_facts(tx, project, caller, &locked).await
}

/// today, max_until, whether `warn_until` is after today, whether it is at
/// most `MAX_DAYS` ahead (the transaction's `now()`, as the CHECK).
type Bounds = (String, String, bool, bool);

/// Set `project`'s migration date to `warn_until` (a calendar date
/// `YYYY-MM-DD`: the caller checked its shape). Errors (nothing written):
/// `decide`'s, `FG-DOC-0005` (`not_in_future` / `too_far`, with `today`
/// and `max_until`), `FG-REPO-0005 resource=project`.
pub async fn set_doc_lint_migration_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    warn_until: &str,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(SetOutcome, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let until = warn_until.to_owned();
        let bounds: Bounds = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "SELECT to_char(t.d, 'YYYY-MM-DD'), to_char(t.d + {MAX_DAYS}, 'YYYY-MM-DD'), \
                     $1::date > t.d, $1::date <= t.d + {MAX_DAYS} \
                     FROM (SELECT (now() AT TIME ZONE 'UTC')::date AS d) t"
                ))
                .bind(until)
                .fetch_one(c)
                .await
                .map_err(|e| query_error("doc_lint_migration.bounds", e))
            })
            .await?;
        let (today, max_until, future, within) = bounds;
        if !future || !within {
            return Err(ForgeError::new(ErrorCode::DocLintMigrationUntilInvalid)
                .with_context("reason", if future { "too_far" } else { "not_in_future" })
                .with_context("warn_until", warn_until.to_owned())
                .with_context("today", today)
                .with_context("max_until", max_until)
                .with_context("project_id", project.get()));
        }
        let previous: Option<String> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT to_char(warn_until, 'YYYY-MM-DD') FROM forge.doc_lint_migrations \
                     WHERE project_id = $1 FOR UPDATE",
                )
                .bind(project.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("doc_lint_migration.lock", e))
            })
            .await?;
        if previous.as_deref() == Some(warn_until) {
            return Ok(Settle::Discard((SetOutcome::Unchanged, decided)));
        }
        let until = warn_until.to_owned();
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.doc_lint_migrations (project_id, warn_until, set_by) \
                 VALUES ($1, $2::date, $3) ON CONFLICT (project_id) DO UPDATE SET \
                 warn_until = EXCLUDED.warn_until, set_by = EXCLUDED.set_by, set_at = now()",
            )
            .bind(project.get())
            .bind(until)
            .bind(caller.get())
            .execute(c)
            .await
            .map_err(|e| query_error("doc_lint_migration.set", e))
        })
        .await?;
        let outcome = if previous.is_some() {
            SetOutcome::Updated
        } else {
            SetOutcome::Created
        };
        let change = DocLintMigrationChange::Set {
            project,
            warn_until: warn_until.to_owned(),
            previous,
        };
        Ok(Settle::Commit(record(change, project), (outcome, decided)))
    }
    .await;
    tx.settle("doc_lint_migration.set", outcome).await
}

/// Remove `project`'s migration date (in force or expired): broken
/// references are errors from now on. Errors: `decide`'s, `FG-REPO-0005`
/// (`resource=project` / `doc_lint_migration` when there is none).
pub async fn clear_doc_lint_migration_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let removed: Option<String> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "DELETE FROM forge.doc_lint_migrations WHERE project_id = $1 \
                     RETURNING to_char(warn_until, 'YYYY-MM-DD')",
                )
                .bind(project.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("doc_lint_migration.clear", e))
            })
            .await?;
        let warn_until = removed.ok_or_else(|| {
            ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "doc_lint_migration")
                .with_context("reason", "absent")
                .with_context("project_id", project.get())
        })?;
        let change = DocLintMigrationChange::Cleared {
            project,
            warn_until,
        };
        Ok(Settle::Commit(record(change, project), decided))
    }
    .await;
    tx.settle("doc_lint_migration.cleared", outcome).await
}

#[cfg(test)]
#[path = "doc_lint_migration_tests.rs"]
mod tests;
