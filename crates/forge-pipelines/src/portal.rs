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

    /// One project a widget's setting names (the `menu`'s `project`), when
    /// the person may read it through the portal; `None` for one they may
    /// not read or that is not there (alike: never a hint).
    pub async fn portal_project(
        &self,
        proof: &Authorized<PortalWidgetCtx>,
        project: ProjectId,
    ) -> Result<Option<ProjectOption>, ForgeError> {
        require(proof.action(), Action::PortalWidgetRead)?;
        let records = forge_db::lists::project_records(&self.db, &[project], *proof.id()).await?;
        Ok(Self::visible(proof, records)
            .into_iter()
            .find(|r| r.id == project)
            .map(|r| ProjectOption {
                project: r.id,
                path: r.path.full_path(),
            }))
    }

    /// The widget's lamps: `projects` (the setting; `None`: the projects the
    /// person holds a role on), each decided, those whose lamp `keep`
    /// accepts, at most 24, in the order asked (newest first without a
    /// setting). A project asked for that the person may not read is left
    /// out, as an absent one (never a hint).
    ///
    /// The candidates are ids only (one statement, at most 200); their
    /// records and lamps are loaded 24 at a time and only until 24 are kept
    /// (review D2) — one load in the usual case. `keep` is applied before
    /// the cap (widget P1 review): a filtered widget (`only_problems`,
    /// `attention-count?scope=failed`) never loses a matching project to the
    /// projects ahead of it that do not match.
    pub async fn portal_lamps(
        &self,
        proof: &Authorized<PortalWidgetCtx>,
        projects: Option<&[ProjectId]>,
        show: Show,
        keep: fn(Lamp) -> bool,
    ) -> Result<Vec<ProjectLamp>, ForgeError> {
        require(proof.action(), Action::PortalWidgetRead)?;
        let user = *proof.id();
        let ids: Vec<ProjectId> = match projects {
            Some(asked) => asked.iter().copied().take(MAX_STATUS_ITEMS).collect(),
            None => db_portal::member_projects(&self.db, user, MAX_OPTIONS).await?,
        };
        let mut shown: Vec<ProjectLamp> = Vec::with_capacity(MAX_STATUS_ITEMS.min(ids.len()));
        for chunk in ids.chunks(MAX_STATUS_ITEMS) {
            let mut records = forge_db::lists::project_records(&self.db, chunk, user).await?;
            records.sort_by_key(|r| {
                chunk
                    .iter()
                    .position(|id| *id == r.id)
                    .unwrap_or(usize::MAX)
            });
            let visible = Self::visible(proof, records);
            let visible_ids: Vec<ProjectId> = visible.iter().map(|r| r.id).collect();
            let lamps = self.lamps_of(&visible_ids, show).await?;
            shown.extend(
                visible
                    .into_iter()
                    .zip(lamps)
                    .filter(|(_, (lamp, _))| keep(*lamp))
                    .map(|(r, (lamp, tier))| ProjectLamp {
                        project: r.id,
                        path: r.path.full_path(),
                        lamp,
                        tier,
                    }),
            );
            if shown.len() >= MAX_STATUS_ITEMS {
                shown.truncate(MAX_STATUS_ITEMS);
                break;
            }
        }
        Ok(shown)
    }

    /// The lamps of `ids`, in their order (one statement).
    async fn lamps_of(
        &self,
        ids: &[ProjectId],
        show: Show,
    ) -> Result<Vec<(Lamp, Option<String>)>, ForgeError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(match show {
            Show::Pipeline => db_portal::latest_pipelines(&self.db, ids, MAIN_REF)
                .await?
                .into_iter()
                .map(|(_, status)| (pipeline_lamp(status.as_deref()), None))
                .collect(),
            Show::Deploy => db_portal::latest_deployments(&self.db, ids)
                .await?
                .into_iter()
                .map(|(_, last)| match last {
                    Some((status, tier)) => (deploy_lamp(Some(&status)), Some(tier)),
                    None => (deploy_lamp(None), None),
                })
                .collect(),
        })
    }
}
