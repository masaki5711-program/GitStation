//! The PFS portal's `pipelines` widget and the project options of both
//! widgets (ADR-0036). Every project is decided with `authorize()`
//! (`PortalWidgetRead` on the project as loaded for the person: read like
//! the person would in a session without administrator rights) before
//! anything of it is shown; the SQL only names candidates
//! (`forge_db::portal`).

use forge_auth::{prove, Action, Authorized, LoadedProject, PortalWidgetCtx, ProjectCtx};
use forge_core::portal::{deploy_lamp, pipeline_lamp, Lamp};
use forge_core::{ProjectId, ProjectRecord};
use forge_db::portal::{self as db_portal, MAX_OPTIONS, MAX_STATUS_ITEMS};
use forge_errors::ForgeError;

use crate::{require, Pipelines};

/// The ref whose pipeline the widget shows (contract: "main").
pub const MAIN_REF: &str = "refs/heads/main";

/// What the `pipelines` widget shows of each project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    /// The newest pipeline of `main`.
    Pipeline,
    /// The newest deployment.
    Deploy,
}

/// One lamp of the widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLamp {
    pub project: ProjectId,
    /// The project's path (`group/project`).
    pub path: String,
    pub lamp: Lamp,
    /// The deployment's environment (`Show::Deploy` with one).
    pub tier: Option<String>,
}

/// One project a widget offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectOption {
    pub project: ProjectId,
    pub path: String,
}

impl Pipelines {
    /// `records` the person may read through the portal, in order.
    fn visible(
        proof: &Authorized<PortalWidgetCtx>,
        records: Vec<ProjectRecord>,
    ) -> Vec<ProjectRecord> {
        records
            .into_iter()
            .filter(|r| {
                let loaded = LoadedProject {
                    id: r.id,
                    visibility: r.access.visibility,
                    role: r.access.role,
                    pending_deletion: r.access.pending_deletion,
                };
                prove(
                    proof.principal(),
                    Action::PortalWidgetRead,
                    ProjectCtx::new(loaded),
                )
                .is_ok()
            })
            .collect()
    }

    /// The projects the widgets offer (`options/project`): where the person
    /// holds a role and may read, newest first, at most 200.
    pub async fn portal_projects(
        &self,
        proof: &Authorized<PortalWidgetCtx>,
    ) -> Result<Vec<ProjectOption>, ForgeError> {
        require(proof.action(), Action::PortalWidgetRead)?;
        let user = *proof.id();
        let ids = db_portal::member_projects(&self.db, user, MAX_OPTIONS).await?;
        let mut records = forge_db::lists::project_records(&self.db, &ids, user).await?;
        records.sort_by_key(|r| std::cmp::Reverse(r.id.get()));
        Ok(Self::visible(proof, records)
            .into_iter()
            .map(|r| ProjectOption {
                project: r.id,
                path: r.path.full_path(),
            })
            .collect())
    }

    /// The widget's lamps: `projects` (the setting; `None`: the projects the
    /// person holds a role on), each decided, at most 24, in the order
    /// asked (newest first without a setting). A project asked for that the
    /// person may not read is left out, as an absent one (never a hint).
    pub async fn portal_lamps(
        &self,
        proof: &Authorized<PortalWidgetCtx>,
        projects: Option<&[ProjectId]>,
        show: Show,
    ) -> Result<Vec<ProjectLamp>, ForgeError> {
        require(proof.action(), Action::PortalWidgetRead)?;
        let user = *proof.id();
        let ids: Vec<ProjectId> = match projects {
            Some(asked) => asked.iter().copied().take(MAX_STATUS_ITEMS).collect(),
            None => db_portal::member_projects(&self.db, user, MAX_OPTIONS).await?,
        };
        let mut records = forge_db::lists::project_records(&self.db, &ids, user).await?;
        records.sort_by_key(|r| ids.iter().position(|id| *id == r.id).unwrap_or(usize::MAX));
        let shown: Vec<ProjectRecord> = Self::visible(proof, records)
            .into_iter()
            .take(MAX_STATUS_ITEMS)
            .collect();
        let shown_ids: Vec<ProjectId> = shown.iter().map(|r| r.id).collect();
        let lamps: Vec<(Lamp, Option<String>)> = match show {
            Show::Pipeline => db_portal::latest_pipelines(&self.db, &shown_ids, MAIN_REF)
                .await?
                .into_iter()
                .map(|(_, status)| (pipeline_lamp(status.as_deref()), None))
                .collect(),
            Show::Deploy => db_portal::latest_deployments(&self.db, &shown_ids)
                .await?
                .into_iter()
                .map(|(_, last)| match last {
                    Some((status, tier)) => (deploy_lamp(Some(&status)), Some(tier)),
                    None => (deploy_lamp(None), None),
                })
                .collect(),
        };
        Ok(shown
            .into_iter()
            .zip(lamps)
            .map(|(r, (lamp, tier))| ProjectLamp {
                project: r.id,
                path: r.path.full_path(),
                lamp,
                tier,
            })
            .collect())
    }
}
