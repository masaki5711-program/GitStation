//! The contexts of the integration routes (M3 I1a, plan 11 §5 I1): one
//! integration of the administration ([`IntegrationCtx`], decided on the
//! instance) and one binding of a project ([`IntegrationBindingCtx`],
//! decided on the project). Sealed like every [`ResourceCtx`]: private
//! fields, one constructor, so the ids a proof carries and the facts it was
//! decided on come from the same loaded rows.

use forge_core::{IntegrationId, ProjectId};

use crate::model::Resource;
use crate::prove::{sealed, LoadedProject, ProjectCtx, ResourceCtx};

/// One integration (`IntegrationRead`, `IntegrationManage`): decided on the
/// instance (administrators); the id is the integration the loader found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationCtx {
    integration: IntegrationId,
}

impl IntegrationCtx {
    pub fn new(integration: IntegrationId) -> Self {
        Self { integration }
    }
}

impl sealed::Sealed for IntegrationCtx {}

impl ResourceCtx for IntegrationCtx {
    type Id = IntegrationId;
    fn resource(&self) -> Resource {
        Resource::Instance
    }
    fn id(&self) -> IntegrationId {
        self.integration
    }
}

/// A project's binding to one integration (`IntegrationBindingManage`):
/// decided on the project as loaded for the caller; the id names the
/// project and the integration of the path (whether that integration
/// exists is the command's, under its row lock — a missing one is not
/// revealed before the decision).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationBindingCtx {
    project: ProjectCtx,
    integration: IntegrationId,
}

impl IntegrationBindingCtx {
    pub fn new(project: LoadedProject, integration: IntegrationId) -> Self {
        Self {
            project: ProjectCtx::new(project),
            integration,
        }
    }
}

impl sealed::Sealed for IntegrationBindingCtx {}

impl ResourceCtx for IntegrationBindingCtx {
    type Id = (ProjectId, IntegrationId);
    fn resource(&self) -> Resource {
        Resource::Project(self.project.view())
    }
    fn id(&self) -> (ProjectId, IntegrationId) {
        (self.project.id(), self.integration)
    }
}
