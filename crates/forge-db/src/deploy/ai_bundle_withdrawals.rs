//! Plan 17 P17-7b (ADR-0031 Amendment P17-7b): the withdrawal of a bad
//! `pfs-ai-bundle` release — a row of its own in
//! `forge.ai_bundle_withdrawals`, since a release never changes — and the
//! board the releases screen reads (each release's state on each stage).

use forge_core::deploy::{DeployChange, DeployTier, DeploymentStatus};
use forge_core::{ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use super::super::{caller_of, record, status_of, tier_of, TS};
use super::{key_not_revoked_sql, MAX_RELEASES};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::pipelines::decided_facts;
use crate::projects::ProjectUpdateFacts;
use crate::{query_error, read_query_error, Db};

/// The longest withdrawal reason (characters; the table's CHECK too).
pub const MAX_WITHDRAW_REASON: usize = 500;

/// Whether `reason` may be a withdrawal's: 1..=500 characters, not only
/// blanks, no control character (one line; the table checks it again).
pub fn withdraw_reason_valid(reason: &str) -> bool {
    !reason.trim().is_empty()
        && reason.chars().count() <= MAX_WITHDRAW_REASON
        && !reason.chars().any(char::is_control)
}

/// A release's withdrawal as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWithdrawal {
    pub id: i64,
    pub release_id: i64,
    pub project_id: i64,
    pub version: String,
    pub reason: String,
    pub withdrawn_by: i64,
    pub withdrawn_at: String,
}

type WithdrawalRow = (i64, i64, i64, String, String, i64, String);

fn withdrawal_of(r: WithdrawalRow) -> StoredWithdrawal {
    StoredWithdrawal {
        id: r.0,
        release_id: r.1,
        project_id: r.2,
        version: r.3,
        reason: r.4,
        withdrawn_by: r.5,
        withdrawn_at: r.6,
    }
}

fn withdrawal_columns() -> String {
    format!(
        "w.id, w.release_id, w.project_id, r.version, w.reason, w.withdrawn_by, \
         to_char(w.withdrawn_at AT TIME ZONE 'UTC', '{TS}')"
    )
}

/// The SQL condition "release `r` was not withdrawn".
pub(crate) fn not_withdrawn_sql(r: &str) -> String {
    format!("NOT EXISTS (SELECT 1 FROM forge.ai_bundle_withdrawals w WHERE w.release_id = {r}.id)")
}

/// The withdrawals of `project`'s newest releases (the releases
/// [`super::list_releases`] answers), newest release first.
pub async fn withdrawals(db: &Db, project: ProjectId) -> Result<Vec<StoredWithdrawal>, ForgeError> {
    const QUERY: &str = "ai_bundle.withdrawals";
    let rows: Vec<WithdrawalRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ai_bundle_withdrawals w \
                   JOIN forge.ai_bundle_releases r ON r.id = w.release_id \
                 WHERE r.project_id = $1 AND r.id IN (SELECT x.id \
                   FROM forge.ai_bundle_releases x WHERE x.project_id = $1 \
                   ORDER BY x.id DESC LIMIT $2) \
                 ORDER BY r.id DESC",
                withdrawal_columns()
            ))
            .bind(project.get())
            .bind(MAX_RELEASES)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows.into_iter().map(withdrawal_of).collect())
}

/// The withdrawal of release `version` of `project` (`None`: not
/// withdrawn, or no such release).
pub async fn withdrawal(
    db: &Db,
    project: ProjectId,
    version: &str,
) -> Result<Option<StoredWithdrawal>, ForgeError> {
    const QUERY: &str = "ai_bundle.withdrawal";
    let row: Option<WithdrawalRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ai_bundle_withdrawals w \
                   JOIN forge.ai_bundle_releases r ON r.id = w.release_id \
                 WHERE r.project_id = $1 AND r.version = $2",
                withdrawal_columns()
            ))
            .bind(project.get())
            .bind(version)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(withdrawal_of))
}

fn withdraw_refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::BundleWithdrawRefused).with_context("reason", reason)
}

/// Mark release `version` of `project` withdrawn for the caller, decided
/// again on the locked project and caller (`decide`: forge-deploy
/// reconfirms `authorize()`): one withdrawal row and one
/// `ai_asset_release.withdrawn` audit row. Errors (nothing written):
/// `decide`'s, `FG-AIASSET-0010 reason=reason_invalid` /
/// `release_not_found` / `already_withdrawn` / `rollback_target_running`
/// (a running deployment would roll back to it on a failure), the
/// database's.
pub async fn withdraw_bundle_decided(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    version: &str,
    reason: &str,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<(), ForgeError>,
) -> Result<StoredWithdrawal, ForgeError> {
    const QUERY: &str = "ai_bundle.withdraw";
    let user = caller_of(ctx)?;
    if !withdraw_reason_valid(reason) {
        return Err(withdraw_refused("reason_invalid"));
    }
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        decide(&decided_facts(&mut tx, project, UserId::new(user)).await?)?;
        let owned = version.to_owned();
        let release: Option<(i64, String)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT id, commit_sha FROM forge.ai_bundle_releases \
                     WHERE project_id = $1 AND version = $2",
                )
                .bind(project.get())
                .bind(owned)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some((release_id, sha)) = release else {
            return Err(withdraw_refused("release_not_found"));
        };
        // Review P17-7b: the project's environments are locked (the lock
        // the worker's claim takes, in id order), so a claim that is
        // deciding now either commits first — and is seen below — or sees
        // this withdrawal and blocks the deployment / never picks the
        // release as its previous version.
        tx.step(|c| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM forge.deploy_environments WHERE project_id = $1                  ORDER BY id FOR UPDATE",
            )
            .bind(project.get())
            .fetch_all(c)
            .await
            .map(drop)
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
        // A running deployment that would roll back to it on a failure
        // fixed its previous version when it started: wait for its end.
        let owned = sha.clone(); // clone: the audit row keeps the SHA
        let in_use: Option<String> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT tier FROM forge.deployments WHERE project_id = $1                        AND status = 'running' AND previous_sha = $2 LIMIT 1",
                )
                .bind(project.get())
                .bind(owned)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        if let Some(tier) = in_use {
            return Err(withdraw_refused("rollback_target_running").with_context("environment", tier));
        }
        let owned = reason.to_owned();
        // One per release (the UNIQUE): a second withdrawal, even a racing
        // one, inserts nothing.
        let inserted: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "INSERT INTO forge.ai_bundle_withdrawals (release_id, project_id, reason, \
                       withdrawn_by) VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (release_id) DO NOTHING RETURNING id",
                )
                .bind(release_id)
                .bind(project.get())
                .bind(owned)
                .bind(user)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some(id) = inserted else {
            return Err(withdraw_refused("already_withdrawn"));
        };
        let sql = format!(
            "SELECT {} FROM forge.ai_bundle_withdrawals w \
               JOIN forge.ai_bundle_releases r ON r.id = w.release_id WHERE w.id = $1",
            withdrawal_columns()
        );
        let row: WithdrawalRow = tx
            .step(|c| async move {
                sqlx::query_as(&sql)
                    .bind(id)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let stored = withdrawal_of(row);
        let change = DeployChange::BundleWithdrawn {
            project,
            release: release_id,
            version: stored.version.clone(), // clone: the audit row owns it
            sha,
            reason: stored.reason.clone(), // clone: as version
        };
        Ok(Settle::Commit(record(change), stored))
    }
    .await;
    tx.settle("ai_asset_release.withdrawn", outcome).await
}

/// Whether the release of `project` at `sha` was withdrawn, in the
/// caller's transaction (`false`: no release there, or not withdrawn).
pub(crate) async fn withdrawn_at(
    tx: &mut ChangeTx,
    project: ProjectId,
    sha: &str,
    query: &'static str,
) -> Result<bool, ForgeError> {
    let owned = sha.to_owned();
    tx.step(|c| async move {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.ai_bundle_withdrawals w \
               JOIN forge.ai_bundle_releases r ON r.id = w.release_id \
             WHERE r.project_id = $1 AND r.commit_sha = $2)",
        )
        .bind(project.get())
        .bind(owned)
        .fetch_one(c)
        .await
        .map_err(|e| query_error(query, e))
    })
    .await
}

/// A release's state on one stage: its newest deployment there, and
/// whether one succeeded there with a person's approval (what the next
/// stage and a rollback count).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseStage {
    pub release_id: i64,
    pub tier: DeployTier,
    pub deployment_id: Option<i64>,
    pub status: Option<DeploymentStatus>,
    pub approved_success: bool,
}

/// What the releases screen reads besides the releases: each release's
/// state on each stage, what runs on each stage (the SHA of its last
/// succeeded deployment) and the releases signed by a revoked key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReleaseBoard {
    pub stages: Vec<ReleaseStage>,
    pub running: Vec<(DeployTier, String)>,
    pub key_revoked: Vec<i64>,
}

type StageRow = (i64, String, Option<i64>, Option<String>, bool);

/// The board of `project`'s newest releases (the ones
/// [`super::list_releases`] answers; three stages each).
pub async fn release_board(db: &Db, project: ProjectId) -> Result<ReleaseBoard, ForgeError> {
    const QUERY: &str = "ai_bundle.board";
    let rows: Vec<StageRow> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT r.id, t.tier, last.id, last.status, EXISTS (SELECT 1 \
                   FROM forge.deployments d WHERE d.project_id = r.project_id \
                     AND d.tier = t.tier AND d.sha = r.commit_sha AND d.status = 'succeeded' \
                     AND d.approved_by IS NOT NULL) \
                 FROM (SELECT x.id, x.project_id, x.commit_sha FROM forge.ai_bundle_releases x \
                   WHERE x.project_id = $1 ORDER BY x.id DESC LIMIT $2) r \
                 CROSS JOIN (VALUES ('dev'), ('stg'), ('prod')) AS t (tier) \
                 LEFT JOIN LATERAL (SELECT d.id, d.status FROM forge.deployments d \
                   WHERE d.project_id = r.project_id AND d.tier = t.tier \
                     AND d.sha = r.commit_sha ORDER BY d.id DESC LIMIT 1) last ON true \
                 ORDER BY r.id DESC, t.tier",
            )
            .bind(project.get())
            .bind(MAX_RELEASES)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let mut stages = Vec::with_capacity(rows.len());
    for (release_id, tier, deployment_id, status, approved_success) in rows {
        stages.push(ReleaseStage {
            release_id,
            tier: tier_of(&tier)?,
            deployment_id,
            status: status.as_deref().map(status_of).transpose()?,
            approved_success,
        });
    }
    let running: Vec<(String, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT DISTINCT ON (d.tier) d.tier, d.sha FROM forge.deployments d \
                 WHERE d.project_id = $1 AND d.status = 'succeeded' \
                 ORDER BY d.tier, d.finished_at DESC, d.id DESC",
            )
            .bind(project.get())
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let running = running
        .into_iter()
        .map(|(tier, sha)| Ok((tier_of(&tier)?, sha)))
        .collect::<Result<Vec<_>, ForgeError>>()?;
    let key_revoked: Vec<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar(&format!(
                "SELECT r.id FROM forge.ai_bundle_releases r \
                 WHERE r.project_id = $1 AND NOT {}",
                key_not_revoked_sql("r")
            ))
            .bind(project.get())
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(ReleaseBoard {
        stages,
        running,
        key_revoked,
    })
}
