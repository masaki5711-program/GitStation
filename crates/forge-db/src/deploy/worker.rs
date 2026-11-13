//! The worker side of deployments (M4 C11 and its review): take one
//! queued deployment, record its end, end requests nobody approved in
//! time and deployments whose worker stopped. Every transition writes its
//! audit row (`deployment.started`, `.succeeded`, `.failed`,
//! `.rolled_back`, `.rollback_failed`, `.blocked`, `.abandoned`) in the
//! same transaction, as the system.

use forge_core::deploy::{DeployChange, DeployTier, DeploymentStatus};
use forge_core::{ProjectId, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use super::{internal, record, tier_of};
use crate::change::{Actor, AuditEntry, ChangeContext, ChangeTx, Outcome, Settle};
use crate::{query_error, read_query_error, Db};

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

fn system(request_id: &RequestId) -> ChangeContext {
    ChangeContext::new(Actor::System, request_id.clone()) // clone: the context owns it
}

fn progress(
    (project, deployment, tier): (i64, i64, DeployTier),
    status: DeploymentStatus,
    abandoned: bool,
    code: Option<ErrorCode>,
    failed_target: Option<String>,
) -> DeployChange {
    DeployChange::Progress {
        project: ProjectId::new(project),
        deployment,
        tier,
        status,
        abandoned,
        error_code: code.map(|c| c.id().to_owned()),
        failed_target,
    }
}

type RunTargetRow = (String, String, i32, String, String, String, String, String);
type NextRow = (i64, i64, i64, String, String, bool, bool, bool);

/// Take the oldest `queued` deployment of an environment that runs none,
/// under the environment's lock (two workers never run two deployments of
/// one environment), and check again — at execution time — the freeze
/// windows, the approval (live, and no target changed after it) before
/// it runs (`blocked`) or records the previous version and marks it
/// `running` with its first heartbeat.
pub async fn claim_next(db: &Db, request_id: &RequestId) -> Result<Claim, ForgeError> {
    const QUERY: &str = "deploy.claim";
    let ctx = system(request_id);
    let mut tx = ChangeTx::begin(db, &ctx).await?;
    let outcome = async {
        let next: Option<NextRow> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT d.id, d.project_id, d.environment_id, d.tier, d.sha, \
                       d.approved_by IS NOT NULL, coalesce(d.approval_expires_at > now(), false), \
                       EXISTS (SELECT 1 FROM forge.deploy_targets t \
                         WHERE t.environment_id = d.environment_id AND d.approved_at IS NOT NULL \
                           AND t.updated_at > d.approved_at) \
                     FROM forge.deployments d WHERE d.status = 'queued' \
                       AND NOT EXISTS (SELECT 1 FROM forge.deployments r \
                         WHERE r.environment_id = d.environment_id AND r.status = 'running') \
                     ORDER BY d.id LIMIT 1 FOR UPDATE OF d SKIP LOCKED",
                )
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some((id, project_id, env_id, tier, sha, approved, approval_live, target_changed)) =
            next
        else {
            return Ok(Settle::Discard(Claim::Idle));
        };
        let tier = tier_of(&tier)?;
        let (requires_approval, frozen, auto_rollback, running): (bool, bool, bool, bool) = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT e.requires_approval, e.freeze_windows @> now(), e.auto_rollback, \
                       EXISTS (SELECT 1 FROM forge.deployments r \
                         WHERE r.environment_id = e.id AND r.status = 'running') \
                     FROM forge.deploy_environments e WHERE e.id = $1 FOR UPDATE",
                )
                .bind(env_id)
                .fetch_one(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        if running {
            return Ok(Settle::Discard(Claim::Idle));
        }
        let needs_approval = requires_approval || tier.is_prod();
        let blocked = if frozen {
            Some((ErrorCode::DeployFrozen, "frozen"))
        } else if needs_approval && !(approved && approval_live) {
            Some((ErrorCode::DeployApprovalRequired, "approval_expired"))
        } else if needs_approval && target_changed {
            Some((
                ErrorCode::DeployApprovalRequired,
                "target_changed_after_approval",
            ))
        } else {
            None
        };
        let key = (project_id, id, tier);
        if let Some((code, reason)) = blocked {
            tx.step(|c| async move {
                sqlx::query(
                    "UPDATE forge.deployments SET status = 'blocked', error_code = $2, \
                       finished_at = now(), updated_at = now() WHERE id = $1",
                )
                .bind(id)
                .bind(code.id())
                .execute(c)
                .await
                .map(drop)
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
            let change = progress(key, DeploymentStatus::Blocked, false, Some(code), None);
            return Ok(Settle::Commit(
                record(change),
                Claim::Blocked {
                    id,
                    project_id,
                    tier,
                    code,
                    reason,
                },
            ));
        }
        let previous_sha: Option<String> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT sha FROM forge.deployments WHERE environment_id = $1 \
                       AND status = 'succeeded' ORDER BY finished_at DESC, id DESC LIMIT 1",
                )
                .bind(env_id)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let targets: Vec<RunTargetRow> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT name, host, port, login_user, host_key_type, host_key, \
                       host_key_fingerprint, script FROM forge.deploy_targets \
                     WHERE environment_id = $1 AND enabled ORDER BY name",
                )
                .bind(env_id)
                .fetch_all(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let prev = previous_sha.clone(); // clone: the row and the claim both own it
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.deployments SET status = 'running', started_at = now(), \
                   heartbeat_at = now(), previous_sha = $2, updated_at = now() WHERE id = $1",
            )
            .bind(id)
            .bind(prev.as_deref())
            .execute(c)
            .await
            .map(drop)
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
        let change = progress(key, DeploymentStatus::Running, false, None, None);
        Ok(Settle::Commit(
            record(change),
            Claim::Run(Claimed {
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
            }),
        ))
    }
    .await;
    tx.settle("deployment.claim", outcome).await
}

/// How a running deployment ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finish {
    pub status: DeploymentStatus,
    pub error_code: Option<ErrorCode>,
    pub failed_target: Option<String>,
}

/// Record the end of running deployment `id` with its audit row; a success
/// also keeps the CI artifacts of the deployed SHA's pipelines
/// (`ci_artifacts.keep`, plan 14 §9 #6). `false`: it was not running
/// (nothing written — the sweep ended it first).
pub async fn finish(
    db: &Db,
    request_id: &RequestId,
    id: i64,
    end: &Finish,
) -> Result<bool, ForgeError> {
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
    let ctx = system(request_id);
    let mut tx = ChangeTx::begin(db, &ctx).await?;
    let outcome = async {
        let done: Option<(i64, String, String)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "UPDATE forge.deployments SET status = $2, error_code = $3, \
                       failed_target = $4, finished_at = now(), updated_at = now() \
                     WHERE id = $1 AND status = 'running' RETURNING project_id, sha, tier",
                )
                .bind(id)
                .bind(end.status.as_str())
                .bind(end.error_code.map(|c| c.id()))
                .bind(end.failed_target.as_deref())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some((project, sha, tier)) = done else {
            return Ok(Settle::Discard(false));
        };
        if end.status == DeploymentStatus::Succeeded {
            tx.step(|c| async move {
                sqlx::query(
                    "UPDATE forge.ci_artifacts SET keep = true WHERE project_id = $1 AND NOT keep \
                       AND pipeline_id IN (SELECT id FROM forge.ci_pipelines \
                         WHERE project_id = $1 AND sha = $2)",
                )
                .bind(project)
                .bind(&sha)
                .execute(c)
                .await
                .map(drop)
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        }
        let change = progress(
            (project, id, tier_of(&tier)?),
            end.status,
            false,
            end.error_code,
            end.failed_target.clone(), // clone: the audit row owns it
        );
        Ok(Settle::Commit(record(change), true))
    }
    .await;
    tx.settle("deployment.finished", outcome).await
}

/// The worker's heartbeat on running deployment `id` (`false`: it is no
/// longer running).
pub async fn heartbeat(db: &Db, id: i64) -> Result<bool, ForgeError> {
    const QUERY: &str = "deploy.heartbeat";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.deployments SET heartbeat_at = now() \
             WHERE id = $1 AND status = 'running'",
        )
        .bind(id)
        .execute(db.write_pool())
        .await
        .map(|r| r.rows_affected() == 1)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// One audit row per changed deployment: the first is the change's own,
/// the others its `affected` rows.
fn many(changes: Vec<DeployChange>) -> Option<crate::change::ChangeRecord> {
    let mut iter = changes.into_iter();
    let first = iter.next()?;
    let mut rec = record(first);
    rec.affected = iter
        .map(|c| {
            let project = c.project();
            let details = forge_core::AuditDetails::Deploy(c);
            AuditEntry {
                action: details.action(),
                outcome: Outcome::Succeeded,
                resource_type: super::RESOURCE,
                resource_id: Some(project.get()),
                project_id: Some(project),
                error_code: None,
                details: details.to_value(),
            }
        })
        .collect();
    Some(rec)
}

/// The most rows one sweep ends.
const SWEEP_BATCH: i64 = 50;

/// Requests that waited for an approval longer than their environment's
/// `approval_ttl_minutes` end `blocked` with `FG-DEPLOY-0001`, audited.
pub async fn block_unapproved(
    db: &Db,
    request_id: &RequestId,
) -> Result<Vec<(i64, i64, DeployTier)>, ForgeError> {
    const QUERY: &str = "deploy.block_unapproved";
    let ctx = system(request_id);
    let mut tx = ChangeTx::begin(db, &ctx).await?;
    let outcome = async {
        let rows: Vec<(i64, i64, String)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "UPDATE forge.deployments d SET status = 'blocked', error_code = $1, \
                       finished_at = now(), updated_at = now() \
                     FROM forge.deploy_environments e \
                     WHERE e.id = d.environment_id AND d.id IN (SELECT d2.id \
                       FROM forge.deployments d2 JOIN forge.deploy_environments e2 \
                         ON e2.id = d2.environment_id \
                       WHERE d2.status = 'pending_approval' \
                         AND d2.created_at + make_interval(mins => e2.approval_ttl_minutes) \
                           <= now() ORDER BY d2.id LIMIT $2) \
                     RETURNING d.id, d.project_id, d.tier",
                )
                .bind(ErrorCode::DeployApprovalRequired.id())
                .bind(SWEEP_BATCH)
                .fetch_all(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let rows = rows
            .into_iter()
            .map(|(id, p, t)| Ok((id, p, tier_of(&t)?)))
            .collect::<Result<Vec<_>, ForgeError>>()?;
        let changes = rows
            .iter()
            .map(|(id, p, t)| {
                progress(
                    (*p, *id, *t),
                    DeploymentStatus::Blocked,
                    false,
                    Some(ErrorCode::DeployApprovalRequired),
                    None,
                )
            })
            .collect();
        match many(changes) {
            Some(rec) => Ok(Settle::Commit(rec, rows)),
            None => Ok(Settle::Discard(rows)),
        }
    }
    .await;
    tx.settle("deployment.blocked", outcome).await
}

/// Running deployments whose heartbeat is older than `stale_secs` (their
/// worker stopped, or could not record the end) end `failed` with
/// `FG-DEPLOY-0012`, audited (`deployment.abandoned`); never rolled back.
pub async fn abandon_stale(
    db: &Db,
    request_id: &RequestId,
    stale_secs: u32,
) -> Result<Vec<(i64, i64, DeployTier)>, ForgeError> {
    const QUERY: &str = "deploy.abandon_stale";
    let ctx = system(request_id);
    let mut tx = ChangeTx::begin(db, &ctx).await?;
    let outcome = async {
        let rows: Vec<(i64, i64, String)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "UPDATE forge.deployments SET status = 'failed', error_code = $1, \
                       finished_at = now(), updated_at = now() \
                     WHERE id IN (SELECT id FROM forge.deployments WHERE status = 'running' \
                       AND coalesce(heartbeat_at, started_at) < now() - make_interval(secs => $2) \
                       ORDER BY id LIMIT $3) \
                     RETURNING id, project_id, tier",
                )
                .bind(ErrorCode::DeploymentAbandoned.id())
                .bind(f64::from(stale_secs))
                .bind(SWEEP_BATCH)
                .fetch_all(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let rows = rows
            .into_iter()
            .map(|(id, p, t)| Ok((id, p, tier_of(&t)?)))
            .collect::<Result<Vec<_>, ForgeError>>()?;
        let changes = rows
            .iter()
            .map(|(id, p, t)| {
                progress(
                    (*p, *id, *t),
                    DeploymentStatus::Failed,
                    true,
                    Some(ErrorCode::DeploymentAbandoned),
                    None,
                )
            })
            .collect();
        match many(changes) {
            Some(rec) => Ok(Settle::Commit(rec, rows)),
            None => Ok(Settle::Discard(rows)),
        }
    }
    .await;
    tx.settle("deployment.abandoned", outcome).await
}

/// Deployments `running` without a heartbeat for more than `minutes`
/// (`forge doctor`).
pub async fn stuck_running(db: &Db, minutes: i32) -> Result<Vec<i64>, ForgeError> {
    const QUERY: &str = "deploy.stuck_running";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT id FROM forge.deployments WHERE status = 'running' \
               AND coalesce(heartbeat_at, started_at) < now() - make_interval(mins => $1) \
             ORDER BY id LIMIT 100",
        )
        .bind(minutes)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}
