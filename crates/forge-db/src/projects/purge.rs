//! The purge of a project whose grace period is over (D5 MR4, ADR-0015
//! Amendment D5): the database half. The caller (`forge worker`'s purge
//! task, `forge admin project purge --now`) runs, per project:
//!
//! 1. [`start`] — one transaction: lock the project row `FOR UPDATE` (the
//!    order of `restore_decided`: project row, then repository row), re-read
//!    `deleted_at` / `purge_after` under the lock, lock the repository row
//!    `FOR UPDATE`, move it to `purging` with one audit-only
//!    `repository.purge_started` row. A project restored (or never deleted,
//!    or not expired) in the meantime is a clean [`Started::Skipped`], never
//!    the guard's `FG-DB-0009`: the guard of migration 20261102100200 is the
//!    database's own check that exactly one of restore and purge wins. A
//!    repository already `purging` is [`Started::Resumed`] (a crash or a
//!    failure after phase 1: the purge continues, it never goes back).
//! 2. The caller removes the bare directory (forge-git
//!    `StorageRoot::remove_until`, the path derived from the repository id).
//! 3. [`finish`] — one transaction: lock the project row, the repository row
//!    and the project's namespace row (`FOR UPDATE`), then the path's
//!    advisory lock (rows before path locks, as moves, creates and tombstone
//!    releases); delete the members, protected refs, the repository, the
//!    namespace (the `moved` tombstones that named it lose their target by
//!    `ON DELETE SET NULL`: their redirect ends, their path stays blocked
//!    and is released like a `deleted` tombstone) and the project; keep the
//!    path with one `deleted` tombstone (`owner_at_write` = `deleted_by`);
//!    write one `project.purged` audit row and one `project.purged` event
//!    (admin class, dedup `project_purged:<project_id>`). The delete guards
//!    (migration 20261102100100) allow exactly these deletions.
//!
//! The project namespace is a leaf (projects have no children), so a group
//! move above it holds its namespace row (`FOR NO KEY UPDATE`) and the path
//! locks only after it; `finish` waits for that row and reads the path the
//! move committed. Neither takes the other's locks in the opposite order.
//!
//! [`due`] lists the projects to purge, oldest `purge_after` first,
//! without locks (each one is re-checked under its locks).
//!
//! [`PurgeLock`] keeps two purges of one project apart (the worker and
//! `forge admin project purge --now`, two workers, a removal still running
//! after its deadline): a **session** advisory lock on its own connection,
//! held from before `start` until after `finish` (and, after a cut
//! removal, until the removal really ends). Its key is the two-`int4` form
//! `(PURGE_LOCK_CLASS, project id mod 2^31-1)`: PostgreSQL keeps the
//! one-`bigint` keys (the path locks `hashtextextended('namespace:' ||
//! path, 0)`, the bootstrap-admin lock) apart from the two-`int4` keys, and
//! the other two-`int4` users take class 1718579815 (sequencer 1, seal 8).
//! A collision of two project ids only defers one of them a run.

use forge_core::ids::{NamespaceId, ProjectId, RepoId, TombstoneId, UserId};
use forge_core::{AuditDetails, DedupKey, DedupPart, RepositoryState};
use forge_errors::{ErrorCode, ForgeError};

use sqlx::pool::PoolConnection;
use sqlx::Postgres;

use super::RESOURCE;
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::error_map::connect_error;
use crate::namespaces::lock_path;
use crate::{query_error, Db};

/// The class (first `int4`) of the purge's session advisory lock, `FGPU`.
pub const PURGE_LOCK_CLASS: i32 = 0x4647_5055;

/// The session advisory lock of one project's purge (module docs). Release
/// it with [`PurgeLock::release`]; dropped without it, its connection is
/// closed instead of going back to the pool, which ends the lock too
/// (never handed to another user still held).
#[derive(Debug)]
pub struct PurgeLock {
    conn: Option<PoolConnection<Postgres>>,
    project: ProjectId,
}

impl PurgeLock {
    /// Take the lock of `project` if nobody holds it: `None` when another
    /// purge of it is in progress. Errors: `FG-DB-0001` (no connection),
    /// `FG-DB-0002`.
    pub async fn try_acquire(db: &Db, project: ProjectId) -> Result<Option<Self>, ForgeError> {
        db.within("projects.purge.lock", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| connect_error(db.target(), e))?;
            let taken: bool = sqlx::query_scalar(
                "SELECT pg_catalog.pg_try_advisory_lock($1::int4, ($2::bigint % 2147483647)::int4)",
            )
            .bind(PURGE_LOCK_CLASS)
            .bind(project.get())
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| query_error("projects.purge.lock", e))?;
            Ok(taken.then_some(Self {
                conn: Some(conn),
                project,
            }))
        })
        .await
    }

    /// The project it locks.
    pub fn project(&self) -> ProjectId {
        self.project
    }

    /// Unlock and give the connection back. A failed unlock closes the
    /// connection instead (the lock ends with the session); it is logged
    /// here, the only place that knows, and never fails the purge.
    pub async fn release(mut self) {
        let Some(mut conn) = self.conn.take() else {
            return;
        };
        let unlocked = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            sqlx::query_scalar::<_, bool>(
                "SELECT pg_catalog.pg_advisory_unlock($1::int4, ($2::bigint % 2147483647)::int4)",
            )
            .bind(PURGE_LOCK_CLASS)
            .bind(self.project.get())
            .fetch_one(&mut *conn),
        )
        .await;
        if !matches!(unlocked, Ok(Ok(true))) {
            tracing::warn!(
                target: "forge_db",
                project_id = self.project.get(),
                "the purge lock could not be released; its connection is closed instead"
            );
            drop(conn.detach());
        }
    }
}

impl Drop for PurgeLock {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Closing the session ends its advisory lock.
            drop(conn.detach());
        }
    }
}

/// The prefix of the `project.purged` event's dedup key,
/// `project_purged:<project_id>` (project ids are never reused).
pub const PURGED_DEDUP_PREFIX: &str = "project_purged";
/// The largest batch [`due`] returns.
pub const DUE_MAX: i64 = 1000;

/// A project whose grace period is over, as [`due`] read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuePurge {
    pub project_id: ProjectId,
    pub repository_id: RepoId,
    /// `purging` when an earlier run started it.
    pub state: RepositoryState,
    /// UTC, RFC 3339.
    pub purge_after: String,
    /// Seconds since `purge_after` (≥ 0).
    pub overdue_secs: i64,
}

/// What [`start`] did.
#[must_use = "only Started and Resumed may go on to remove the directory"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    /// The repository moved to `purging` now (from `from`).
    Started {
        repository_id: RepoId,
        from: RepositoryState,
    },
    /// The repository was `purging` already: an earlier run started it.
    Resumed { repository_id: RepoId },
    /// Not purged, nothing written: `reason` = `gone` (no such project any
    /// more: purged by a concurrent run), `not_deleted` (restored, or never
    /// deleted), `not_expired` (the grace period is not over).
    Skipped { reason: &'static str },
}

/// What [`finish`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finished {
    /// The rows are deleted, the tombstone, audit row and event written.
    Purged(Purged),
    /// The project is gone already (a concurrent run finished it).
    Gone,
}

/// The purged project as `project.purged` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Purged {
    pub project_id: ProjectId,
    pub repository_id: RepoId,
    pub namespace_id: NamespaceId,
    /// The `deleted` tombstone that keeps the path.
    pub tombstone_id: TombstoneId,
    /// `moved` tombstones whose redirect to this project ended.
    pub redirects_ended: i64,
}

fn internal(reason: &'static str, project: ProjectId) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("project_id", project.get())
}

fn state_of(raw: &str, project: ProjectId) -> Result<RepositoryState, ForgeError> {
    RepositoryState::parse(raw).ok_or_else(|| internal("unknown_repository_state", project))
}

/// The projects pending deletion whose `purge_after` has passed, with
/// their repository, oldest `purge_after` first (then by id), at most
/// `limit` (1..=[`DUE_MAX`]). No locks: [`start`] re-checks each.
pub async fn due(db: &Db, limit: i64) -> Result<Vec<DuePurge>, ForgeError> {
    let limit = limit.clamp(1, DUE_MAX);
    let rows: Vec<(i64, i64, String, String, i64)> = db
        .within("projects.purge.due", async {
            sqlx::query_as(
                "SELECT p.id, r.id, r.state, \
                 to_char(p.purge_after AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                 greatest(0, floor(extract(epoch FROM clock_timestamp() - p.purge_after)))::bigint \
                 FROM forge.projects p JOIN forge.repositories r ON r.project_id = p.id \
                 WHERE p.deleted_at IS NOT NULL AND p.purge_after <= clock_timestamp() \
                 ORDER BY p.purge_after, p.id LIMIT $1",
            )
            .bind(limit)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error("projects.purge.due", e))
        })
        .await?;
    rows.into_iter()
        .map(|(project, repo, state, purge_after, overdue_secs)| {
            let project_id = ProjectId::new(project);
            Ok(DuePurge {
                project_id,
                repository_id: RepoId::new(repo),
                state: state_of(&state, project_id)?,
                purge_after,
                overdue_secs,
            })
        })
        .collect()
}

/// Phase 1 (module docs): move the repository of `project` to `purging`
/// when its grace period is over. Every exit that does not commit rolls
/// back explicitly. Errors: the change's own (`FG-DB-0002`, …),
/// `FG-SYS-0001 project_without_repository`.
pub async fn start(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
) -> Result<Started, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = start_steps(&mut tx, project).await;
    tx.settle("repository.purge_started", outcome).await
}

async fn start_steps(tx: &mut ChangeTx, project: ProjectId) -> Result<Settle<Started>, ForgeError> {
    // The project row first, as restore locks it; re-read under the lock.
    let row: Option<(bool, bool)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT deleted_at IS NOT NULL, coalesce(purge_after <= clock_timestamp(), false) \
                 FROM forge.projects WHERE id = $1 FOR UPDATE",
            )
            .bind(project.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("projects.purge.start.lock", e))
        })
        .await?;
    let reason = match row {
        None => Some("gone"),
        Some((false, _)) => Some("not_deleted"),
        Some((true, false)) => Some("not_expired"),
        Some((true, true)) => None,
    };
    if let Some(reason) = reason {
        return Ok(Settle::Discard(Started::Skipped { reason }));
    }
    let repo: Option<(i64, String)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT id, state FROM forge.repositories WHERE project_id = $1 FOR UPDATE",
            )
            .bind(project.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("projects.purge.start.repository", e))
        })
        .await?;
    let Some((repo, state)) = repo else {
        return Err(internal("project_without_repository", project));
    };
    let repository_id = RepoId::new(repo);
    let from = state_of(&state, project)?;
    if from == RepositoryState::Purging {
        return Ok(Settle::Discard(Started::Resumed { repository_id }));
    }
    tx.step(|c| async move {
        sqlx::query("UPDATE forge.repositories SET state = 'purging' WHERE id = $1")
            .bind(repo)
            .execute(c)
            .await
            .map_err(|e| query_error("projects.purge.start", e))
    })
    .await?;
    let details = AuditDetails::RepositoryPurgeStarted {
        project_id: project,
        repository_id,
        from,
    };
    let record = ChangeRecord {
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
    };
    Ok(Settle::Commit(
        record,
        Started::Started {
            repository_id,
            from,
        },
    ))
}

/// The deletion as the project row holds it: deleted_at, purge_after (UTC
/// RFC 3339), deleted_by.
type DeletionRow = (Option<String>, Option<String>, Option<i64>);

/// The project's namespace: id, parent id, full path.
type NamespaceRow = (i64, Option<i64>, String);

/// Phase 3 (module docs): delete the rows of `project`, whose repository
/// is `purging` and whose directory the caller removed (`storage_removed`:
/// it was there). Every exit that does not commit rolls back explicitly.
/// Errors: `FG-SYS-0001` (`not_purging`: phase 1 did not run;
/// `purging_without_deletion`, `project_without_repository`,
/// `project_without_namespace`, `unexpected_dedup`: damaged data), the
/// change's own.
pub async fn finish(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    storage_removed: bool,
) -> Result<Finished, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = finish_steps(&mut tx, project, storage_removed).await;
    tx.settle("project.purged", outcome).await
}

async fn finish_steps(
    tx: &mut ChangeTx,
    project: ProjectId,
    storage_removed: bool,
) -> Result<Settle<Finished>, ForgeError> {
    let row: Option<DeletionRow> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT to_char(deleted_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                 to_char(purge_after AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                 deleted_by FROM forge.projects WHERE id = $1 FOR UPDATE",
            )
            .bind(project.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("projects.purge.finish.lock", e))
        })
        .await?;
    let Some((deleted_at, purge_after, deleted_by)) = row else {
        return Ok(Settle::Discard(Finished::Gone));
    };
    let repo: Option<(i64, String)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT id, state FROM forge.repositories WHERE project_id = $1 FOR UPDATE",
            )
            .bind(project.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("projects.purge.finish.repository", e))
        })
        .await?;
    let Some((repo, state)) = repo else {
        return Err(internal("project_without_repository", project));
    };
    if state_of(&state, project)? != RepositoryState::Purging {
        return Err(internal("not_purging", project).with_context("state", state));
    }
    // Restore refuses a purging repository, so the deletion is still there.
    let (Some(deleted_at), Some(purge_after)) = (deleted_at, purge_after) else {
        return Err(internal("purging_without_deletion", project));
    };
    let namespace: Option<NamespaceRow> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT id, parent_id, full_path FROM forge.namespaces \
                 WHERE kind = 'project' AND owner_id = $1 FOR UPDATE",
            )
            .bind(project.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("projects.purge.finish.namespace", e))
        })
        .await?;
    let Some((namespace_id, parent_id, path)) = namespace else {
        return Err(internal("project_without_namespace", project));
    };
    let path = path.as_str();
    tx.step(|c| lock_path(c, path)).await?;
    let redirects_ended: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "SELECT count(*) FROM forge.namespace_tombstones \
                 WHERE namespace_id = $1 AND kind = 'moved'",
            )
            .bind(namespace_id)
            .fetch_one(c)
            .await
            .map_err(|e| query_error("projects.purge.finish.redirects", e))
        })
        .await?;
    let members_removed = delete_rows(
        tx,
        "DELETE FROM forge.project_members WHERE project_id = $1",
        project.get(),
        "projects.purge.finish.members",
    )
    .await?;
    let protected_refs_removed = delete_rows(
        tx,
        "DELETE FROM forge.protected_refs WHERE project_id = $1",
        project.get(),
        "projects.purge.finish.protected_refs",
    )
    .await?;
    delete_one(
        tx,
        "DELETE FROM forge.repositories WHERE id = $1",
        repo,
        "projects.purge.finish.repository_row",
        project,
    )
    .await?;
    // ON DELETE SET NULL ends the redirects of the `moved` tombstones.
    delete_one(
        tx,
        "DELETE FROM forge.namespaces WHERE id = $1",
        namespace_id,
        "projects.purge.finish.namespace_row",
        project,
    )
    .await?;
    let tombstone: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "INSERT INTO forge.namespace_tombstones (old_path, parent_namespace_id, \
                 namespace_id, kind, owner_at_write, cause_type, cause_id) \
                 VALUES ($1, $2, NULL, 'deleted', $3, 'project', $4) RETURNING id",
            )
            .bind(path)
            .bind(parent_id)
            .bind(deleted_by)
            .bind(project.get())
            .fetch_one(c)
            .await
            .map_err(|e| query_error("projects.purge.finish.tombstone", e))
        })
        .await?;
    delete_one(
        tx,
        "DELETE FROM forge.projects WHERE id = $1",
        project.get(),
        "projects.purge.finish.project_row",
        project,
    )
    .await?;
    let purged = Purged {
        project_id: project,
        repository_id: RepoId::new(repo),
        namespace_id: NamespaceId::new(namespace_id),
        tombstone_id: TombstoneId::new(tombstone),
        redirects_ended,
    };
    let details = AuditDetails::ProjectPurged {
        project_id: project,
        repository_id: purged.repository_id,
        namespace_id: purged.namespace_id,
        deleted_at,
        purge_after,
        deleted_by: deleted_by.map(UserId::new),
        tombstone_id: purged.tombstone_id,
        redirects_ended,
        members_removed,
        protected_refs_removed,
        storage_removed,
    };
    let mut record = super::evented_record(&details, project)?;
    for event in &mut record.events {
        event.dedup_key = Some(DedupKey::new(
            PURGED_DEDUP_PREFIX,
            &[DedupPart::Id(project.get())],
        )?);
    }
    Ok(Settle::Commit(record, Finished::Purged(purged)))
}

/// Run a `DELETE … WHERE <key> = $1` and return how many rows it removed.
async fn delete_rows(
    tx: &mut ChangeTx,
    sql: &'static str,
    key: i64,
    query: &'static str,
) -> Result<i64, ForgeError> {
    let removed = tx
        .step(|c| async move {
            sqlx::query(sql)
                .bind(key)
                .execute(c)
                .await
                .map_err(|e| query_error(query, e))
        })
        .await?
        .rows_affected();
    Ok(i64::try_from(removed).unwrap_or(i64::MAX))
}

/// [`delete_rows`] for a row that must exist (it is locked).
async fn delete_one(
    tx: &mut ChangeTx,
    sql: &'static str,
    key: i64,
    query: &'static str,
    project: ProjectId,
) -> Result<(), ForgeError> {
    match delete_rows(tx, sql, key, query).await? {
        1 => Ok(()),
        rows => Err(internal("unexpected_row_count", project)
            .with_context("query", query)
            .with_context("rows", rows)),
    }
}
