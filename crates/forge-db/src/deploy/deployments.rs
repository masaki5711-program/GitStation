//! Deployments: requests, approvals and reads (M4 C11; the review's
//! amendment of ADR-0031: the commit shown to the approver, every person
//! behind the SHA kept from approving it, prod only from a succeeded
//! pipeline).

use forge_core::deploy::{DeployChange, DeployTier, DeploymentStatus};
use forge_core::{ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use super::{caller_of, internal, invalid, record, status_of, tier_of, TS};
use crate::change::{Actor, ChangeContext, ChangeTx, Settle};
use crate::pipelines::decided_facts;
use crate::projects::ProjectUpdateFacts;
use crate::{query_error, read_query_error, Db};

/// The most deployments a list answers.
pub const MAX_LIST: i64 = 100;

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
    pub commit_subject: Option<String>,
    pub commit_author_name: Option<String>,
    pub commit_author_email: Option<String>,
    /// The accounts behind the SHA (merge request authors of any merge or
    /// head at that SHA, its pushers, the commit author and committer by
    /// email). Read on the write pool only (the read pool cannot see
    /// emails); empty in lists.
    pub change_authors: Vec<i64>,
}

/// The commit as read from git when a person requested it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitInput {
    pub subject: String,
    pub author_name: String,
    pub author_email: String,
    pub committer_email: String,
}

#[derive(sqlx::FromRow)]
struct DeploymentRow {
    id: i64,
    project_id: i64,
    tier: String,
    sha: String,
    status: String,
    requested_by: Option<i64>,
    merge_request_author: Option<i64>,
    approved_by: Option<i64>,
    approved_at: Option<String>,
    approval_expires_at: Option<String>,
    previous_sha: Option<String>,
    error_code: Option<String>,
    failed_target: Option<String>,
    created_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    commit_subject: Option<String>,
    commit_author_name: Option<String>,
    commit_author_email: Option<String>,
    change_authors: Vec<i64>,
}

/// The people behind deployment `a`'s SHA (write pool only: emails).
pub(crate) fn change_authors_sql(a: &str) -> String {
    format!(
        "ARRAY(SELECT DISTINCT x.u FROM ( \
           SELECT mr.author_id AS u FROM forge.merge_request_merges m \
             JOIN forge.merge_requests mr ON mr.id = m.merge_request_id \
            WHERE m.project_id = {a}.project_id AND (m.new_sha = {a}.sha OR m.head_sha = {a}.sha) \
           UNION SELECT mr.author_id FROM forge.merge_requests mr \
            WHERE mr.project_id = {a}.project_id AND mr.head_sha = {a}.sha \
           UNION SELECT pc.pusher_id FROM forge.push_commits pc \
            WHERE pc.project_id = {a}.project_id AND pc.commit_sha = {a}.sha \
           UNION SELECT us.id FROM forge.users us WHERE us.email IS NOT NULL \
             AND lower(us.email) IN (lower({a}.commit_author_email), lower({a}.committer_email)) \
         ) x WHERE x.u IS NOT NULL ORDER BY x.u)"
    )
}

/// The select list of a deployment `a`; `authors`: with the people behind
/// it (write pool only).
pub(crate) fn deployment_columns(a: &str, authors: bool) -> String {
    let ts = |c: &str| format!("to_char({a}.{c} AT TIME ZONE 'UTC', '{TS}') AS {c}");
    let change = if authors {
        change_authors_sql(a)
    } else {
        "'{}'::bigint[]".to_owned()
    };
    format!(
        "{a}.id, {a}.project_id, {a}.tier, {a}.sha, {a}.status, {a}.requested_by, \
         {a}.merge_request_author, {a}.approved_by, {}, {}, {a}.previous_sha, {a}.error_code, \
         {a}.failed_target, {}, {}, {}, {a}.commit_subject, {a}.commit_author_name, \
         {a}.commit_author_email, {change} AS change_authors",
        ts("approved_at"),
        ts("approval_expires_at"),
        ts("created_at"),
        ts("started_at"),
        ts("finished_at"),
    )
}

fn deployment_of(r: DeploymentRow) -> Result<StoredDeployment, ForgeError> {
    Ok(StoredDeployment {
        id: r.id,
        project_id: r.project_id,
        tier: tier_of(&r.tier)?,
        sha: r.sha,
        status: status_of(&r.status)?,
        requested_by: r.requested_by,
        merge_request_author: r.merge_request_author,
        approved_by: r.approved_by,
        approved_at: r.approved_at,
        approval_expires_at: r.approval_expires_at,
        previous_sha: r.previous_sha,
        error_code: r.error_code,
        failed_target: r.failed_target,
        created_at: r.created_at,
        started_at: r.started_at,
        finished_at: r.finished_at,
        commit_subject: r.commit_subject,
        commit_author_name: r.commit_author_name,
        commit_author_email: r.commit_author_email,
        change_authors: r.change_authors,
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
                deployment_columns("d", false)
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

/// One deployment of `project` with the people behind its SHA (the write
/// pool: an approval loads it).
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
                deployment_columns("d", true)
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

/// What [`insert_deployment`] writes.
struct NewDeployment<'a> {
    env_id: i64,
    tier: DeployTier,
    status: DeploymentStatus,
    sha: &'a str,
    requested_by: Option<i64>,
    request_id: &'a str,
    commit: Option<&'a CommitInput>,
}

/// Insert the deployment and answer it (the audit record is the caller's).
async fn insert_deployment(
    tx: &mut ChangeTx,
    project: ProjectId,
    new: NewDeployment<'_>,
    query: &'static str,
) -> Result<StoredDeployment, ForgeError> {
    let sql = format!(
        "WITH ins AS (INSERT INTO forge.deployments (project_id, environment_id, tier, sha, \
           status, requested_by, request_id, merge_request_author, commit_subject, \
           commit_author_name, commit_author_email, committer_email) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, \
           (SELECT mr.author_id FROM forge.merge_request_merges m \
              JOIN forge.merge_requests mr ON mr.id = m.merge_request_id \
             WHERE m.project_id = $1 AND m.new_sha = $4 AND m.state = 'applied' \
             ORDER BY m.id DESC LIMIT 1), $8, $9, $10, $11) \
         RETURNING *) SELECT {} FROM ins d",
        deployment_columns("d", true)
    );
    let c = new.commit;
    let row: DeploymentRow = tx
        .step(|conn| async move {
            sqlx::query_as(&sql)
                .bind(project.get())
                .bind(new.env_id)
                .bind(new.tier.as_str())
                .bind(new.sha)
                .bind(new.status.as_str())
                .bind(new.requested_by)
                .bind(new.request_id)
                .bind(c.map(|c| c.subject.as_str()))
                .bind(c.map(|c| c.author_name.as_str()))
                .bind(c.map(|c| c.author_email.as_str()))
                .bind(c.map(|c| c.committer_email.as_str()))
                .fetch_one(conn)
                .await
                .map_err(|e| query_error(query, e))
        })
        .await?;
    deployment_of(row)
}

fn not_allowed(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::DeployCommitNotAllowed).with_context("reason", reason)
}

/// A person's request to deploy `sha` (checked on a protected branch by
/// the caller, with `commit` read from git) to the environment `tier` of
/// `project`, decided again on the locked project and caller.
/// `pending_approval` when the environment requires an approval,
/// `queued` otherwise. Errors (nothing written): `decide`'s,
/// `FG-REPO-0005 resource=deploy_environment`, `FG-DEPLOY-0002` (frozen
/// now), `FG-DEPLOY-0011 reason=no_succeeded_pipeline` (prod without a
/// succeeded push pipeline of the SHA), `FG-WEB-0006 reason=no_targets`,
/// the database's.
pub async fn request_decided(
    db: &Db,
    ctx: &ChangeContext,
    (project, tier, sha): (ProjectId, DeployTier, &str),
    commit: Option<&CommitInput>,
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
        if tier.is_prod() {
            let passed: bool = tx
                .step(|c| async move {
                    sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM forge.ci_pipelines WHERE project_id = $1 \
                           AND sha = $2 AND source = 'push' AND status = 'succeeded')",
                    )
                    .bind(project.get())
                    .bind(sha)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
                })
                .await?;
            if !passed {
                return Err(not_allowed("no_succeeded_pipeline")
                    .with_context("project_id", project.get())
                    .with_context("environment", tier.as_str()));
            }
        }
        let status = if requires_approval || tier.is_prod() {
            DeploymentStatus::PendingApproval
        } else {
            DeploymentStatus::Queued
        };
        let stored = insert_deployment(
            &mut tx,
            project,
            NewDeployment {
                env_id,
                tier,
                status,
                sha,
                requested_by: Some(user),
                request_id: &request_id,
                commit,
            },
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
/// pipeline of that branch, **created while the branch was protected**
/// (`ci_pipelines.protected`), whose SHA the environment has no deployment
/// of yet becomes a deployment requested by the system — unless the
/// environment is frozen now or has no enabled target (the next pass looks
/// again). Each in its own transaction with its audit row; at most `batch`
/// per pass.
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
                   WHERE p.project_id = e.project_id AND p.source = 'push' AND p.protected \
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
        let rid = request_id.to_string();
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
                NewDeployment {
                    env_id,
                    tier,
                    status,
                    sha: &sha,
                    requested_by: None,
                    request_id: &rid,
                    commit: None,
                },
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

#[derive(sqlx::FromRow)]
struct LockedRow {
    #[sqlx(flatten)]
    row: DeploymentRow,
    in_window: bool,
}

/// A person's approval of deployment `id` of `project`, under the row's
/// lock, decided again (`decide`: the locked project's facts and the
/// locked deployment with the people behind its SHA). Errors (nothing
/// written): `decide`'s, `FG-REPO-0005 resource=deployment`,
/// `FG-DEPLOY-0010` (not `pending_approval`), `FG-DEPLOY-0001
/// reason=approval_window_passed`, `FG-DEPLOY-0009` (a backstop: the
/// approver is the requester or behind the SHA), the database's.
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
               AS in_window \
             FROM forge.deployments d JOIN forge.deploy_environments e \
               ON e.id = d.environment_id \
             WHERE d.project_id = $1 AND d.id = $2 FOR UPDATE OF d",
            deployment_columns("d", true)
        );
        let locked: Option<LockedRow> = tx
            .step(|c| async move {
                sqlx::query_as(&sql)
                    .bind(project.get())
                    .bind(id)
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some(LockedRow { row, in_window }) = locked else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "deployment")
                .with_context("deployment_id", id));
        };
        let current = deployment_of(row)?;
        decide(&facts, &current)?;
        if current.requested_by == Some(user) || current.change_authors.contains(&user) {
            return Err(ForgeError::new(ErrorCode::DeploySelfApprovalRefused)
                .with_context("reason", "backstop")
                .with_context("deployment_id", id));
        }
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
            deployment_columns("d", true)
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
        if stored.id != id {
            return Err(internal("approved_another_deployment"));
        }
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
