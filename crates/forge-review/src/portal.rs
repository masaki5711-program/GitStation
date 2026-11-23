//! The PFS portal's `merge-queue` and `spec-status` widgets (widget design
//! P2a §3.6, §3.7; ADR-0036). Every project is decided with `authorize()`
//! (`PortalWidgetRead` on the project as loaded for the person: read like
//! the person would in a session without administrator rights) before
//! anything of it is read; the SQL only names candidates
//! (`forge_db::portal`). What is shown of a readable project — its queues
//! (`MergeQueueRead`, as `RepoBrowse`) and the counts of its open merge
//! requests and their checks (`MergeRequestRead`, as the project) — every
//! reader of the project may see, so no row is decided twice.

use forge_auth::{prove, Action, Authorized, LoadedProject, PortalWidgetCtx, ProjectCtx};
use forge_core::{ProjectId, ProjectRecord};
use forge_db::portal::{self as db_portal, MAX_OPTIONS, MAX_STATUS_ITEMS};
pub use forge_db::portal::{QueueStatus, SpecStatus, MAX_STATUS_COUNT};
use forge_errors::ForgeError;

use crate::{require, Review};

/// One queue the widget shows: the project's path and its facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalQueue {
    /// The project's path (`group/project`).
    pub path: String,
    pub status: QueueStatus,
}

/// One specification project the widget shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalSpec {
    pub path: String,
    pub status: SpecStatus,
}

/// `records` the person may read through the portal, in order.
fn visible(proof: &Authorized<PortalWidgetCtx>, records: Vec<ProjectRecord>) -> Vec<ProjectRecord> {
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

impl Review {
    /// The candidates: `projects` (the setting, at most 24) or the projects
    /// the person holds a role on (newest first, at most 200).
    async fn portal_candidates(
        &self,
        proof: &Authorized<PortalWidgetCtx>,
        projects: Option<&[ProjectId]>,
    ) -> Result<Vec<ProjectId>, ForgeError> {
        Ok(match projects {
            Some(asked) => asked.iter().copied().take(MAX_STATUS_ITEMS).collect(),
            None => db_portal::member_projects(&self.db, *proof.id(), MAX_OPTIONS).await?,
        })
    }

    /// The readable records of `chunk`, in its order.
    async fn portal_visible(
        &self,
        proof: &Authorized<PortalWidgetCtx>,
        chunk: &[ProjectId],
    ) -> Result<Vec<ProjectRecord>, ForgeError> {
        let mut records = forge_db::lists::project_records(&self.db, chunk, *proof.id()).await?;
        records.sort_by_key(|r| {
            chunk
                .iter()
                .position(|id| *id == r.id)
                .unwrap_or(usize::MAX)
        });
        Ok(visible(proof, records))
    }

    /// The `merge-queue` widget: the queues of `projects` (the setting;
    /// `None`: the projects the person holds a role on), each project
    /// decided first, in the order asked (newest first without a setting),
    /// `main` first within a project, at most 24. A project without a queue
    /// has none; one the person may not read is left out as an absent one
    /// (never a hint). The candidates are read 24 at a time and only until
    /// 24 queues are kept, so projects without a queue never push a queue
    /// out.
    pub async fn portal_merge_queues(
        &self,
        proof: &Authorized<PortalWidgetCtx>,
        projects: Option<&[ProjectId]>,
    ) -> Result<Vec<PortalQueue>, ForgeError> {
        require(proof.action(), Action::PortalWidgetRead)?;
        let ids = self.portal_candidates(proof, projects).await?;
        let mut shown: Vec<PortalQueue> = Vec::new();
        for chunk in ids.chunks(MAX_STATUS_ITEMS) {
            let visible = self.portal_visible(proof, chunk).await?;
            let visible_ids: Vec<ProjectId> = visible.iter().map(|r| r.id).collect();
            for status in db_portal::merge_queue_status(&self.db, &visible_ids).await? {
                let Some(record) = visible.iter().find(|r| r.id == status.project) else {
                    continue;
                };
                shown.push(PortalQueue {
                    path: record.path.full_path(),
                    status,
                });
            }
            if shown.len() >= MAX_STATUS_ITEMS {
                shown.truncate(MAX_STATUS_ITEMS);
                break;
            }
        }
        Ok(shown)
    }

    /// The `spec-status` widget: `projects` as asked (the setting: a
    /// specification project the person chose: any project they may read
    /// is shown even without a released specification), or
    /// without a setting the projects the person holds a role on that
    /// released a specification (newest first); each decided first, at
    /// most 24. One the person may not read is left out as an absent one.
    pub async fn portal_spec_status(
        &self,
        proof: &Authorized<PortalWidgetCtx>,
        projects: Option<&[ProjectId]>,
    ) -> Result<Vec<PortalSpec>, ForgeError> {
        require(proof.action(), Action::PortalWidgetRead)?;
        let ids = self.portal_candidates(proof, projects).await?;
        let mut shown: Vec<PortalSpec> = Vec::new();
        for chunk in ids.chunks(MAX_STATUS_ITEMS) {
            let candidates = if projects.is_some() {
                chunk.to_vec()
            } else {
                db_portal::spec_projects(&self.db, chunk).await?
            };
            let visible = self.portal_visible(proof, &candidates).await?;
            let visible_ids: Vec<ProjectId> = visible.iter().map(|r| r.id).collect();
            let statuses = db_portal::spec_status(&self.db, &visible_ids).await?;
            shown.extend(
                visible
                    .into_iter()
                    .zip(statuses)
                    .map(|(r, status)| PortalSpec {
                        path: r.path.full_path(),
                        status,
                    }),
            );
            if shown.len() >= MAX_STATUS_ITEMS {
                shown.truncate(MAX_STATUS_ITEMS);
                break;
            }
        }
        Ok(shown)
    }
}
