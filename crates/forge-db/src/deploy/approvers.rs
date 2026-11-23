//! The approvers of a deploy environment (ADR-0038 §3.4, §3.6, step C):
//! users, groups and organisation selectors, how many of them must approve
//! (`approvals_required`) and whether a delegate may approve
//! (`allow_delegation`, default off). None named: any Maintainer, as
//! before. Set by an Owner or an administrator
//! ([`put_approvers_decided`], `deploy_environment.approvers_set`, audited
//! on the project); read for the approval's decision
//! ([`environment_approvers`]) and the settings screen.

use forge_core::approver::{ApproverSelector, MAX_GROUPS, MAX_NAMED_USERS, MAX_SELECTORS};
use forge_core::deploy::{DeployChange, DeployTier};
use forge_core::ids::{GroupId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{internal, invalid, record, tier_of, TS};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::pipelines::decided_facts;
use crate::projects::ProjectUpdateFacts;
use crate::{query_error, read_query_error, Db};

/// One environment's approvers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentApprovers {
    pub environment_id: i64,
    pub tier: DeployTier,
    pub users: Vec<UserId>,
    pub groups: Vec<GroupId>,
    pub selectors: Vec<ApproverSelector>,
    pub required: i32,
    pub allow_delegation: bool,
    /// The environment's `updated_at` (UTC, to the microsecond): the
    /// version an approval decided on (a change meanwhile is a conflict).
    pub version: String,
}

impl EnvironmentApprovers {
    /// No one named: any Maintainer approves (as before step C).
    pub fn unrestricted(&self) -> bool {
        self.users.is_empty() && self.groups.is_empty() && self.selectors.is_empty()
    }
}

/// What an Owner sets (validated by the caller: the selectors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproversInput {
    pub users: Vec<UserId>,
    pub groups: Vec<GroupId>,
    pub selectors: Vec<ApproverSelector>,
    pub required: i32,
    pub allow_delegation: bool,
}

type EnvSql = (i64, String, i32, bool, String);
type ApproverSql = (Option<i64>, Option<i64>, Option<String>);

const VERSION: &str =
    "to_char(e.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')";

async fn approvers_on(
    conn: &mut PgConnection,
    env: EnvSql,
) -> Result<EnvironmentApprovers, ForgeError> {
    let (id, tier, required, allow_delegation, version) = env;
    let rows: Vec<ApproverSql> = sqlx::query_as(
        "SELECT user_id, group_id, selector::text FROM forge.deploy_environment_approvers \
         WHERE environment_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(conn)
    .await
    .map_err(|e| query_error("deploy.environment_approvers", e))?;
    let mut out = EnvironmentApprovers {
        environment_id: id,
        tier: tier_of(&tier)?,
        users: Vec::new(),
        groups: Vec::new(),
        selectors: Vec::new(),
        required,
        allow_delegation,
        version,
    };
    for (user, group, selector) in rows {
        if let Some(u) = user {
            out.users.push(UserId::new(u));
        }
        if let Some(g) = group {
            out.groups.push(GroupId::new(g));
        }
        if let Some(raw) = selector {
            // A stored selector that no longer parses fails closed.
            let parsed = serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|v| ApproverSelector::from_value(&v).ok())
                .ok_or_else(|| {
                    internal("stored_selector_invalid").with_context("environment_id", id)
                })?;
            out.selectors.push(parsed);
        }
    }
    Ok(out)
}

/// The approvers of `project`'s environment `tier` (`None`: no such
/// environment), on its own connection of the write pool — the approval's
/// decision reads it, never a lagging copy.
pub async fn environment_approvers(
    db: &Db,
    project: ProjectId,
    tier: DeployTier,
) -> Result<Option<EnvironmentApprovers>, ForgeError> {
    const QUERY: &str = "deploy.environment_approvers";
    db.within(QUERY, async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let env: Option<EnvSql> = sqlx::query_as(&format!(
            "SELECT e.id, e.tier, e.approvals_required, e.allow_delegation, {VERSION} \
             FROM forge.deploy_environments e WHERE e.project_id = $1 AND e.tier = $2"
        ))
        .bind(project.get())
        .bind(tier.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        match env {
            Some(env) => approvers_on(&mut conn, env).await.map(Some),
            None => Ok(None),
        }
    })
    .await
}

/// Every environment's approvers of `project`, by tier (the read pool:
/// the settings screen).
pub async fn project_approvers(
    db: &Db,
    project: ProjectId,
) -> Result<Vec<EnvironmentApprovers>, ForgeError> {
    const QUERY: &str = "deploy.project_approvers";
    db.within(QUERY, async {
        let mut conn = db
            .pool()
            .executor()
            .acquire()
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        let envs: Vec<EnvSql> = sqlx::query_as(&format!(
            "SELECT e.id, e.tier, e.approvals_required, e.allow_delegation, {VERSION} \
             FROM forge.deploy_environments e WHERE e.project_id = $1 \
             ORDER BY CASE e.tier WHEN 'dev' THEN 1 WHEN 'stg' THEN 2 ELSE 3 END"
        ))
        .bind(project.get())
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let mut out = Vec::with_capacity(envs.len());
        for env in envs {
            out.push(approvers_on(&mut conn, env).await?);
        }
        Ok(out)
    })
    .await
}

/// The environment's approval version as locked in an approval's
/// transaction (`FOR SHARE`: an approvers change waits).
pub(crate) async fn version_on(
    conn: &mut PgConnection,
    environment: i64,
) -> Result<(String, i32, bool), ForgeError> {
    sqlx::query_as(&format!(
        "SELECT {VERSION}, e.approvals_required, e.allow_delegation \
         FROM forge.deploy_environments e WHERE e.id = $1 FOR SHARE"
    ))
    .bind(environment)
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("deploy.environment_version", e))
}

fn write_error(e: sqlx::Error) -> ForgeError {
    let constraint = e
        .as_database_error()
        .and_then(|d| d.constraint())
        .map(str::to_owned);
    match constraint.as_deref() {
        Some("deploy_environment_approvers_user_id_fkey") => invalid("users", "unknown_user"),
        Some("deploy_environment_approvers_group_id_fkey") => invalid("groups", "unknown_group"),
        Some("deploy_environment_approvers_selector_shape") => invalid("selectors", "shape"),
        Some("deploy_environments_approvals_required") => invalid("approvals_required", "range"),
        Some(_) | None => query_error("deploy.put_approvers", e),
    }
}

/// Replace the approvers of `project`'s environment `tier`, decided again
/// on the locked project and caller (`decide`). Errors (nothing written):
/// `decide`'s, `FG-REPO-0005 resource=deploy_environment` (no such
/// environment), `FG-WEB-0006` (too many, unknown user or group, a
/// required count outside 1..=20 or above the named users when only users
/// are named), the database's.
pub async fn put_approvers_decided(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    tier: DeployTier,
    input: &ApproversInput,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<(), ForgeError>,
) -> Result<EnvironmentApprovers, ForgeError> {
    const QUERY: &str = "deploy.put_approvers";
    let user = super::caller_of(ctx)?;
    if input.users.len() > MAX_NAMED_USERS {
        return Err(invalid("users", "too_many"));
    }
    if input.groups.len() > MAX_GROUPS {
        return Err(invalid("groups", "too_many"));
    }
    if input.selectors.len() > MAX_SELECTORS {
        return Err(invalid("selectors", "too_many"));
    }
    if !(1..=20).contains(&input.required) {
        return Err(invalid("approvals_required", "range"));
    }
    let mut input = input.clone(); // clone: deduplicated locally
    input.users.sort_unstable_by_key(|u| u.get());
    input.users.dedup();
    input.groups.sort_unstable_by_key(|g| g.get());
    input.groups.dedup();
    let only_users = input.groups.is_empty() && input.selectors.is_empty();
    if only_users
        && !input.users.is_empty()
        && usize::try_from(input.required).unwrap_or(usize::MAX) > input.users.len()
    {
        // More approvals than the people named: no deployment could ever
        // be approved.
        return Err(invalid("approvals_required", "more_than_approvers"));
    }
    let input = &input;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        decide(&decided_facts(&mut tx, project, UserId::new(user)).await?)?;
        let env: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT id FROM forge.deploy_environments WHERE project_id = $1 AND tier = $2 \
                     FOR UPDATE",
                )
                .bind(project.get())
                .bind(tier.as_str())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some(env) = env else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "deploy_environment")
                .with_context("environment", tier.as_str()));
        };
        let selectors: Vec<String> = input
            .selectors
            .iter()
            .map(|s| s.to_value().to_string())
            .collect();
        let users: Vec<i64> = input.users.iter().map(|u| u.get()).collect();
        let groups: Vec<i64> = input.groups.iter().map(|g| g.get()).collect();
        let stored = tx
            .step(|c| async move {
                sqlx::query(
                    "DELETE FROM forge.deploy_environment_approvers WHERE environment_id = $1",
                )
                .bind(env)
                .execute(&mut *c)
                .await
                .map_err(|e| query_error(QUERY, e))?;
                sqlx::query(
                    "INSERT INTO forge.deploy_environment_approvers (environment_id, project_id, \
                     user_id) SELECT $1, $2, u FROM unnest($3::bigint[]) AS u",
                )
                .bind(env)
                .bind(project.get())
                .bind(&users)
                .execute(&mut *c)
                .await
                .map_err(write_error)?;
                sqlx::query(
                    "INSERT INTO forge.deploy_environment_approvers (environment_id, project_id, \
                     group_id) SELECT $1, $2, g FROM unnest($3::bigint[]) AS g",
                )
                .bind(env)
                .bind(project.get())
                .bind(&groups)
                .execute(&mut *c)
                .await
                .map_err(write_error)?;
                sqlx::query(
                    "INSERT INTO forge.deploy_environment_approvers (environment_id, project_id, \
                     selector) SELECT $1, $2, s::jsonb FROM unnest($3::text[]) AS s",
                )
                .bind(env)
                .bind(project.get())
                .bind(&selectors)
                .execute(&mut *c)
                .await
                .map_err(write_error)?;
                let row: EnvSql = sqlx::query_as(&format!(
                    "UPDATE forge.deploy_environments e SET approvals_required = $2, \
                     allow_delegation = $3, updated_by = $4, updated_at = clock_timestamp() \
                     WHERE e.id = $1 RETURNING e.id, e.tier, e.approvals_required, \
                     e.allow_delegation, {VERSION}"
                ))
                .bind(env)
                .bind(input.required)
                .bind(input.allow_delegation)
                .bind(user)
                .fetch_one(&mut *c)
                .await
                .map_err(write_error)?;
                // Review of ADR-0038 step C: approvals given to deployments
                // still waiting were decided on the approvers just replaced;
                // they end with them (the people approve again).
                sqlx::query(
                    "DELETE FROM forge.deployment_approvals a USING forge.deployments d \
                     WHERE d.id = a.deployment_id AND d.environment_id = $1 \
                       AND d.status = 'pending_approval'",
                )
                .bind(env)
                .execute(&mut *c)
                .await
                .map_err(|e| query_error(QUERY, e))?;
                approvers_on(c, row).await
            })
            .await?;
        let change = DeployChange::ApproversSet {
            project,
            tier,
            users: stored.users.clone(), // clone: the audit row owns them
            groups: stored.groups.clone(), // clone: as above
            selectors: stored
                .selectors
                .iter()
                .map(ApproverSelector::to_value)
                .collect(),
            required: stored.required,
            allow_delegation: stored.allow_delegation,
        };
        Ok(Settle::Commit(record(change), stored))
    }
    .await;
    tx.settle("deploy_environment.approvers_set", outcome).await
}

/// One approval of a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentApproval {
    pub approver: UserId,
    pub approver_username: String,
    pub on_behalf_of: Option<UserId>,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// The approvals of deployment `id` of `project`, oldest first (the read
/// pool).
pub async fn deployment_approvals(
    db: &Db,
    project: ProjectId,
    id: i64,
) -> Result<Vec<DeploymentApproval>, ForgeError> {
    const QUERY: &str = "deploy.deployment_approvals";
    let rows: Vec<(i64, String, Option<i64>, String)> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT a.approver_id, u.username, a.on_behalf_of, \
                 to_char(a.created_at AT TIME ZONE 'UTC', '{TS}') \
                 FROM forge.deployment_approvals a JOIN forge.users u ON u.id = a.approver_id \
                 WHERE a.project_id = $1 AND a.deployment_id = $2 ORDER BY a.id"
            ))
            .bind(project.get())
            .bind(id)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(a, name, b, at)| DeploymentApproval {
            approver: UserId::new(a),
            approver_username: name,
            on_behalf_of: b.map(UserId::new),
            created_at: at,
        })
        .collect())
}
