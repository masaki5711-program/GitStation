//! Deleting and restoring a project, decided inside their transaction
//! (D5 MR3, ADR-0015 Amendment D5): [`delete_decided`] puts a project into
//! its grace period (`deleted_at`, `purge_after`, `deleted_by`, event
//! `project.deleted`), [`restore_decided`] takes it out again unless the
//! purge has started (`FG-REPO-0019`, event `project.restored`).
//!
//! Lock order, as in `projects::update` and `projects::members`: the project
//! row `FOR UPDATE`, the caller's membership rows `FOR SHARE`, the caller's
//! account row; `restore_decided` then locks the project's repository row
//! (`FOR UPDATE`) — the row the purge job (D5 MR4) moves to `purging` — so
//! a restore and a purge start exclude each other: exactly one of them wins
//! and the other sees its state. `decide` (forge-hosting:
//! `Authorized::reconfirm`) runs before anything is written or revealed.
//!
//! Both events carry a dedup key built from the deletion's time, so a
//! repeat of the same deletion (or of the same restore) is rolled back
//! instead of publishing twice: `project_deleted:<project_id>:<deleted_at
//! epoch>` and `project_restored:<project_id>:<deleted_at epoch of the
//! deletion it undoes>`. The epoch is in microseconds (the column's
//! precision), so a delete, a restore and a delete again within one second
//! are three different events, never a dedup hit.

use std::time::Duration;

use forge_core::dedup_key::{DedupKey, DedupPart};
use forge_core::ids::{ProjectId, UserId};
use forge_core::AuditDetails;
use forge_errors::{ErrorCode, ForgeError};

use super::evented_record;
use super::update::{facts, lock, ProjectUpdateFacts};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::{query_error, Db};

/// A project's deletion as written: when it began and when the purge may
/// start (UTC, RFC 3339).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deletion {
    pub deleted_at: String,
    pub purge_after: String,
    /// Who deleted it (`None`: the operator CLI, actor `system`).
    pub deleted_by: Option<UserId>,
}

/// `FG-REPO-0019` with its `reason`.
fn restore_unavailable(reason: &'static str, project: ProjectId) -> ForgeError {
    ForgeError::new(ErrorCode::RestoreUnavailable)
        .with_context("reason", reason)
        .with_context("project_id", project.get())
}

/// What the deletion UPDATE returned.
type DeletionRow = (String, String, Option<i64>, i64);

const RETURNING: &str = "RETURNING \
    to_char(deleted_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
    to_char(purge_after AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
    deleted_by, floor(extract(epoch FROM deleted_at) * 1000000)::bigint";

/// Clears the deletion and returns it (the CTE reads the row as it was).
const RESTORE: &str =
    "WITH old AS (SELECT deleted_at, purge_after, deleted_by FROM forge.projects \
             WHERE id = $1 AND deleted_at IS NOT NULL), \
             cleared AS (UPDATE forge.projects SET deleted_at = NULL, purge_after = NULL, \
             deleted_by = NULL, updated_at = now() WHERE id = $1 AND deleted_at IS NOT NULL \
             RETURNING id) \
             SELECT to_char(old.deleted_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
             to_char(old.purge_after AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
             old.deleted_by, floor(extract(epoch FROM old.deleted_at) * 1000000)::bigint \
             FROM old JOIN cleared ON true";

fn dedup(prefix: &'static str, project: ProjectId, epoch: i64) -> Result<DedupKey, ForgeError> {
    DedupKey::new(
        prefix,
        &[DedupPart::Id(project.get()), DedupPart::Id(epoch)],
    )
}

/// Delete `project` for `viewer`: it stays restorable for `grace`, then the
/// purge may remove it. The statement is a conditional UPDATE under the
/// row lock; a project already pending deletion is `decide`'s denial
/// (`FG-REPO-0006`, authorize) — and, if `decide` let it pass, the same
/// code from here, never a second deletion. One `project.deleted` audit row
/// and event. Every exit that does not commit rolls back explicitly.
/// Errors: `FG-REPO-0005 resource=project`, `FG-REPO-0006`, whatever
/// `decide` returns.
pub async fn delete_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    grace: Duration,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(Deletion, P), ForgeError> {
    let grace_seconds = i64::try_from(grace.as_secs()).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "grace_out_of_range")
    })?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock(&mut tx, project).await?;
        let decided = decide(&facts(&mut tx, project, viewer, &locked).await?)?;
        let sql = format!(
            "UPDATE forge.projects SET deleted_at = now(), \
             purge_after = now() + $2::bigint * interval '1 second', deleted_by = $3, \
             updated_at = now() WHERE id = $1 AND deleted_at IS NULL {RETURNING}"
        );
        let row: Option<DeletionRow> = tx
            .step(|c| async move {
                sqlx::query_as(&sql)
                    .bind(project.get())
                    .bind(grace_seconds)
                    .bind(viewer.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("projects.delete", e))
            })
            .await?;
        let Some((deleted_at, purge_after, deleted_by, epoch)) = row else {
            return Err(ForgeError::new(ErrorCode::PendingDeletion)
                .with_context("reason", "already_pending")
                .with_context("project_id", project.get()));
        };
        let deleted_by = deleted_by.map(UserId::new);
        let details = AuditDetails::ProjectDeleted {
            project_id: project,
            deleted_at: deleted_at.clone(), // clone: the outcome carries it too
            purge_after: purge_after.clone(), // clone: the outcome carries it too
            deleted_by,
        };
        let mut record = evented_record(&details, project)?;
        for event in &mut record.events {
            event.dedup_key = Some(dedup("project_deleted", project, epoch)?);
        }
        Ok(Settle::Commit(
            record,
            (
                Deletion {
                    deleted_at,
                    purge_after,
                    deleted_by,
                },
                decided,
            ),
        ))
    }
    .await;
    tx.settle("project.deleted", outcome).await
}

/// Restore `project` for `viewer`: clears `deleted_at`, `purge_after` and
/// `deleted_by` and returns the cleared deletion. `decide` first (a caller
/// who may not restore gets the denial, never `FG-REPO-0019`); then
/// `FG-REPO-0019 reason=not_deleted` when the project is not pending
/// deletion and `reason=purging` when its repository is in state `purging`
/// (read under `FOR UPDATE`, module docs). One `project.restored` audit row
/// and event. Every exit that does not commit rolls back explicitly.
/// Errors: `FG-REPO-0005 resource=project`, `FG-REPO-0019`, whatever
/// `decide` returns.
pub async fn restore_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(Deletion, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock(&mut tx, project).await?;
        let decided = decide(&facts(&mut tx, project, viewer, &locked).await?)?;
        if !locked.pending_deletion {
            return Err(restore_unavailable("not_deleted", project));
        }
        let state: Option<String> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT state FROM forge.repositories WHERE project_id = $1 FOR UPDATE",
                )
                .bind(project.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("projects.restore.repository", e))
            })
            .await?;
        match state.as_deref() {
            Some("purging") => return Err(restore_unavailable("purging", project)),
            Some(_) => {}
            None => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "project_without_repository")
                    .with_context("project_id", project.get()))
            }
        }
        // The deletion being undone, read under the project lock.
        let row: Option<DeletionRow> = tx
            .step(|c| async move {
                sqlx::query_as(RESTORE)
                    .bind(project.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("projects.restore", e))
            })
            .await?;
        let Some((deleted_at, purge_after, deleted_by, epoch)) = row else {
            return Err(restore_unavailable("not_deleted", project));
        };
        let deleted_by = deleted_by.map(UserId::new);
        let details = AuditDetails::ProjectRestored {
            project_id: project,
            deleted_at: deleted_at.clone(), // clone: the outcome carries it too
            purge_after: purge_after.clone(), // clone: the outcome carries it too
            deleted_by,
        };
        let mut record = evented_record(&details, project)?;
        for event in &mut record.events {
            event.dedup_key = Some(dedup("project_restored", project, epoch)?);
        }
        Ok(Settle::Commit(
            record,
            (
                Deletion {
                    deleted_at,
                    purge_after,
                    deleted_by,
                },
                decided,
            ),
        ))
    }
    .await;
    tx.settle("project.restored", outcome).await
}
