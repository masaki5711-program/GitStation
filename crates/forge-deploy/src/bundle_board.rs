//! Plan 17 P17-7b (ADR-0031 Amendment P17-7b): the withdrawal of a bad
//! `pfs-ai-bundle` release and the releases screen's overview.
//!
//! **Withdrawing** ([`Deploy::withdraw_release`], `AiAssetReleaseWithdraw`:
//! people only, Maintainer and above): one append-only row with the
//! reason, decided again on the locked project. From then on the release
//! is never requested on a stage (`FG-AIASSET-0009 release_withdrawn`),
//! approved, run by the worker (blocked), or gone back to by the manual
//! rollback or the worker's automatic one. A withdrawal is never undone:
//! a fixed bundle is a new release.
//!
//! **The overview** ([`Deploy::release_overview`], `DeployRead`): the
//! newest releases with their withdrawal, a revoked key, their state on
//! each stage and the one stage a request would be accepted on now
//! ([`next_stage`]), and per stage what runs and what a rollback would
//! request. The screen offers a button only where this says it would
//! pass, and only to a caller the API says may press it.

use forge_auth::{Action, Authorized, ProjectCtx};
use forge_core::deploy::{DeployTier, DeploymentStatus};
use forge_db::deploy::ai_bundle::{self as db, ReleaseStage, RollbackTarget, StoredWithdrawal};
use forge_errors::{ErrorCode, ForgeError};

use super::StoredRelease;
use crate::{caller, change_context, reconfirm, require, ChangeRequest, Deploy};

/// One release on the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRow {
    pub release: StoredRelease,
    pub withdrawal: Option<StoredWithdrawal>,
    pub key_revoked: bool,
    /// Shadow, canary, prod (in this order).
    pub stages: Vec<ReleaseStage>,
    /// The stage a request of this release would be accepted on now
    /// (`None`: withdrawn, a revoked key, one is under way, or done).
    pub next_stage: Option<DeployTier>,
}

/// One stage on the screen: the release that runs and the release a
/// rollback would request (versions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageRow {
    pub tier: DeployTier,
    pub running: Option<String>,
    pub rollback_to: Option<String>,
}

/// What [`Deploy::release_overview`] answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseOverview {
    pub releases: Vec<ReleaseRow>,
    pub stages: Vec<StageRow>,
}

/// The stages in order.
pub const STAGES: [DeployTier; 3] = [DeployTier::Dev, DeployTier::Stg, DeployTier::Prod];

fn moving(status: Option<DeploymentStatus>) -> bool {
    matches!(
        status,
        Some(
            DeploymentStatus::PendingApproval
                | DeploymentStatus::Queued
                | DeploymentStatus::Running
        )
    )
}

/// The stage a request of a release would be accepted on now, from its
/// stages (shadow → canary → prod, each after the one before succeeded
/// with an approval): the first stage it has not passed, unless a
/// deployment of it there is under way. `None` when `blocked` (withdrawn,
/// a revoked key) or when it passed prod.
pub fn next_stage(stages: &[ReleaseStage], blocked: bool) -> Option<DeployTier> {
    if blocked {
        return None;
    }
    for tier in STAGES {
        let here = stages.iter().find(|s| s.tier == tier);
        if here.is_some_and(|s| s.approved_success) {
            continue;
        }
        if moving(here.and_then(|s| s.status)) {
            return None;
        }
        return Some(tier);
    }
    None
}

fn withdraw_refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::BundleWithdrawRefused).with_context("reason", reason)
}

impl Deploy {
    /// `GET …/ai-asset-releases` (`DeployRead`): the releases screen.
    pub async fn release_overview(
        &self,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<ReleaseOverview, ForgeError> {
        require(proof.action(), Action::DeployRead)?;
        let project = *proof.id();
        let releases = db::list_releases(&self.db, project).await?;
        let mut withdrawals = db::withdrawals(&self.db, project).await?;
        let board = db::release_board(&self.db, project).await?;
        let mut stages = Vec::with_capacity(STAGES.len());
        for tier in STAGES {
            let (running, rollback_to) = match db::rollback_target(&self.db, project, tier).await? {
                RollbackTarget::NothingRunning => (None, None),
                RollbackTarget::NoEarlier { current } => (Some(current.version), None),
                RollbackTarget::To { current, target } => {
                    (Some(current.version), Some(target.version))
                }
            };
            stages.push(StageRow {
                tier,
                running,
                rollback_to,
            });
        }
        let rows = releases
            .into_iter()
            .map(|release| {
                let withdrawal = withdrawals
                    .iter()
                    .position(|w| w.release_id == release.id)
                    .map(|i| withdrawals.swap_remove(i));
                let key_revoked = board.key_revoked.contains(&release.id);
                let mut own: Vec<ReleaseStage> = board
                    .stages
                    .iter()
                    .filter(|s| s.release_id == release.id)
                    .cloned() // clone: each row owns its stages
                    .collect();
                own.sort_by_key(|s| STAGES.iter().position(|t| *t == s.tier));
                let next_stage = next_stage(&own, withdrawal.is_some() || key_revoked);
                ReleaseRow {
                    release,
                    withdrawal,
                    key_revoked,
                    stages: own,
                    next_stage,
                }
            })
            .collect();
        Ok(ReleaseOverview {
            releases: rows,
            stages,
        })
    }

    /// `GET …/ai-asset-releases/{version}` (`DeployRead`): the release's
    /// withdrawal (`None`: not withdrawn, or no such release).
    pub async fn release_withdrawal(
        &self,
        proof: &Authorized<ProjectCtx>,
        version: &str,
    ) -> Result<Option<StoredWithdrawal>, ForgeError> {
        require(proof.action(), Action::DeployRead)?;
        db::withdrawal(&self.db, *proof.id(), version).await
    }

    /// `POST …/ai-asset-releases/{version}/withdrawal`
    /// (`AiAssetReleaseWithdraw`): mark the release withdrawn, for good,
    /// with `reason` (module docs).
    pub async fn withdraw_release(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        version: &str,
        reason: &str,
    ) -> Result<StoredWithdrawal, ForgeError> {
        require(proof.action(), Action::AiAssetReleaseWithdraw)?;
        let user = caller(proof.principal(), Action::AiAssetReleaseWithdraw)?;
        let project = *proof.id();
        let outcome = if db::withdraw_reason_valid(reason) {
            let ctx = change_context(request, proof.principal(), user);
            db::withdraw_bundle_decided(&self.db, &ctx, project, version, reason, |facts| {
                reconfirm(proof, facts)
            })
            .await
        } else {
            Err(withdraw_refused("reason_invalid"))
        };
        match &outcome {
            Ok(stored) => tracing::info!(
                target: "forge_deploy",
                event = "forge.deploy.bundle_withdrawn",
                request_id = %request.request_id,
                project_id = project.get(),
                release_id = stored.release_id,
                version = %stored.version,
                reason_chars = stored.reason.chars().count(),
                user_id = user.get(),
                credential_id = request.credential_id,
                "ai asset release withdrawn"
            ),
            Err(e) if e.code() == ErrorCode::BundleWithdrawRefused => {
                let why = e
                    .context()
                    .iter()
                    .find(|(k, _)| *k == "reason")
                    .map(|(_, v)| v.clone()) // clone: logged below
                    .unwrap_or_default();
                tracing::info!(
                    target: "forge_deploy",
                    event = "forge.deploy.bundle_withdraw_refused",
                    error_code = ErrorCode::BundleWithdrawRefused.id(),
                    request_id = %request.request_id,
                    project_id = project.get(),
                    version = %version.chars().take(32).collect::<String>(),
                    reason = %why,
                    "ai asset release withdrawal refused"
                );
            }
            Err(_) => {}
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(tier: DeployTier, status: Option<DeploymentStatus>, ok: bool) -> ReleaseStage {
        ReleaseStage {
            release_id: 1,
            tier,
            deployment_id: status.map(|_| 9),
            status,
            approved_success: ok,
        }
    }

    #[test]
    fn next_stage_walks_shadow_canary_prod() {
        use DeployTier::{Dev, Prod, Stg};
        use DeploymentStatus as S;
        let none = [
            stage(Dev, None, false),
            stage(Stg, None, false),
            stage(Prod, None, false),
        ];
        assert_eq!(next_stage(&none, false), Some(Dev));
        assert_eq!(
            next_stage(&[], false),
            Some(Dev),
            "no row reads as never deployed"
        );
        assert_eq!(next_stage(&none, true), None, "withdrawn or revoked");
        let waiting = [stage(Dev, Some(S::PendingApproval), false)];
        assert_eq!(next_stage(&waiting, false), None, "one is under way");
        let failed = [stage(Dev, Some(S::Failed), false)];
        assert_eq!(
            next_stage(&failed, false),
            Some(Dev),
            "asked again after a failure"
        );
        let shadow = [
            stage(Dev, Some(S::Succeeded), true),
            stage(Stg, None, false),
        ];
        assert_eq!(next_stage(&shadow, false), Some(Stg));
        // An unapproved success does not open the next stage.
        let unapproved = [stage(Dev, Some(S::Succeeded), false)];
        assert_eq!(next_stage(&unapproved, false), Some(Dev));
        let done = [
            stage(Dev, Some(S::Succeeded), true),
            stage(Stg, Some(S::Succeeded), true),
            stage(Prod, Some(S::Succeeded), true),
        ];
        assert_eq!(next_stage(&done, false), None);
    }
}
