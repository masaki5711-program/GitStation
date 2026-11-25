//! The deployments waiting for an approval (widget design P2b §3.5, the
//! PFS portal's `deploy-approvals` and `attention-count?scope=deploy`):
//! candidates only. The caller (forge-deploy `portal`) decides each one
//! for the person with `authorize()` — the project first, then the
//! approval trial (`DeployApprove`, with the environment's approvers as
//! step C loads them) — so nothing here decides who may approve.

use forge_core::deploy::DeployTier;
use forge_core::{ProjectId, UserId};
use forge_errors::ForgeError;

use super::deployments::{deployment_columns, deployment_of, DeploymentRow};
use super::{StoredDeployment, TS};
use crate::{query_error, Db};

/// The most waiting deployments read for one person (a count reaching it
/// is shown `n+`).
pub const MAX_PENDING: i64 = 100;

/// A deployment waiting for approvals, with what the widget shows of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDeployment {
    /// With the people behind its SHA (`authorize()` refuses them).
    pub deployment: StoredDeployment,
    /// The end of the environment's freeze window that holds now (UTC,
    /// RFC 3339); `None`: not frozen.
    pub frozen_until: Option<String>,
    /// Seconds since it was requested.
    pub waiting_s: i64,
    /// Who already approved it (one approval per approver).
    pub approvers: Vec<i64>,
    /// The people an approval already counts for (one per person:
    /// `coalesce(on_behalf_of, approver_id)`).
    pub approved_for: Vec<i64>,
}

#[derive(sqlx::FromRow)]
struct PendingRow {
    #[sqlx(flatten)]
    row: DeploymentRow,
    frozen_until: Option<String>,
    waiting_s: i64,
    approvers: Vec<i64>,
    approved_for: Vec<i64>,
}

/// The deployments of `projects` in `tiers` that are `pending_approval`
/// inside their approval window, oldest first (what has waited longest),
/// at most `limit` (≤ [`MAX_PENDING`]). Those `viewer` requested or
/// already approved are left out here (the approval route refuses both,
/// for themself and as a delegate), so they never fill the limit. The
/// write pool: the people behind a SHA are read by email, which the read
/// pool cannot see.
pub async fn pending_approvals(
    db: &Db,
    viewer: UserId,
    projects: &[ProjectId],
    tiers: &[DeployTier],
    limit: i64,
) -> Result<Vec<PendingDeployment>, ForgeError> {
    const QUERY: &str = "deploy.pending_approvals";
    if projects.is_empty() || tiers.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = projects.iter().map(|p| p.get()).collect();
    let tiers: Vec<&str> = tiers.iter().map(|t| t.as_str()).collect();
    let sql = format!(
        "SELECT {}, \
           (SELECT to_char(max(upper(w)) AT TIME ZONE 'UTC', '{TS}') \
              FROM unnest(e.freeze_windows) AS w WHERE w @> now()) AS frozen_until, \
           GREATEST(0, extract(epoch FROM now() - d.created_at))::bigint AS waiting_s, \
           ARRAY(SELECT a.approver_id FROM forge.deployment_approvals a \
                  WHERE a.deployment_id = d.id ORDER BY a.id) AS approvers, \
           ARRAY(SELECT coalesce(a.on_behalf_of, a.approver_id) \
                  FROM forge.deployment_approvals a \
                  WHERE a.deployment_id = d.id ORDER BY a.id) AS approved_for \
         FROM forge.deployments d JOIN forge.deploy_environments e ON e.id = d.environment_id \
         WHERE d.project_id = ANY($1) AND d.status = 'pending_approval' AND d.tier = ANY($2) \
           AND d.created_at + make_interval(mins => e.approval_ttl_minutes) > now() \
           AND d.requested_by IS DISTINCT FROM $4 \
           AND NOT EXISTS (SELECT 1 FROM forge.deployment_approvals m \
                            WHERE m.deployment_id = d.id AND m.approver_id = $4) \
         ORDER BY d.created_at, d.id LIMIT $3",
        deployment_columns("d", true)
    );
    let rows: Vec<PendingRow> = db
        .within(QUERY, async {
            sqlx::query_as(&sql)
                .bind(&ids)
                .bind(&tiers)
                .bind(limit.clamp(1, MAX_PENDING))
                .bind(viewer.get())
                .fetch_all(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok(PendingDeployment {
                deployment: deployment_of(r.row)?,
                frozen_until: r.frozen_until,
                waiting_s: r.waiting_s,
                approvers: r.approvers,
                approved_for: r.approved_for,
            })
        })
        .collect()
}
