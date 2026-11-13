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

use crate::change::{Actor, AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::pipelines::decided_facts;
use crate::projects::ProjectUpdateFacts;
use crate::{query_error, read_query_error, Db};

#[cfg(test)]
mod tests;

/// The audit resource of every change here (tables.toml).
pub const RESOURCE: &str = "project";
/// The most deployments a list answers.
pub const MAX_LIST: i64 = 100;

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

/// A target as the claim reads it.
type RunTargetRow = (String, String, i32, String, String, String, String, String);

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

/// A deployment as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDeployment {
    pub id: i64,
    pub project_id: i64,
    pub tier: DeployTier,
    pub sha: String,
    pub status: DeploymentStatus,
    pub requested_by: Option<i64>,
    pub merge_request_author: Option<i64>,
    pub approved_by: Option<i64>,
    pub approved_at: Option<String>,
    pub approval_expires_at: Option<String>,
    pub previous_sha: Option<String>,
    pub error_code: Option<String>,
    pub failed_target: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

type DeploymentRow = (
    i64,
    i64,
    String,
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

fn deployment_columns(alias: &str) -> String {
    let ts = |c: &str| format!("to_char({alias}.{c} AT TIME ZONE 'UTC', '{TS}')");
    format!(
        "{a}.id, {a}.project_id, {a}.tier, {a}.sha, {a}.status, {a}.requested_by, \
         {a}.merge_request_author, {a}.approved_by, {}, {}, {a}.previous_sha, {a}.error_code, \
         {a}.failed_target, {}, {}, {}",
        ts("approved_at"),
        ts("approval_expires_at"),
        ts("created_at"),
        ts("started_at"),
        ts("finished_at"),
        a = alias
    )
}

fn deployment_of(r: DeploymentRow) -> Result<StoredDeployment, ForgeError> {
    Ok(StoredDeployment {
        id: r.0,
        project_id: r.1,
        tier: tier_of(&r.2)?,
        sha: r.3,
        status: status_of(&r.4)?,
        requested_by: r.5,
        merge_request_author: r.6,
        approved_by: r.7,
        approved_at: r.8,
        approval_expires_at: r.9,
        previous_sha: r.10,
        error_code: r.11,
        failed_target: r.12,
        created_at: r.13,
        started_at: r.14,
        finished_at: r.15,
    })
}

/// The newest deployments of `project` (the read pool), at most
/// [`MAX_LIST`].
pub async fn list_deployments(
    db: &Db,
    project: ProjectId,
) -> Result<Vec<StoredDeployment>, ForgeError> {
    const QUERY: &str = "deploy.list_deployments";
    let rows: Vec<DeploymentRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.deployments d WHERE d.project_id = $1 \
                 ORDER BY d.id DESC LIMIT $2",
                deployment_columns("d")
            ))
            .bind(project.get())
            .bind(MAX_LIST)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(deployment_of).collect()
}

/// One deployment of `project` (the write pool: an approval's route reads
/// it right after a request).
pub async fn get_deployment(
    db: &Db,
    project: ProjectId,
    id: i64,
) -> Result<Option<StoredDeployment>, ForgeError> {
    const QUERY: &str = "deploy.get_deployment";
    let row: Option<DeploymentRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.deployments d WHERE d.project_id = $1 AND d.id = $2",
                deployment_columns("d")
            ))
            .bind(project.get())
            .bind(id)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(deployment_of).transpose()
}

/// The environment a request locks (`FOR SHARE`): its id, approval rule,
/// whether it is frozen now, and whether it has an enabled target.
type RequestEnv = (i64, bool, bool, bool);

async fn lock_env_for_request(
    tx: &mut ChangeTx,
    project: ProjectId,
    tier: DeployTier,
    query: &'static str,
) -> Result<Option<RequestEnv>, ForgeError> {
    tx.step(|c| async move {
        sqlx::query_as(
            "SELECT e.id, e.requires_approval, e.freeze_windows @> now(), \
               EXISTS (SELECT 1 FROM forge.deploy_targets t \
                 WHERE t.environment_id = e.id AND t.enabled) \
             FROM forge.deploy_environments e WHERE e.project_id = $1 AND e.tier = $2 \
             FOR SHARE OF e",
        )
        .bind(project.get())
        .bind(tier.as_str())
        .fetch_optional(c)
        .await
        .map_err(|e| query_error(query, e))
    })
    .await
}

/// Insert the deployment and answer it (the audit record is the caller's).
async fn insert_deployment(
    tx: &mut ChangeTx,
    project: ProjectId,
    (env_id, tier, status): (i64, DeployTier, DeploymentStatus),
    sha: &str,
    requested_by: Option<i64>,
    request_id: &str,
    query: &'static str,
) -> Result<StoredDeployment, ForgeError> {
    let sql = format!(
        "WITH ins AS (INSERT INTO forge.deployments (project_id, environment_id, tier, sha, \
           status, requested_by, request_id, merge_request_author) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, \
           (SELECT mr.author_id FROM forge.merge_request_merges m \
              JOIN forge.merge_requests mr ON mr.id = m.merge_request_id \
             WHERE m.project_id = $1 AND m.new_sha = $4 AND m.state = 'applied' \
             ORDER BY m.id DESC LIMIT 1)) \
         RETURNING *) SELECT {} FROM ins d",
        deployment_columns("d")
    );
    let row: DeploymentRow = tx
        .step(|c| async move {
            sqlx::query_as(&sql)
                .bind(project.get())
                .bind(env_id)
                .bind(tier.as_str())
                .bind(sha)
                .bind(status.as_str())
                .bind(requested_by)
                .bind(request_id)
                .fetch_one(c)
                .await
                .map_err(|e| query_error(query, e))
        })
        .await?;
    deployment_of(row)
}

/// A person's request to deploy `sha` (validated lowercase hex) to the
/// environment `tier` of `project`, decided again on the locked project
/// and caller. `pending_approval` when the environment requires an
/// approval, `queued` otherwise. Errors (nothing written): `decide`'s,
/// `FG-REPO-0005 resource=deploy_environment`, `FG-DEPLOY-0002` (frozen
/// now), `FG-WEB-0006 reason=no_targets`, the database's.
pub async fn request_decided(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    tier: DeployTier,
    sha: &str,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<(), ForgeError>,
) -> Result<StoredDeployment, ForgeError> {
    const QUERY: &str = "deploy.request";
    let user = caller_of(ctx)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let request_id = ctx.request_id.to_string();
    let outcome = async {
        decide(&decided_facts(&mut tx, project, UserId::new(user)).await?)?;
        let Some((env_id, requires_approval, frozen, has_targets)) =
            lock_env_for_request(&mut tx, project, tier, QUERY).await?
        else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "deploy_environment")
                .with_context("environment", tier.as_str()));
        };
        if frozen {
            return Err(ForgeError::new(ErrorCode::DeployFrozen)
                .with_context("project_id", project.get())
                .with_context("environment", tier.as_str()));
        }
        if !has_targets {
            return Err(invalid("environment", "no_targets"));
        }
        let status = if requires_approval || tier.is_prod() {
            DeploymentStatus::PendingApproval
        } else {
            DeploymentStatus::Queued
        };
        let stored = insert_deployment(
            &mut tx,
            project,
            (env_id, tier, status),
            sha,
            Some(user),
            &request_id,
            QUERY,
        )
        .await?;
        let change = DeployChange::Requested {
            project,
            deployment: stored.id,
            tier,
            sha: stored.sha.clone(), // clone: the audit row owns it
            status: stored.status,
        };
        Ok(Settle::Commit(record(change), stored))
    }
    .await;
    tx.settle("deployment.requested", outcome).await
}

/// The automatic requests (ADR-0031 decision 9): for every environment
/// with an `auto_deploy_ref` (never prod), the newest succeeded push
/// pipeline of that branch whose SHA the environment has no deployment of
/// yet becomes a deployment requested by the system — unless the
/// environment is frozen now or has no enabled target (the next pass looks
/// again). Each in its own transaction with its audit row; at most
/// `batch` per pass.
pub async fn auto_request(
    db: &Db,
    request_id: &forge_core::RequestId,
    batch: i64,
) -> Result<Vec<StoredDeployment>, ForgeError> {
    const QUERY: &str = "deploy.auto_request";
    let candidates: Vec<(i64, String, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT e.project_id, e.tier, p.sha FROM forge.deploy_environments e \
                 JOIN LATERAL (SELECT sha FROM forge.ci_pipelines p \
                   WHERE p.project_id = e.project_id AND p.source = 'push' \
                     AND p.ref = e.auto_deploy_ref AND p.status = 'succeeded' \
                   ORDER BY p.id DESC LIMIT 1) p ON true \
                 WHERE e.auto_deploy_ref IS NOT NULL AND e.tier <> 'prod' \
                   AND NOT (e.freeze_windows @> now()) \
                   AND EXISTS (SELECT 1 FROM forge.deploy_targets t \
                     WHERE t.environment_id = e.id AND t.enabled) \
                   AND NOT EXISTS (SELECT 1 FROM forge.deployments d \
                     WHERE d.environment_id = e.id AND d.sha = p.sha) \
                 ORDER BY e.id LIMIT $1",
            )
            .bind(batch)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let mut out = Vec::new();
    for (project, tier, sha) in candidates {
        let project = ProjectId::new(project);
        let tier = tier_of(&tier)?;
        let ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: one context per change
        let mut tx = ChangeTx::begin(db, &ctx).await?;
        let outcome = async {
            let Some((env_id, requires_approval, frozen, has_targets)) =
                lock_env_for_request(&mut tx, project, tier, QUERY).await?
            else {
                return Ok(Settle::Discard(None));
            };
            let already: bool = tx
                .step(|c| {
                    let sha = sha.clone(); // clone: the step owns its bind
                    async move {
                        sqlx::query_scalar(
                            "SELECT EXISTS (SELECT 1 FROM forge.deployments \
                             WHERE environment_id = $1 AND sha = $2)",
                        )
                        .bind(env_id)
                        .bind(sha)
                        .fetch_one(c)
                        .await
                        .map_err(|e| query_error(QUERY, e))
                    }
                })
                .await?;
            if frozen || !has_targets || already {
                return Ok(Settle::Discard(None));
            }
            let status = if requires_approval {
                DeploymentStatus::PendingApproval
            } else {
                DeploymentStatus::Queued
            };
            let stored = insert_deployment(
                &mut tx,
                project,
                (env_id, tier, status),
                &sha,
                None,
                &request_id.to_string(),
                QUERY,
            )
            .await?;
            let change = DeployChange::Requested {
                project,
                deployment: stored.id,
                tier,
                sha: stored.sha.clone(), // clone: the audit row owns it
                status: stored.status,
            };
            Ok(Settle::Commit(record(change), Some(stored)))
        }
        .await;
        if let Some(stored) = tx.settle("deployment.requested", outcome).await? {
            out.push(stored);
        }
    }
    Ok(out)
}

/// A person's approval of deployment `id` of `project`, under the row's
/// lock, decided again (`decide`: the locked project's facts and the
/// locked deployment). Errors (nothing written): `decide`'s, `FG-REPO-0005
/// resource=deployment`, `FG-DEPLOY-0010` (not `pending_approval`),
/// `FG-DEPLOY-0001 reason=approval_window_passed`, the database's.
pub async fn approve_decided(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    id: i64,
    decide: impl FnOnce(&ProjectUpdateFacts, &StoredDeployment) -> Result<(), ForgeError>,
) -> Result<StoredDeployment, ForgeError> {
    const QUERY: &str = "deploy.approve";
    let user = caller_of(ctx)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = decided_facts(&mut tx, project, UserId::new(user)).await?;
        let sql = format!(
            "SELECT {}, d.created_at + make_interval(mins => e.approval_ttl_minutes) > now() \
             FROM forge.deployments d JOIN forge.deploy_environments e \
               ON e.id = d.environment_id \
             WHERE d.project_id = $1 AND d.id = $2 FOR UPDATE OF d",
            deployment_columns("d")
        );
        let locked: Option<(DeploymentRow, bool)> = tx
            .step(|c| async move {
                let row: Option<sqlx::postgres::PgRow> = sqlx::query(&sql)
                    .bind(project.get())
                    .bind(id)
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))?;
                row.map(|r| split_row(&r, QUERY)).transpose()
            })
            .await?;
        let Some((row, in_window)) = locked else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "deployment")
                .with_context("deployment_id", id));
        };
        let current = deployment_of(row)?;
        decide(&facts, &current)?;
        if current.status != DeploymentStatus::PendingApproval {
            return Err(ForgeError::new(ErrorCode::DeploymentStateConflict)
                .with_context("deployment_id", id)
                .with_context("project_id", project.get())
                .with_context("status", current.status.as_str()));
        }
        if !in_window {
            return Err(ForgeError::new(ErrorCode::DeployApprovalRequired)
                .with_context("deployment_id", id)
                .with_context("reason", "approval_window_passed"));
        }
        let sql = format!(
            "WITH up AS (UPDATE forge.deployments d SET status = 'queued', approved_by = $3, \
               approved_at = now(), approval_expires_at = now() + make_interval(mins => \
                 (SELECT e.approval_ttl_minutes FROM forge.deploy_environments e \
                  WHERE e.id = d.environment_id)), \
               updated_at = now() \
             WHERE d.project_id = $1 AND d.id = $2 RETURNING d.*) SELECT {} FROM up d",
            deployment_columns("d")
        );
        let row: DeploymentRow = tx
            .step(|c| async move {
                sqlx::query_as(&sql)
                    .bind(project.get())
                    .bind(id)
                    .bind(user)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let stored = deployment_of(row)?;
        let change = DeployChange::Approved {
            project,
            deployment: id,
            tier: stored.tier,
            sha: stored.sha.clone(), // clone: the audit row owns it
        };
        Ok(Settle::Commit(record(change), stored))
    }
    .await;
    tx.settle("deployment.approved", outcome).await
}

/// A [`DeploymentRow`] and one trailing boolean from a raw row.
fn split_row(
    r: &sqlx::postgres::PgRow,
    query: &'static str,
) -> Result<(DeploymentRow, bool), ForgeError> {
    use sqlx::{FromRow as _, Row as _};
    let row = DeploymentRow::from_row(r).map_err(|e| query_error(query, e))?;
    let flag: bool = r.try_get(16).map_err(|e| query_error(query, e))?;
    Ok((row, flag))
}

// ---------------------------------------------------------------- the worker

/// A target as the worker runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTarget {
    pub name: String,
    pub host: String,
    pub port: i32,
    pub login_user: String,
    pub host_key_type: String,
    pub host_key: String,
    pub host_key_fingerprint: String,
    pub script: String,
}

/// A deployment the worker took (`running`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    pub id: i64,
    pub project_id: i64,
    pub tier: DeployTier,
    pub sha: String,
    /// The environment's last succeeded SHA (`None`: its first deployment).
    pub previous_sha: Option<String>,
    pub auto_rollback: bool,
    pub targets: Vec<RunTarget>,
}

/// What [`claim_next`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// Nothing to run.
    Idle,
    /// A deployment ended `blocked` at execution time.
    Blocked {
        id: i64,
        project_id: i64,
        tier: DeployTier,
        code: ErrorCode,
        reason: &'static str,
    },
    /// A deployment to run.
    Run(Claimed),
}

/// Take the oldest `queued` deployment of an environment that runs none
/// (module docs). The environment row is locked `FOR UPDATE` so two
/// workers never run two deployments of one environment.
pub async fn claim_next(db: &Db) -> Result<Claim, ForgeError> {
    const QUERY: &str = "deploy.claim";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let next: Option<(i64, i64, i64, String, String, bool, bool)> = sqlx::query_as(
            "SELECT d.id, d.project_id, d.environment_id, d.tier, d.sha, \
               d.approved_by IS NOT NULL, coalesce(d.approval_expires_at > now(), false) \
             FROM forge.deployments d WHERE d.status = 'queued' \
               AND NOT EXISTS (SELECT 1 FROM forge.deployments r \
                 WHERE r.environment_id = d.environment_id AND r.status = 'running') \
             ORDER BY d.id LIMIT 1 FOR UPDATE OF d SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some((id, project_id, env_id, tier, sha, approved, approval_live)) = next else {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Ok(Claim::Idle);
        };
        let tier = tier_of(&tier)?;
        // The environment, locked: one running deployment per environment.
        let (requires_approval, frozen, auto_rollback, running): (bool, bool, bool, bool) =
            sqlx::query_as(
                "SELECT e.requires_approval, e.freeze_windows @> now(), e.auto_rollback, \
                   EXISTS (SELECT 1 FROM forge.deployments r \
                     WHERE r.environment_id = e.id AND r.status = 'running') \
                 FROM forge.deploy_environments e WHERE e.id = $1 FOR UPDATE",
            )
            .bind(env_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
        if running {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Ok(Claim::Idle);
        }
        let needs_approval = requires_approval || tier.is_prod();
        let blocked = if frozen {
            Some((ErrorCode::DeployFrozen, "frozen"))
        } else if needs_approval && !(approved && approval_live) {
            Some((ErrorCode::DeployApprovalRequired, "approval_expired"))
        } else {
            None
        };
        if let Some((code, reason)) = blocked {
            sqlx::query(
                "UPDATE forge.deployments SET status = 'blocked', error_code = $2, \
                   finished_at = now(), updated_at = now() WHERE id = $1",
            )
            .bind(id)
            .bind(code.id())
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            tx.commit().await.map_err(|e| query_error(QUERY, e))?;
            return Ok(Claim::Blocked {
                id,
                project_id,
                tier,
                code,
                reason,
            });
        }
        let previous_sha: Option<String> = sqlx::query_scalar(
            "SELECT sha FROM forge.deployments WHERE environment_id = $1 \
               AND status = 'succeeded' ORDER BY finished_at DESC, id DESC LIMIT 1",
        )
        .bind(env_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let targets: Vec<RunTargetRow> = sqlx::query_as(
            "SELECT name, host, port, login_user, host_key_type, host_key, \
                   host_key_fingerprint, script FROM forge.deploy_targets \
                 WHERE environment_id = $1 AND enabled ORDER BY name",
        )
        .bind(env_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(
            "UPDATE forge.deployments SET status = 'running', started_at = now(), \
               previous_sha = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(previous_sha.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Claim::Run(Claimed {
            id,
            project_id,
            tier,
            sha,
            previous_sha,
            auto_rollback,
            targets: targets
                .into_iter()
                .map(|t| RunTarget {
                    name: t.0,
                    host: t.1,
                    port: t.2,
                    login_user: t.3,
                    host_key_type: t.4,
                    host_key: t.5,
                    host_key_fingerprint: t.6,
                    script: t.7,
                })
                .collect(),
        }))
    })
    .await
}

/// How a running deployment ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finish {
    pub status: DeploymentStatus,
    pub error_code: Option<ErrorCode>,
    pub failed_target: Option<String>,
}

/// Record the end of running deployment `id`; a success also keeps the CI
/// artifacts of the deployed SHA's pipelines (`ci_artifacts.keep`: a
/// deployed version never expires, plan 14 §9 #6). `false`: it was not
/// running (nothing written).
pub async fn finish(db: &Db, id: i64, end: &Finish) -> Result<bool, ForgeError> {
    const QUERY: &str = "deploy.finish";
    if !matches!(
        end.status,
        DeploymentStatus::Succeeded
            | DeploymentStatus::Failed
            | DeploymentStatus::RolledBack
            | DeploymentStatus::RollbackFailed
    ) {
        return Err(internal("finish_not_final").with_context("status", end.status.as_str()));
    }
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let done: Option<(i64, String)> = sqlx::query_as(
            "UPDATE forge.deployments SET status = $2, error_code = $3, failed_target = $4, \
               finished_at = now(), updated_at = now() \
             WHERE id = $1 AND status = 'running' RETURNING project_id, sha",
        )
        .bind(id)
        .bind(end.status.as_str())
        .bind(end.error_code.map(|c| c.id()))
        .bind(end.failed_target.as_deref())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some((project, sha)) = done else {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Ok(false);
        };
        if end.status == DeploymentStatus::Succeeded {
            sqlx::query(
                "UPDATE forge.ci_artifacts SET keep = true WHERE project_id = $1 AND NOT keep \
                   AND pipeline_id IN (SELECT id FROM forge.ci_pipelines \
                     WHERE project_id = $1 AND sha = $2)",
            )
            .bind(project)
            .bind(&sha)
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
        }
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(true)
    })
    .await
}

/// Requests that waited for an approval longer than their environment's
/// `approval_ttl_minutes` end `blocked` with `FG-DEPLOY-0001`; their ids.
pub async fn block_unapproved(db: &Db) -> Result<Vec<(i64, i64, DeployTier)>, ForgeError> {
    const QUERY: &str = "deploy.block_unapproved";
    let rows: Vec<(i64, i64, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "UPDATE forge.deployments d SET status = 'blocked', error_code = $1, \
                   finished_at = now(), updated_at = now() \
                 FROM forge.deploy_environments e \
                 WHERE e.id = d.environment_id AND d.status = 'pending_approval' \
                   AND d.created_at + make_interval(mins => e.approval_ttl_minutes) <= now() \
                 RETURNING d.id, d.project_id, d.tier",
            )
            .bind(ErrorCode::DeployApprovalRequired.id())
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(|(id, p, t)| Ok((id, p, tier_of(&t)?)))
        .collect()
}

/// Deployments `running` for longer than `minutes` (a worker that stopped
/// mid-deployment; `forge doctor`).
pub async fn stuck_running(db: &Db, minutes: i32) -> Result<Vec<i64>, ForgeError> {
    const QUERY: &str = "deploy.stuck_running";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT id FROM forge.deployments WHERE status = 'running' \
               AND started_at < now() - make_interval(mins => $1) ORDER BY id LIMIT 100",
        )
        .bind(minutes)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}
