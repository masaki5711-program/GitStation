//! The context of a deployment's approval (M4 C11, ADR-0031 decision 6):
//! one deployment of one project, as loaded for the caller. Sealed like
//! every [`ResourceCtx`]: private fields, built from one loaded project and
//! one loaded deployment row, so the id a proof carries and the facts it
//! was decided on cannot disagree.

use forge_core::deploy::DeployTier;
use forge_core::{ProjectId, UserId};

use crate::model::{DeploymentView, ProjectView, Resource};
use crate::prove::{sealed, LoadedProject, ResourceCtx};

/// One deployment as loaded (one row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedDeployment {
    pub id: i64,
    pub tier: DeployTier,
    /// `None`: requested by the system (an automatic deployment).
    pub requested_by: Option<UserId>,
    /// The people behind the SHA: merge request authors, pushers, the
    /// commit author and committer (as accounts).
    pub change_authors: Vec<UserId>,
}

/// A deployment of a project (`DeployApprove`). The id is the project and
/// the deployment together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentCtx {
    project: LoadedProject,
    deployment: LoadedDeployment,
}

impl DeploymentCtx {
    pub fn new(project: LoadedProject, deployment: LoadedDeployment) -> Self {
        Self {
            project,
            deployment,
        }
    }

    pub fn deployment(&self) -> &LoadedDeployment {
        &self.deployment
    }
}

impl sealed::Sealed for DeploymentCtx {}

impl ResourceCtx for DeploymentCtx {
    type Id = (ProjectId, i64);
    fn resource(&self) -> Resource {
        Resource::Deployment(DeploymentView {
            project: ProjectView {
                visibility: self.project.visibility,
                role: self.project.role,
                pending_deletion: self.project.pending_deletion,
            },
            tier: self.deployment.tier,
            requested_by: self.deployment.requested_by,
            change_authors: self.deployment.change_authors.clone(), // clone: the resource owns it
        })
    }
    fn id(&self) -> (ProjectId, i64) {
        (self.project.id, self.deployment.id)
    }
}
