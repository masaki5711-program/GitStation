//! Deployments (M4 C11, plan 14 §4.1, ADR-0031; migration
//! 20261214100000): the typed commands and reads of
//! `forge.deploy_environments`, `forge.deploy_targets` and
//! `forge.deployments`.
//!
//! - [`put_environment_decided`] / [`put_target_decided`]: a person's
//!   change, decided again on the locked project and caller (`decide`),
//!   audited on the project in the same transaction (the tables' audit
//!   triggers refuse a write without it).
//! - [`request_decided`] / [`auto_request`]: a deployment requested by a
//!   person or by the system (an environment's `auto_deploy_ref`); the
//!   freeze windows are checked on the database clock, an audit row
//!   `deployment.requested` is written with it.
//! - [`approve_decided`]: a person's approval of a `pending_approval`
//!   deployment, under the row's lock (`FG-DEPLOY-0010` otherwise), with
//!   `approval_expires_at` from the environment's window.
//! - [`claim_next`] / [`finish`]: the worker's side. A claim takes the
//!   oldest `queued` deployment of an environment that runs none, under
//!   the environment's lock, and checks again — at execution time — the
//!   freeze windows and the approval (`blocked` with `FG-DEPLOY-0002` /
//!   `FG-DEPLOY-0001`); otherwise it records the previous version and
//!   marks the deployment `running`.
//! - [`block_unapproved`]: requests that waited for an approval longer
//!   than their environment's window end `blocked` (`FG-DEPLOY-0001`).

use forge_core::deploy::{
    DeployChange, DeployTier, DeploymentStatus, HostKey, MAX_FREEZE_WINDOWS,
    MAX_TARGETS_PER_ENVIRONMENT,
};
use forge_core::{AuditDetails, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::pipelines::decided_facts;
use crate::projects::ProjectUpdateFacts;
use crate::{query_error, read_query_error, Db};

#[cfg(test)]
mod tests;

/// The audit resource of every change here (tables.toml).
pub const RESOURCE: &str = "project";
const TS: &str = "YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"";

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

fn invalid(param: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("param", param)
        .with_context("reason", reason)
}

fn tier_of(raw: &str) -> Result<DeployTier, ForgeError> {
    DeployTier::parse(raw).ok_or_else(|| internal("stored_tier_invalid").with_context("tier", raw))
}

fn status_of(raw: &str) -> Result<DeploymentStatus, ForgeError> {
    DeploymentStatus::parse(raw)
        .ok_or_else(|| internal("stored_status_invalid").with_context("status", raw))
}

fn record(change: DeployChange) -> ChangeRecord {
    let project = change.project();
    let details = AuditDetails::Deploy(change);
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

fn caller_of(ctx: &ChangeContext) -> Result<i64, ForgeError> {
    ctx.actor
        .user()
        .ok_or_else(|| internal("deploy_change_without_user"))
}

// ---------------------------------------------------------------- environments

/// An environment as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEnvironment {
    pub id: i64,
    pub project_id: i64,
    pub tier: DeployTier,
    pub requires_approval: bool,
    pub approval_ttl_minutes: i32,
    pub auto_rollback: bool,
    pub auto_deploy_ref: Option<String>,
    /// `<start>/<end>` in UTC (`YYYY-MM-DDTHH:MM:SSZ`), by start.
    pub freeze_windows: Vec<String>,
    /// `now()` is inside a freeze window.
    pub frozen_now: bool,
    pub updated_by: i64,
    pub updated_at: String,
}

/// What a Maintainer sets (validated by the caller: tiers, prod rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentInput {
    pub tier: DeployTier,
    pub requires_approval: bool,
    pub approval_ttl_minutes: i32,
    pub auto_rollback: bool,
    pub auto_deploy_ref: Option<String>,
    /// `(start, end)` timestamps with a time zone, each window `[start, end)`.
    pub freeze_windows: Vec<(String, String)>,
}

type EnvRow = (
    i64,
    i64,
    String,
    bool,
    i32,
    bool,
    Option<String>,
    Vec<String>,
    bool,
    i64,
    String,
);

fn env_columns(alias: &str) -> String {
    format!(
        "{a}.id, {a}.project_id, {a}.tier, {a}.requires_approval, {a}.approval_ttl_minutes, \
         {a}.auto_rollback, {a}.auto_deploy_ref, \
         ARRAY(SELECT to_char(lower(w) AT TIME ZONE 'UTC', '{TS}') || '/' || \
               to_char(upper(w) AT TIME ZONE 'UTC', '{TS}') \
               FROM unnest({a}.freeze_windows) AS w ORDER BY lower(w)), \
         {a}.freeze_windows @> now(), {a}.updated_by, \
         to_char({a}.updated_at AT TIME ZONE 'UTC', '{TS}')",
        a = alias
    )
}

fn env_of(r: EnvRow) -> Result<StoredEnvironment, ForgeError> {
    Ok(StoredEnvironment {
        id: r.0,
        project_id: r.1,
        tier: tier_of(&r.2)?,
        requires_approval: r.3,
        approval_ttl_minutes: r.4,
        auto_rollback: r.5,
        auto_deploy_ref: r.6,
        freeze_windows: r.7,
        frozen_now: r.8,
        updated_by: r.9,
        updated_at: r.10,
    })
}

/// The environments of `project`, by tier (the read pool).
pub async fn list_environments(
    db: &Db,
    project: ProjectId,
) -> Result<Vec<StoredEnvironment>, ForgeError> {
    const QUERY: &str = "deploy.list_environments";
    let rows: Vec<EnvRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.deploy_environments e WHERE e.project_id = $1 \
                 ORDER BY CASE e.tier WHEN 'dev' THEN 1 WHEN 'stg' THEN 2 ELSE 3 END",
                env_columns("e")
            ))
            .bind(project.get())
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(env_of).collect()
}

/// Create or replace the environment `input.tier` of `project`, decided
/// again on the locked project and caller; `true` with it when created.
/// Errors (nothing written): `decide`'s, `FG-WEB-0006 param=freeze_windows`
/// (`too_many`, `invalid_timestamp`, `empty_window`), the database's.
pub async fn put_environment_decided(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    input: &EnvironmentInput,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<(), ForgeError>,
) -> Result<(StoredEnvironment, bool), ForgeError> {
    const QUERY: &str = "deploy.put_environment";
    let user = caller_of(ctx)?;
    if input.freeze_windows.len() > MAX_FREEZE_WINDOWS {
        return Err(invalid("freeze_windows", "too_many"));
    }
    let starts: Vec<&str> = input.freeze_windows.iter().map(|w| w.0.as_str()).collect();
    let ends: Vec<&str> = input.freeze_windows.iter().map(|w| w.1.as_str()).collect();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        decide(&decided_facts(&mut tx, project, UserId::new(user)).await?)?;
        // PostgreSQL 16 `pg_input_is_valid`: a bad timestamp is the
        // caller's 400, never a database error.
        let (bad, empty): (i64, i64) = tx
            .step(|c| {
                let (starts, ends) = (starts.clone(), ends.clone()); // clone: the step owns its binds
                async move {
                    sqlx::query_as(
                        "SELECT count(*) FILTER (WHERE NOT (pg_input_is_valid(w.s, 'timestamptz') \
                                AND pg_input_is_valid(w.e, 'timestamptz'))), \
                         count(*) FILTER (WHERE CASE WHEN pg_input_is_valid(w.s, 'timestamptz') \
                                AND pg_input_is_valid(w.e, 'timestamptz') \
                                THEN w.s::timestamptz >= w.e::timestamptz ELSE false END) \
                         FROM unnest($1::text[], $2::text[]) AS w(s, e)",
                    )
                    .bind(starts)
                    .bind(ends)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
                }
            })
            .await?;
        if bad > 0 {
            return Err(invalid("freeze_windows", "invalid_timestamp"));
        }
        if empty > 0 {
            return Err(invalid("freeze_windows", "empty_window"));
        }
        let existed: bool = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM forge.deploy_environments \
                     WHERE project_id = $1 AND tier = $2)",
                )
                .bind(project.get())
                .bind(input.tier.as_str())
                .fetch_one(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let sql = format!(
            "WITH up AS (INSERT INTO forge.deploy_environments AS t (project_id, tier, \
               requires_approval, approval_ttl_minutes, auto_rollback, auto_deploy_ref, \
               freeze_windows, created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, \
               (SELECT coalesce(range_agg(tstzrange(w.s::timestamptz, w.e::timestamptz, '[)')), \
                 '{{}}'::tstzmultirange) FROM unnest($7::text[], $8::text[]) AS w(s, e)), $9, $9) \
             ON CONFLICT (project_id, tier) DO UPDATE SET \
               requires_approval = EXCLUDED.requires_approval, \
               approval_ttl_minutes = EXCLUDED.approval_ttl_minutes, \
               auto_rollback = EXCLUDED.auto_rollback, auto_deploy_ref = EXCLUDED.auto_deploy_ref, \
               freeze_windows = EXCLUDED.freeze_windows, updated_by = EXCLUDED.updated_by, \
               updated_at = now() \
             RETURNING t.*) SELECT {} FROM up e",
            env_columns("e")
        );
        let row: EnvRow = tx
            .step(|c| {
                let (starts, ends) = (starts.clone(), ends.clone()); // clone: as above
                async move {
                    sqlx::query_as(&sql)
                        .bind(project.get())
                        .bind(input.tier.as_str())
                        .bind(input.requires_approval)
                        .bind(input.approval_ttl_minutes)
                        .bind(input.auto_rollback)
                        .bind(input.auto_deploy_ref.as_deref())
                        .bind(starts)
                        .bind(ends)
                        .bind(user)
                        .fetch_one(c)
                        .await
                        .map_err(|e| query_error(QUERY, e))
                }
            })
            .await?;
        let stored = env_of(row)?;
        let change = DeployChange::EnvironmentSet {
            project,
            tier: stored.tier,
            created: !existed,
            requires_approval: stored.requires_approval,
            auto_rollback: stored.auto_rollback,
            approval_ttl_minutes: stored.approval_ttl_minutes,
            freeze_windows: stored.freeze_windows.len(),
            auto_deploy_ref: stored.auto_deploy_ref.clone(), // clone: the audit row owns it
        };
        Ok(Settle::Commit(record(change), (stored, !existed)))
    }
    .await;
    tx.settle("deploy_environment.set", outcome).await
}

// ---------------------------------------------------------------- targets

/// A target as read (its pinned key is public).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTarget {
    pub id: i64,
    pub tier: DeployTier,
    pub name: String,
    pub host: String,
    pub port: i32,
    pub login_user: String,
    pub host_key_type: String,
    pub host_key: String,
    pub host_key_fingerprint: String,
    pub script: String,
    pub enabled: bool,
    pub updated_at: String,
}

/// What an administrator sets (validated by the caller).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetInput {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub login_user: String,
    pub host_key: HostKey,
    pub script: String,
    pub enabled: bool,
}

type TargetRow = (
    i64,
    String,
    String,
    String,
    i32,
    String,
    String,
    String,
    String,
    String,
    bool,
    String,
);

const TARGET_COLUMNS: &str = "t.id, e.tier, t.name, t.host, t.port, t.login_user, \
     t.host_key_type, t.host_key, t.host_key_fingerprint, t.script, t.enabled, \
     to_char(t.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')";

/// A target row with its project in front (`all_targets`).
type ProjectTargetRow = (
    i64,
    i64,
    String,
    String,
    String,
    i32,
    String,
    String,
    String,
    String,
    String,
    bool,
    String,
);

fn target_of(r: TargetRow) -> Result<StoredTarget, ForgeError> {
    Ok(StoredTarget {
        id: r.0,
        tier: tier_of(&r.1)?,
        name: r.2,
        host: r.3,
        port: r.4,
        login_user: r.5,
        host_key_type: r.6,
        host_key: r.7,
        host_key_fingerprint: r.8,
        script: r.9,
        enabled: r.10,
        updated_at: r.11,
    })
}

/// The targets of `project`, by tier and name (the read pool).
pub async fn list_targets(db: &Db, project: ProjectId) -> Result<Vec<StoredTarget>, ForgeError> {
    const QUERY: &str = "deploy.list_targets";
    let rows: Vec<TargetRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {TARGET_COLUMNS} FROM forge.deploy_targets t \
                 JOIN forge.deploy_environments e ON e.id = t.environment_id \
                 WHERE t.project_id = $1 ORDER BY e.tier, t.name"
            ))
            .bind(project.get())
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(target_of).collect()
}

/// Every target of the instance with its project (`forge doctor`
/// `deploy.known_hosts`; the read pool).
pub async fn all_targets(db: &Db) -> Result<Vec<(i64, StoredTarget)>, ForgeError> {
    const QUERY: &str = "deploy.all_targets";
    let rows: Vec<(i64, TargetRow)> = db
        .within(QUERY, async {
            let rows: Vec<ProjectTargetRow> = sqlx::query_as(&format!(
                "SELECT t.project_id, {TARGET_COLUMNS} FROM forge.deploy_targets t \
                 JOIN forge.deploy_environments e ON e.id = t.environment_id \
                 ORDER BY t.project_id, e.tier, t.name LIMIT 10000"
            ))
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    (
                        r.0,
                        (
                            r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9, r.10, r.11, r.12,
                        ),
                    )
                })
                .collect())
        })
        .await?;
    rows.into_iter()
        .map(|(p, r)| Ok((p, target_of(r)?)))
        .collect()
}

/// Create or replace the target `input.name` of the environment `tier` of
/// `project`, decided again on the locked project and caller; `true` with
/// it when created. Errors (nothing written): `decide`'s, `FG-REPO-0005
/// resource=deploy_environment` without that environment, `FG-WEB-0006
/// reason=too_many_targets`, the database's.
pub async fn put_target_decided(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    tier: DeployTier,
    input: &TargetInput,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<(), ForgeError>,
) -> Result<(StoredTarget, bool), ForgeError> {
    const QUERY: &str = "deploy.put_target";
    let user = caller_of(ctx)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        decide(&decided_facts(&mut tx, project, UserId::new(user)).await?)?;
        let env: Option<(i64, bool, i64)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT e.id, EXISTS (SELECT 1 FROM forge.deploy_targets t \
                       WHERE t.environment_id = e.id AND t.name = $3), \
                     (SELECT count(*) FROM forge.deploy_targets t WHERE t.environment_id = e.id) \
                     FROM forge.deploy_environments e WHERE e.project_id = $1 AND e.tier = $2 \
                     FOR UPDATE OF e",
                )
                .bind(project.get())
                .bind(tier.as_str())
                .bind(&input.name)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some((env_id, existed, count)) = env else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "deploy_environment")
                .with_context("environment", tier.as_str()));
        };
        if !existed && count >= MAX_TARGETS_PER_ENVIRONMENT {
            return Err(invalid("name", "too_many_targets"));
        }
        let sql = format!(
            "WITH up AS (INSERT INTO forge.deploy_targets AS t (environment_id, project_id, name, \
               host, port, login_user, host_key_type, host_key, host_key_fingerprint, script, \
               enabled, created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12) \
             ON CONFLICT (environment_id, name) DO UPDATE SET host = EXCLUDED.host, \
               port = EXCLUDED.port, login_user = EXCLUDED.login_user, \
               host_key_type = EXCLUDED.host_key_type, host_key = EXCLUDED.host_key, \
               host_key_fingerprint = EXCLUDED.host_key_fingerprint, script = EXCLUDED.script, \
               enabled = EXCLUDED.enabled, updated_by = EXCLUDED.updated_by, updated_at = now() \
             RETURNING t.*) \
             SELECT {TARGET_COLUMNS} FROM up t JOIN forge.deploy_environments e \
               ON e.id = t.environment_id"
        );
        let row: TargetRow = tx
            .step(|c| async move {
                sqlx::query_as(&sql)
                    .bind(env_id)
                    .bind(project.get())
                    .bind(&input.name)
                    .bind(&input.host)
                    .bind(i32::from(input.port))
                    .bind(&input.login_user)
                    .bind(input.host_key.key_type())
                    .bind(input.host_key.blob_b64())
                    .bind(input.host_key.fingerprint())
                    .bind(&input.script)
                    .bind(input.enabled)
                    .bind(user)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let stored = target_of(row)?;
        let change = DeployChange::TargetSet {
            project,
            tier,
            name: stored.name.clone(), // clone: the audit row owns it
            created: !existed,
            host: stored.host.clone(), // clone: as name
            port: input.port,
            user: stored.login_user.clone(), // clone: as name
            script: stored.script.clone(),   // clone: as name
            fingerprint: stored.host_key_fingerprint.clone(), // clone: as name
            enabled: stored.enabled,
        };
        Ok(Settle::Commit(record(change), (stored, !existed)))
    }
    .await;
    tx.settle("deploy_target.set", outcome).await
}

// ---------------------------------------------------------------- deployments

mod deployments;
mod worker;

pub use deployments::{
    approve_decided, auto_request, get_deployment, list_deployments, request_decided, CommitInput,
    StoredDeployment, MAX_LIST,
};
pub use worker::{
    abandon_stale, block_unapproved, claim_next, finish, heartbeat, stuck_running, Claim, Claimed,
    Finish, RunTarget,
};
