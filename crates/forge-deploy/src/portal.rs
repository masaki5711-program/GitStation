//! The PFS portal's `deploy-approvals` widget and
//! `attention-count?scope=deploy` (widget design P2b §3.3, §3.5;
//! ADR-0036): the deployments waiting for an approval the person can give
//! now.
//!
//! "Can give" is the approval route's own decision, never a rule of its
//! own (ADR-0038 step C):
//!
//! 1. each project is decided first — `PortalWidgetRead` on the project as
//!    loaded for the person (read like a session without administrator
//!    rights would); a project they cannot read is never a candidate;
//! 2. forge-db names the deployments of those projects that are
//!    `pending_approval` inside their approval window (candidates only);
//! 3. each is tried as the approval route decides it: the environment's
//!    approvers (named users, groups, organisation selectors) loaded by
//!    [`Deploy::approver_gate`] for the person — and, where the person
//!    holds a delegation, for each delegator — then `authorize()`'s
//!    `DeployApprove` for the person as a web session without
//!    administrator rights (`forge_auth::portal_reader`): never the
//!    requester nor anyone behind the SHA, the role the gate asks of the
//!    person and of the delegator.
//!
//! A deployment the person (or the person an approval would count for)
//! already approved is left out: the route would answer `FG-DEPLOY-0010
//! already_approved`. A refusal is a fact, never an error of the widget.

use forge_auth::{prove, Action, Authorized, DeploymentCtx, LoadedProject, PortalWidgetCtx};
use forge_core::deploy::DeployTier;
use forge_core::{ProjectId, ProjectRecord, UserId};
use forge_db::deploy::{PendingDeployment, MAX_PENDING};
use forge_db::portal::{MAX_OPTIONS, MAX_STATUS_ITEMS};
use forge_errors::ForgeError;

use crate::{loaded_deployment, require, Deploy, StoredDeployment};

/// The most delegators tried for one person (the delegations naming them
/// are read at most 100; a person approving for more than a few at once
/// is not a home-page case).
pub const MAX_DELEGATORS: usize = 10;

/// One deployment the person can approve now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalApproval {
    /// The project's path (`group/project`).
    pub path: String,
    pub deployment: StoredDeployment,
    /// The end of the freeze window that holds now (UTC); `None`: none.
    pub frozen_until: Option<String>,
    /// Seconds since it was requested.
    pub waiting_s: i64,
    /// Only as a delegate (for a delegator), not for themself.
    pub as_delegate: bool,
}

/// What the widget shows: the first `limit` approvals (oldest first) and
/// how many there are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalApprovals {
    pub items: Vec<PortalApproval>,
    pub count: i64,
    /// More waiting deployments than were read ([`MAX_PENDING`]): the
    /// count may be low, said as `n+`.
    pub truncated: bool,
}

impl Deploy {
    /// The deployments of `projects` (the setting, at most 24; `None`: the
    /// projects the person holds a role on, at most 200) in `tiers` that
    /// the person of `proof` can approve now, oldest first (module docs).
    /// `limit` 0 reads the count only.
    pub async fn portal_approvals(
        &self,
        proof: &Authorized<PortalWidgetCtx>,
        projects: Option<&[ProjectId]>,
        tiers: &[DeployTier],
        limit: usize,
    ) -> Result<PortalApprovals, ForgeError> {
        require(proof.action(), Action::PortalWidgetRead)?;
        let reader = forge_auth::portal_reader(proof)?;
        let me = *proof.id();
        let candidates: Vec<ProjectId> = match projects {
            Some(asked) => asked.iter().copied().take(MAX_STATUS_ITEMS).collect(),
            None => forge_db::portal::member_projects(&self.db, me, MAX_OPTIONS).await?,
        };
        let visible = self.portal_visible(proof, &candidates).await?;
        let ids: Vec<ProjectId> = visible.iter().map(|r| r.id).collect();
        let pending =
            forge_db::deploy::pending_approvals(&self.db, me, &ids, tiers, MAX_PENDING).await?;
        let truncated = i64::try_from(pending.len()).unwrap_or(i64::MAX) >= MAX_PENDING;
        let delegators: Vec<UserId> = if pending.is_empty() {
            Vec::new()
        } else {
            forge_db::review::delegations_to(&self.db, me)
                .await?
                .into_iter()
                .filter(|d| d.active)
                .map(|d| d.delegator)
                .take(MAX_DELEGATORS)
                .collect()
        };
        let mut out = PortalApprovals {
            items: Vec::new(),
            count: 0,
            truncated,
        };
        for p in pending {
            let Some(record) = visible
                .iter()
                .find(|r| r.id.get() == p.deployment.project_id)
            else {
                continue;
            };
            let Some(as_delegate) = self
                .can_approve(&reader, record, &p, me, &delegators)
                .await?
            else {
                continue;
            };
            out.count = out.count.saturating_add(1);
            if out.items.len() < limit {
                out.items.push(PortalApproval {
                    path: record.path.full_path(),
                    frozen_until: p.frozen_until,
                    waiting_s: p.waiting_s,
                    deployment: p.deployment,
                    as_delegate,
                });
            }
        }
        Ok(out)
    }

    /// The readable records of `ids`, in their order: `PortalWidgetRead`
    /// on each project as loaded for the person.
    async fn portal_visible(
        &self,
        proof: &Authorized<PortalWidgetCtx>,
        ids: &[ProjectId],
    ) -> Result<Vec<ProjectRecord>, ForgeError> {
        let mut records = forge_db::lists::project_records(&self.db, ids, *proof.id()).await?;
        records.sort_by_key(|r| ids.iter().position(|id| *id == r.id).unwrap_or(usize::MAX));
        Ok(records
            .into_iter()
            .filter(|r| {
                prove(
                    proof.principal(),
                    Action::PortalWidgetRead,
                    forge_auth::ProjectCtx::new(loaded(r)),
                )
                .is_ok()
            })
            .collect())
    }

    /// Whether `me` can approve `p` now: `Some(false)` for themself,
    /// `Some(true)` only for one of `delegators`, `None` not at all —
    /// `authorize()`'s `DeployApprove` for `reader` on the deployment
    /// loaded with the approvers' gate, as the approval route loads it.
    async fn can_approve(
        &self,
        reader: &forge_auth::Principal,
        record: &ProjectRecord,
        p: &PendingDeployment,
        me: UserId,
        delegators: &[UserId],
    ) -> Result<Option<bool>, ForgeError> {
        // One approval per approver: once the person approved (for anyone),
        // the route refuses another; never the requester nor anyone behind
        // the SHA (`authorize()` refuses them first — checked here only to
        // spare the gate's reads; `authorize()` decides again below).
        let d = &p.deployment;
        let behind =
            |u: UserId| d.requested_by == Some(u.get()) || d.change_authors.contains(&u.get());
        if p.approvers.contains(&me.get()) || behind(me) {
            return Ok(None);
        }
        let ways = std::iter::once(None).chain(delegators.iter().copied().map(Some));
        for on_behalf_of in ways {
            let counts_for = on_behalf_of.unwrap_or(me);
            if p.approved_for.contains(&counts_for.get()) || behind(counts_for) {
                continue;
            }
            let gate = match self.approver_gate(record.id, d, me, on_behalf_of).await {
                Ok(gate) => gate,
                // The environment went away since the candidates were read:
                // nothing to approve there (the route answers 404).
                Err(e) if e.code() == forge_errors::ErrorCode::NotFound => return Ok(None),
                Err(e) => return Err(e),
            };
            let ctx = DeploymentCtx::new(loaded(record), loaded_deployment(d, gate.fact));
            if prove(reader, Action::DeployApprove, ctx).is_ok() {
                return Ok(Some(on_behalf_of.is_some()));
            }
        }
        Ok(None)
    }
}

/// The project as loaded for the person (`authorize()`'s facts).
fn loaded(r: &ProjectRecord) -> LoadedProject {
    LoadedProject {
        id: r.id,
        visibility: r.access.visibility,
        role: r.access.role,
        pending_deletion: r.access.pending_deletion,
    }
}
