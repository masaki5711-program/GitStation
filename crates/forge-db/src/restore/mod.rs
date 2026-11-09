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
pub const ACTION_PUSH_MIRRORS_DISABLED: &str = "restore.push_mirrors_disabled"; // project
pub const ACTION_INTEGRATION_DISABLED: &str = "restore.integration_disabled"; // integration
pub const ACTION_ASSERTION_REJECTED: &str = "restore.assertion_rejected"; // act_as_assertion

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
    /// Projects whose webhooks or push mirror were disabled (one change each).
    pub projects: i64,
    pub webhooks: i64,
    pub push_mirrors: i64,
    /// Integrations disabled (one change each).
    pub integrations: i64,
    /// Act-as assertions still pending or claimed, rejected (`restore`).
    pub assertions: i64,
}

/// Act-as assertions rejected per change.
const ASSERTION_BATCH: i64 = 500;

/// Revoke every credential the restored database holds (ADR-0019
/// Amendment plan 11 P1, step 1): every token not yet revoked
/// (`revoked_reason = 'restore'`, personal and service-account tokens
/// alike), every web session (`SessionRevokeReason::Restore`), every
/// enabled webhook (`disabled_reason = 'restore'`: its secret may be one
/// rotated away after the backup), every enabled push mirror and every
/// enabled integration (one removed or disabled after the backup must not
/// come back; their audit rows say `restore`), and every act-as assertion
/// still pending or claimed (`reject_reason = 'restore'`). One change per
/// user (the user row locked `FOR UPDATE` like `tokens::issue` and
/// `users::disable` do), per project and per integration, each with its own
/// `restore.*` audit row carrying `begin` (the `forge restore begin`
/// request of this restore). No event: the event feed's epoch is rotated
/// by the same restore. Per unit and re-runnable: a run that stopped
/// half-way is run again.
pub async fn revoke_all_credentials(
    db: &Db,
    ctx: &ChangeContext,
    begin: &str,
) -> Result<Revoked, ForgeError> {
    const USERS: &str = "restore.credential_owners";
    const PROJECTS: &str = "restore.disable_projects";
    const INTEGRATIONS: &str = "restore.integrations";
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
        let (tokens, sessions) = revoke_user(db, ctx, UserId::new(user), begin).await?;
        if tokens + sessions > 0 {
            revoked.users += 1;
            revoked.tokens += tokens;
            revoked.sessions += sessions;
        }
    }
    let projects: Vec<i64> = db
        .within(PROJECTS, async {
            sqlx::query_scalar(
                "SELECT project_id FROM forge.webhooks WHERE enabled \
                 UNION SELECT project_id FROM forge.push_mirrors WHERE enabled ORDER BY 1",
            )
            .fetch_all(pool)
            .await
            .map_err(|e| read_query_error(PROJECTS, e))
        })
        .await?;
    for project in projects {
        let (hooks, mirrors) = disable_project(db, ctx, ProjectId::new(project), begin).await?;
        if hooks + mirrors > 0 {
            revoked.projects += 1;
            revoked.webhooks += hooks;
            revoked.push_mirrors += mirrors;
        }
    }
    let integrations: Vec<i64> = db
        .within(INTEGRATIONS, async {
            sqlx::query_scalar("SELECT id FROM forge.integrations WHERE enabled ORDER BY id")
                .fetch_all(pool)
                .await
                .map_err(|e| read_query_error(INTEGRATIONS, e))
        })
        .await?;
    for id in integrations {
        revoked.integrations += disable_integration(db, ctx, id, begin).await?;
    }
    revoked.assertions = reject_assertions(db, ctx, begin).await?;
    Ok(revoked)
}

async fn revoke_user(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
    begin: &str,
) -> Result<(i64, i64), ForgeError> {
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
            serde_json::json!({
                "revoked_tokens": tokens,
                "revoked_sessions": sessions,
                "begin": begin,
            }),
        );
        Ok(Settle::Commit(record(entry), (tokens, sessions)))
    }
    .await;
    tx.settle(ACTION_CREDENTIALS_REVOKED, outcome).await
}

/// The project's enabled webhooks (`disabled_reason = 'restore'`) and its
/// enabled push mirror, in one change.
async fn disable_project(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    begin: &str,
) -> Result<(i64, i64), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let hooks: Vec<i64> = tx
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
        let mirrors: Vec<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "UPDATE forge.push_mirrors SET enabled = false, updated_at = now() \
                     WHERE project_id = $1 AND enabled RETURNING repo_id",
                )
                .bind(project.get())
                .fetch_all(c)
                .await
                .map_err(|e| query_error("restore.revoke.push_mirrors", e))
            })
            .await?;
        if hooks.is_empty() && mirrors.is_empty() {
            return Ok(Settle::Discard((0, 0)));
        }
        let row = |action, details| {
            audit(
                action,
                "project",
                Some(project.get()),
                Some(project),
                details,
            )
        };
        let mut rec = if hooks.is_empty() {
            record(row(
                ACTION_PUSH_MIRRORS_DISABLED,
                serde_json::json!({"repo_ids": mirrors, "reason": "restore", "begin": begin}),
            ))
        } else {
            record(row(
                ACTION_WEBHOOKS_DISABLED,
                serde_json::json!({"webhook_ids": hooks, "begin": begin}),
            ))
        };
        if !hooks.is_empty() && !mirrors.is_empty() {
            rec.affected.push(row(
                ACTION_PUSH_MIRRORS_DISABLED,
                serde_json::json!({"repo_ids": mirrors, "reason": "restore", "begin": begin}),
            ));
        }
        Ok(Settle::Commit(
            rec,
            (count(hooks.len()), count(mirrors.len())),
        ))
    }
    .await;
    tx.settle(ACTION_WEBHOOKS_DISABLED, outcome).await
}

async fn disable_integration(
    db: &Db,
    ctx: &ChangeContext,
    id: i64,
    begin: &str,
) -> Result<i64, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let n = tx
            .step(|c| async move {
                sqlx::query(
                    "UPDATE forge.integrations SET enabled = false, updated_at = now() \
                     WHERE id = $1 AND enabled",
                )
                .bind(id)
                .execute(c)
                .await
                .map(|r| r.rows_affected())
                .map_err(|e| query_error("restore.revoke.integration", e))
            })
            .await?;
        if n == 0 {
            return Ok(Settle::Discard(0));
        }
        let entry = audit(
            ACTION_INTEGRATION_DISABLED,
            "integration",
            Some(id),
            None,
            serde_json::json!({"reason": "restore", "begin": begin}),
        );
        Ok(Settle::Commit(record(entry), 1))
    }
    .await;
    tx.settle(ACTION_INTEGRATION_DISABLED, outcome).await
}

/// The act-as assertions still pending or claimed: rejected (`restore`), at
/// most [`ASSERTION_BATCH`] per change, one audit row each.
async fn reject_assertions(db: &Db, ctx: &ChangeContext, begin: &str) -> Result<i64, ForgeError> {
    let mut total = 0;
    loop {
        let mut tx = ChangeTx::begin(db, ctx).await?;
        let request = ctx.request_id.as_str().to_owned();
        let outcome = async {
            let ids: Vec<i64> = tx
                .step(|c| async move {
                    sqlx::query_scalar(
                        "UPDATE forge.act_as_assertions SET state = 'rejected', \
                         reject_reason = 'restore', finished_at = now(), \
                         finished_request_id = $1 WHERE id IN (SELECT id FROM \
                         forge.act_as_assertions WHERE state IN ('pending', 'claimed') \
                         ORDER BY id LIMIT $2 FOR UPDATE) RETURNING id",
                    )
                    .bind(&request)
                    .bind(ASSERTION_BATCH)
                    .fetch_all(c)
                    .await
                    .map_err(|e| query_error("restore.revoke.assertions", e))
                })
                .await?;
            let Some((first, rest)) = ids.split_first() else {
                return Ok(Settle::Discard(0));
            };
            let row = |id: i64| {
                audit(
                    ACTION_ASSERTION_REJECTED,
                    "act_as_assertion",
                    Some(id),
                    None,
                    serde_json::json!({"reason": "restore", "begin": begin}),
                )
            };
            let mut rec = record(row(*first));
            rec.affected = rest.iter().map(|id| row(*id)).collect();
            Ok(Settle::Commit(rec, count(ids.len())))
        }
        .await;
        let n = tx.settle(ACTION_ASSERTION_REJECTED, outcome).await?;
        total += n;
        if n < ASSERTION_BATCH {
            return Ok(total);
        }
    }
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
    begin: &str,
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
            serde_json::json!({"user_ids": ids, "reason": reason, "begin": begin}),
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
/// `restore.admin_flags_removed` row of the newest `restore.finished`
/// (same `begin`) lists, and whom no audit row outside a restore has
/// touched since (the runbook's step after the operator checked the
/// person; ADR-0024 #44). Anyone else is refused (`FG-AUTH-0009
/// reason=not_removed_by_restore`), as is a disabled or non-human user
/// (`reason=not_active_human`). One `user.admin_regranted` row: a second
/// regrant of the same user is refused (the first one's row is newer).
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
        // Listed by the newest finished restore (its `begin`), and nothing
        // changed the user since outside that restore (a regrant, a disable,
        // a rename: the operator's check is then out of date).
        let listed: bool = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "WITH f AS (SELECT details->>'begin' AS begin FROM forge.audit_log \
                       WHERE action = $2 AND resource_type = $4 ORDER BY id DESC LIMIT 1), \
                     r AS (SELECT a.id, a.details FROM forge.audit_log a, f \
                       WHERE a.action = $3 AND a.resource_type = $4 \
                         AND a.details->>'begin' = f.begin ORDER BY a.id DESC LIMIT 1) \
                     SELECT COALESCE((SELECT r.details->'user_ids' @> to_jsonb(ARRAY[$1::bigint]) \
                       AND NOT EXISTS (SELECT 1 FROM forge.audit_log l \
                         WHERE l.resource_type = 'user' AND l.resource_id = $1 \
                           AND l.id > r.id AND l.action NOT LIKE 'restore.%') \
                     FROM r), false)",
                )
                .bind(id)
                .bind(ACTION_FINISHED)
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

/// The users the newest `restore.admin_flags_removed` row of restore
/// `begin` lists (a re-run of `finish` removes nothing new but reports
/// these), and whether an earlier `finish` of the same restore wrote any
/// `restore.*` row (`rerun`).
pub async fn earlier_run(db: &Db, begin: &str) -> Result<(bool, Vec<AdminRemoved>), ForgeError> {
    const QUERY: &str = "restore.earlier_run";
    let pool = db.pool().executor();
    db.within(QUERY, async {
        let q = |e| read_query_error(QUERY, e);
        let rerun: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.audit_log WHERE action LIKE 'restore.%' \
             AND details->>'begin' = $1)",
        )
        .bind(begin)
        .fetch_one(pool)
        .await
        .map_err(q)?;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT u.id, u.username FROM forge.users u WHERE u.id IN (SELECT \
             jsonb_array_elements_text(details->'user_ids')::bigint FROM (SELECT details \
             FROM forge.audit_log WHERE action = $2 AND resource_type = $3 \
             AND details->>'begin' = $1 ORDER BY id DESC LIMIT 1) r) ORDER BY u.id",
        )
        .bind(begin)
        .bind(ACTION_ADMIN_FLAGS_REMOVED)
        .bind(RESOURCE_RESTORE)
        .fetch_all(pool)
        .await
        .map_err(q)?;
        Ok((
            rerun,
            rows.into_iter()
                .map(|(id, username)| AdminRemoved {
                    user: UserId::new(id),
                    username,
                })
                .collect(),
        ))
    })
    .await
}

/// The number of active human administrators (a restore that leaves none
/// says so; `forge admin bootstrap-admin` is then the way back).
pub async fn active_admins(db: &Db) -> Result<i64, ForgeError> {
    const QUERY: &str = "restore.active_admins";
    let pool = db.pool().executor();
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT count(*) FROM forge.users WHERE is_admin AND state = 'active' \
             AND kind = 'human'",
        )
        .fetch_one(pool)
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// The advisory lock `repo_maint:<repo_id>` (forge-db
/// `review::REPO_MAINT_LOCK_PREFIX`, reserved by O3 for O1): the backup's
/// copy of a repository and the repository's maintenance never overlap. A
/// **session** lock on a connection of its own (detached from the pool: a
/// transaction would be ended by `idle_in_transaction_session_timeout`
/// during a long copy). Dropping it closes the connection, which releases
/// the lock; [`RepoMaintLock::release`] unlocks and closes explicitly.
pub struct RepoMaintLock {
    conn: Option<sqlx::PgConnection>,
    repo: RepoId,
}

const REPO_MAINT_KEY: &str = "SELECT hashtextextended($1 || $2::text, 0)";

async fn repo_maint_conn(db: &Db, query: &'static str) -> Result<sqlx::PgConnection, ForgeError> {
    let conn = db
        .write_pool()
        .acquire()
        .await
        .map_err(|e| query_error(query, e))?;
    Ok(conn.detach())
}

async fn try_take(
    conn: &mut sqlx::PgConnection,
    repo: RepoId,
    query: &'static str,
) -> Result<bool, ForgeError> {
    let key: i64 = sqlx::query_scalar(REPO_MAINT_KEY)
        .bind(crate::review::merges::REPO_MAINT_LOCK_PREFIX)
        .bind(repo.get())
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| query_error(query, e))?;
    sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(key)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| query_error(query, e).with_context("repo_id", repo.get()))
}

impl RepoMaintLock {
    /// Take it now or not at all (the maintenance: `None` while a backup
    /// copies the repository).
    pub async fn try_lock(db: &Db, repo: RepoId) -> Result<Option<Self>, ForgeError> {
        const QUERY: &str = "restore.repo_maint_try_lock";
        db.within(QUERY, async {
            let mut conn = repo_maint_conn(db, QUERY).await?;
            if try_take(&mut conn, repo, QUERY).await? {
                Ok(Some(Self {
                    conn: Some(conn),
                    repo,
                }))
            } else {
                close(conn, repo).await;
                Ok(None)
            }
        })
        .await
    }

    /// Wait for it (polling every 500 ms) until `wait` has passed (the
    /// backup: a maintenance run ends within its job timeout). `None` when it
    /// was still held then.
    pub async fn lock(
        db: &Db,
        repo: RepoId,
        wait: std::time::Duration,
    ) -> Result<Option<Self>, ForgeError> {
        const QUERY: &str = "restore.repo_maint_lock";
        let until = tokio::time::Instant::now() + wait;
        let mut conn = db.within(QUERY, repo_maint_conn(db, QUERY)).await?;
        loop {
            match db.within(QUERY, try_take(&mut conn, repo, QUERY)).await {
                Ok(true) => {
                    return Ok(Some(Self {
                        conn: Some(conn),
                        repo,
                    }))
                }
                Ok(false) if tokio::time::Instant::now() < until => {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Ok(false) => {
                    close(conn, repo).await;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The lock still stands (its connection answers): `FG-DB-0002
    /// reason=repo_maint_lock_lost` otherwise.
    pub async fn alive(&mut self) -> Result<(), ForgeError> {
        let lost = || {
            ForgeError::new(ErrorCode::DbQueryFailed)
                .with_context("reason", "repo_maint_lock_lost")
                .with_context("repo_id", self.repo.get())
        };
        let Some(conn) = self.conn.as_mut() else {
            return Err(lost());
        };
        let held = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' \
                 AND pid = pg_backend_pid() AND granted)",
            )
            .fetch_one(&mut *conn),
        )
        .await;
        match held {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) | Ok(Err(_)) | Err(_) => Err(lost()),
        }
    }

    /// Unlock and close the connection.
    pub async fn release(mut self) {
        if let Some(conn) = self.conn.take() {
            close(conn, self.repo).await;
        }
    }
}

/// Close a lock connection (closing releases every session lock it holds);
/// a failed close is said, the server ends the session anyway.
async fn close(conn: sqlx::PgConnection, repo: RepoId) {
    use sqlx::Connection as _;
    match tokio::time::timeout(std::time::Duration::from_secs(10), conn.close()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(
            target: "forge_db",
            repo_id = repo.get(),
            error = %e,
            "forge.restore.repo_maint_lock_close_failed"
        ),
        Err(_) => tracing::warn!(
            target: "forge_db",
            repo_id = repo.get(),
            "forge.restore.repo_maint_lock_close_timeout"
        ),
    }
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
