//! Restore support (M3 O1, ADR-0019 Amendment plan 11 P1, runbook
//! backup-restore). The operator commands are `forge restore …` (forge-cli,
//! actor `system`, `actor_label = forge-cli:<OS account>`); they never run
//! from HTTP. This module holds their SQL:
//!
//! - reads for `forge restore verify`: the repository rows
//!   ([`repositories`]) and the version refs the database expects
//!   ([`expected_versions`]);
//! - the database clock ([`clock`]) that dates a backup's database step and
//!   a state copy on the same clock;
//! - [`revoke_all_credentials`]: every token (personal and service
//!   account), every web session and every enabled webhook issued before
//!   the restore is revoked / disabled with reason `restore`, one change per
//!   user or project, each with its own audit row;
//! - the authorization state copy ([`state`]): what `forge worker` writes
//!   outside the backup, and its narrowing-only re-application;
//! - [`remove_all_admin_flags`] (no usable state copy) and
//!   [`regrant_admin`] (the runbook's way back for the users it listed);
//! - [`record_finished`]: the `restore.finished` audit row.
//!
//! Every change is idempotent: a `finish` that stopped half-way runs again.

use std::collections::BTreeSet;

use forge_core::ids::{ProjectId, RepoId, UserId};
use forge_core::SessionRevokeReason;
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::{query_error, read_query_error, Db};

pub mod state;

/// The audit actions of a restore (resource in brackets).
pub const ACTION_CREDENTIALS_REVOKED: &str = "restore.credentials_revoked"; // user
pub const ACTION_WEBHOOKS_DISABLED: &str = "restore.webhooks_disabled"; // project
pub const ACTION_ADMIN_FLAGS_REMOVED: &str = "restore.admin_flags_removed"; // restore
pub const ACTION_FINISHED: &str = "restore.finished"; // restore
pub const ACTION_ADMIN_REGRANTED: &str = "user.admin_regranted"; // user

/// The resource type of the instance-wide restore rows.
pub const RESOURCE_RESTORE: &str = "restore";

/// A repository row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepositoryRow {
    pub repo: RepoId,
    pub project: ProjectId,
    pub ready: bool,
}

/// Every repository row (provisioning ones too: their directory may exist).
pub async fn repositories(db: &Db) -> Result<Vec<RepositoryRow>, ForgeError> {
    const QUERY: &str = "restore.repositories";
    let pool = db.pool().executor();
    db.within(QUERY, async {
        let rows: Vec<(i64, i64, String)> =
            sqlx::query_as("SELECT id, project_id, state FROM forge.repositories ORDER BY id")
                .fetch_all(pool)
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
        Ok(rows
            .into_iter()
            .map(|(id, project, state)| RepositoryRow {
                repo: RepoId::new(id),
                project: ProjectId::new(project),
                ready: state == "ready",
            })
            .collect())
    })
    .await
}

/// A merge request version as the database knows it: its ref
/// `refs/merge-requests/<iid>/v<version_no>` must hold `head_sha`, unless
/// the version's refs were purged (secret removal), when it must be gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedVersion {
    pub repo: RepoId,
    pub iid: i64,
    pub version_no: i64,
    pub head_sha: String,
    pub purged: bool,
}

impl ExpectedVersion {
    pub fn ref_name(&self) -> String {
        format!("refs/merge-requests/{}/v{}", self.iid, self.version_no)
    }
}

/// Every version of every merge request, with its repository (a project
/// has one), in repository / iid / version order.
pub async fn expected_versions(db: &Db) -> Result<Vec<ExpectedVersion>, ForgeError> {
    const QUERY: &str = "restore.expected_versions";
    let pool = db.pool().executor();
    db.within(QUERY, async {
        let rows: Vec<(i64, i64, i64, String, bool)> = sqlx::query_as(
            "SELECT r.id, m.iid, v.version_no, v.head_sha, v.refs_purged_at IS NOT NULL \
             FROM forge.merge_request_versions v \
             JOIN forge.merge_requests m ON m.id = v.merge_request_id \
             JOIN forge.repositories r ON r.project_id = v.project_id \
             ORDER BY r.id, m.iid, v.version_no",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(rows
            .into_iter()
            .map(
                |(repo, iid, version_no, head_sha, purged)| ExpectedVersion {
                    repo: RepoId::new(repo),
                    iid,
                    version_no,
                    head_sha,
                    purged,
                },
            )
            .collect())
    })
    .await
}

/// The iids of every merge request per repository (a version ref of a
/// known merge request whose version row is missing is still "unknown").
pub async fn merge_request_iids(db: &Db) -> Result<BTreeSet<(i64, i64)>, ForgeError> {
    const QUERY: &str = "restore.merge_request_iids";
    let pool = db.pool().executor();
    db.within(QUERY, async {
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT r.id, m.iid FROM forge.merge_requests m \
             JOIN forge.repositories r ON r.project_id = m.project_id",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(rows.into_iter().collect())
    })
    .await
}

/// The database clock: microseconds since the Unix epoch and the UTC
/// generation name `YYYYMMDDTHHMMSSZ`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbClock {
    pub micros: i64,
    pub generation: String,
    pub rfc3339: String,
}

pub async fn clock(db: &Db) -> Result<DbClock, ForgeError> {
    const QUERY: &str = "restore.clock";
    let pool = db.pool().executor();
    db.within(QUERY, async {
        let (micros, generation, rfc3339): (i64, String, String) = sqlx::query_as(
            "SELECT (extract(epoch FROM clock_timestamp()) * 1000000)::bigint, \
             to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYYMMDD\"T\"HH24MISS\"Z\"'), \
             to_char(clock_timestamp() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
        )
        .fetch_one(pool)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(DbClock {
            micros,
            generation,
            rfc3339,
        })
    })
    .await
}

fn audit(
    action: &'static str,
    resource_type: &'static str,
    resource_id: Option<i64>,
    project: Option<ProjectId>,
    details: serde_json::Value,
) -> AuditEntry {
    AuditEntry {
        action,
        outcome: Outcome::Succeeded,
        resource_type,
        resource_id,
        project_id: project,
        error_code: None,
        details,
    }
}

pub(crate) fn record(entry: AuditEntry) -> ChangeRecord {
    ChangeRecord {
        audit: entry,
        affected: Vec::new(),
        events: Vec::new(),
    }
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// What [`revoke_all_credentials`] revoked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Revoked {
    /// Users whose tokens or sessions were revoked (one change each).
    pub users: i64,
    pub tokens: i64,
    pub sessions: i64,
    /// Projects whose webhooks were disabled (one change each).
    pub projects: i64,
    pub webhooks: i64,
}

/// Revoke every credential the restored database holds (ADR-0019
/// Amendment plan 11 P1, step 1): every token not yet revoked
/// (`revoked_reason = 'restore'`, personal and service-account tokens
/// alike), every web session (`SessionRevokeReason::Restore`) and every
/// enabled webhook (`disabled_reason = 'restore'`: its secret may be one
/// rotated away after the backup). One change per user (the user row is
/// locked `FOR UPDATE` like `tokens::issue` and `users::disable` do) and per
/// project, each with a `restore.credentials_revoked` /
/// `restore.webhooks_disabled` audit row. No event: the event feed's epoch
/// is rotated by the same restore. Idempotent.
pub async fn revoke_all_credentials(db: &Db, ctx: &ChangeContext) -> Result<Revoked, ForgeError> {
    const USERS: &str = "restore.credential_owners";
    const PROJECTS: &str = "restore.webhook_projects";
    let pool = db.pool().executor();
    let users: Vec<i64> = db
        .within(USERS, async {
            sqlx::query_scalar(
                "SELECT user_id FROM forge.tokens WHERE revoked_at IS NULL \
                 UNION SELECT user_id FROM forge.web_sessions WHERE revoked_at IS NULL \
                 ORDER BY 1",
            )
            .fetch_all(pool)
            .await
            .map_err(|e| read_query_error(USERS, e))
        })
        .await?;
    let mut revoked = Revoked::default();
    for user in users {
        let (tokens, sessions) = revoke_user(db, ctx, UserId::new(user)).await?;
        if tokens + sessions > 0 {
            revoked.users += 1;
            revoked.tokens += tokens;
            revoked.sessions += sessions;
        }
    }
    let projects: Vec<i64> = db
        .within(PROJECTS, async {
            sqlx::query_scalar(
                "SELECT DISTINCT project_id FROM forge.webhooks WHERE enabled ORDER BY 1",
            )
            .fetch_all(pool)
            .await
            .map_err(|e| read_query_error(PROJECTS, e))
        })
        .await?;
    for project in projects {
        let disabled = disable_webhooks(db, ctx, ProjectId::new(project)).await?;
        if disabled > 0 {
            revoked.projects += 1;
            revoked.webhooks += disabled;
        }
    }
    Ok(revoked)
}

async fn revoke_user(db: &Db, ctx: &ChangeContext, user: UserId) -> Result<(i64, i64), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        tx.step(|c| async move {
            sqlx::query("SELECT 1 FROM forge.users WHERE id = $1 FOR UPDATE")
                .bind(user.get())
                .execute(c)
                .await
                .map_err(|e| query_error("restore.revoke.lock_user", e))
        })
        .await?;
        let tokens: Vec<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "UPDATE forge.tokens SET revoked_at = now(), revoked_reason = 'restore' \
                     WHERE user_id = $1 AND revoked_at IS NULL RETURNING id",
                )
                .bind(user.get())
                .fetch_all(c)
                .await
                .map_err(|e| query_error("restore.revoke.tokens", e))
            })
            .await?;
        let sessions = tx
            .step(|c| {
                crate::sessions::revoke_active(
                    c,
                    user,
                    SessionRevokeReason::Restore,
                    "restore.revoke.sessions",
                )
            })
            .await?;
        let tokens = count(tokens.len());
        if tokens + sessions == 0 {
            return Ok(Settle::Discard((0, 0)));
        }
        let entry = audit(
            ACTION_CREDENTIALS_REVOKED,
            "user",
            Some(user.get()),
            None,
            serde_json::json!({"revoked_tokens": tokens, "revoked_sessions": sessions}),
        );
        Ok(Settle::Commit(record(entry), (tokens, sessions)))
    }
    .await;
    tx.settle(ACTION_CREDENTIALS_REVOKED, outcome).await
}

async fn disable_webhooks(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
) -> Result<i64, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let ids: Vec<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "UPDATE forge.webhooks SET enabled = false, disabled_reason = 'restore', \
                     updated_at = now() WHERE project_id = $1 AND enabled RETURNING id",
                )
                .bind(project.get())
                .fetch_all(c)
                .await
                .map_err(|e| query_error("restore.revoke.webhooks", e))
            })
            .await?;
        if ids.is_empty() {
            return Ok(Settle::Discard(0));
        }
        let entry = audit(
            ACTION_WEBHOOKS_DISABLED,
            "project",
            Some(project.get()),
            Some(project),
            serde_json::json!({"webhook_ids": ids}),
        );
        Ok(Settle::Commit(record(entry), count(ids.len())))
    }
    .await;
    tx.settle(ACTION_WEBHOOKS_DISABLED, outcome).await
}

/// A user whose administrator flag a restore removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminRemoved {
    pub user: UserId,
    pub username: String,
}

/// Without a usable state copy (ADR-0019 Amendment plan 11 P1, step 2):
/// remove the administrator flag of every user, in one change, and return
/// them for the operator to check and grant again ([`regrant_admin`]).
/// One `restore.admin_flags_removed` row (resource `restore`, the user
/// ids in its details) plus one `user` row per user. Nothing to remove:
/// nothing written, an empty list.
pub async fn remove_all_admin_flags(
    db: &Db,
    ctx: &ChangeContext,
    reason: &'static str,
) -> Result<Vec<AdminRemoved>, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        tx.step(crate::users::admin::lock_admin_set).await?;
        let rows: Vec<(i64, String)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "UPDATE forge.users SET is_admin = false, updated_at = now() \
                     WHERE is_admin RETURNING id, username",
                )
                .fetch_all(c)
                .await
                .map_err(|e| query_error("restore.remove_admin_flags", e))
            })
            .await?;
        if rows.is_empty() {
            return Ok(Settle::Discard(Vec::new()));
        }
        let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        let mut rec = record(audit(
            ACTION_ADMIN_FLAGS_REMOVED,
            RESOURCE_RESTORE,
            None,
            None,
            serde_json::json!({"user_ids": ids, "reason": reason}),
        ));
        for id in &ids {
            rec.affected.push(audit(
                ACTION_ADMIN_FLAGS_REMOVED,
                "user",
                Some(*id),
                None,
                serde_json::json!({"reason": reason}),
            ));
        }
        let removed = rows
            .into_iter()
            .map(|(id, username)| AdminRemoved {
                user: UserId::new(id),
                username,
            })
            .collect();
        Ok(Settle::Commit(rec, removed))
    }
    .await;
    tx.settle(ACTION_ADMIN_FLAGS_REMOVED, outcome).await
}

/// What [`regrant_admin`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regranted {
    Granted(UserId),
    /// The user is an administrator already: nothing written.
    AlreadyAdmin(UserId),
}

/// Give the administrator flag back to `username`, a user whose flag the
/// latest `restore.admin_flags_removed` row lists (the runbook's step after
/// the operator checked the person; ADR-0024 #44). Anyone else is refused
/// (`FG-AUTH-0009 reason=not_removed_by_restore`), as is a disabled or
/// non-human user (`reason=not_active_human`). One `user.admin_regranted`
/// row.
pub async fn regrant_admin(
    db: &Db,
    ctx: &ChangeContext,
    username: &str,
) -> Result<Regranted, ForgeError> {
    let refused = |reason: &'static str| {
        ForgeError::new(ErrorCode::AdministrationConflict).with_context("reason", reason)
    };
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let name = username.to_owned();
    let outcome = async {
        tx.step(crate::users::admin::lock_admin_set).await?;
        let row: Option<(i64, String, String, bool)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT id, kind, state, is_admin FROM forge.users WHERE username = $1 \
                     FOR UPDATE",
                )
                .bind(&name)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("restore.regrant.lock_user", e))
            })
            .await?;
        let Some((id, kind, state, is_admin)) = row else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "user")
                .with_context("reason", "absent"));
        };
        let user = UserId::new(id);
        if is_admin {
            return Ok(Settle::Discard(Regranted::AlreadyAdmin(user)));
        }
        if kind != "human" || state != "active" {
            return Err(refused("not_active_human"));
        }
        let listed: bool = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT COALESCE((SELECT details->'user_ids' @> to_jsonb(ARRAY[$1::bigint]) \
                     FROM forge.audit_log WHERE action = $2 AND resource_type = $3 \
                     ORDER BY id DESC LIMIT 1), false)",
                )
                .bind(id)
                .bind(ACTION_ADMIN_FLAGS_REMOVED)
                .bind(RESOURCE_RESTORE)
                .fetch_one(c)
                .await
                .map_err(|e| query_error("restore.regrant.listed", e))
            })
            .await?;
        if !listed {
            return Err(refused("not_removed_by_restore"));
        }
        tx.step(|c| async move {
            sqlx::query("UPDATE forge.users SET is_admin = true, updated_at = now() WHERE id = $1")
                .bind(id)
                .execute(c)
                .await
                .map_err(|e| query_error("restore.regrant.update", e))
        })
        .await?;
        let entry = audit(
            ACTION_ADMIN_REGRANTED,
            "user",
            Some(id),
            None,
            serde_json::json!({}),
        );
        Ok(Settle::Commit(record(entry), Regranted::Granted(user)))
    }
    .await;
    tx.settle(ACTION_ADMIN_REGRANTED, outcome).await
}

/// The `restore.finished` audit row (resource `restore`): `details` are
/// the counts and the generation, never a secret.
pub async fn record_finished(
    db: &Db,
    ctx: &ChangeContext,
    details: serde_json::Value,
) -> Result<(), ForgeError> {
    let tx = ChangeTx::begin(db, ctx).await?;
    let entry = audit(ACTION_FINISHED, RESOURCE_RESTORE, None, None, details);
    tx.settle(ACTION_FINISHED, Ok(Settle::Commit(record(entry), ())))
        .await
}

#[cfg(test)]
mod tests;
