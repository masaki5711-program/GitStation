//! The context of the act-as confirmation (M3 S2, ADR-0025 §5):
//! `ActAsConfirmView` and `ActAsAssertionIssue` decide on one merge request
//! as loaded for the person, the intent they confirm, and the receiving
//! integration as loaded. Sealed like every [`ResourceCtx`].

use forge_core::act_as::Intent;
use forge_core::{IntegrationId, ProjectId};

use crate::merge_request_ctx::{LoadedMergeRequest, MergeRequestCtx};
use crate::model::{ActAsConfirmView, Resource};
use crate::prove::{sealed, LoadedProject, ResourceCtx};

/// The integration a confirmation names, as loaded (one row) with whether
/// it is bound to the merge request's project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedConfirmIntegration {
    pub integration: IntegrationId,
    pub enabled: bool,
    pub bound: bool,
}

/// See the module documentation. The id is the project, the merge
/// request's `iid` and the integration together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActAsConfirmCtx {
    mr: MergeRequestCtx,
    intent: Intent,
    integration: LoadedConfirmIntegration,
}

impl ActAsConfirmCtx {
    pub fn new(
        project: LoadedProject,
        mr: LoadedMergeRequest,
        intent: Intent,
        integration: LoadedConfirmIntegration,
    ) -> Self {
        Self {
            mr: MergeRequestCtx::new(project, mr),
            intent,
            integration,
        }
    }

    pub fn intent(&self) -> Intent {
        self.intent
    }

    /// The merge request as decided on.
    pub fn merge_request(&self) -> &LoadedMergeRequest {
        self.mr.merge_request()
    }
}

impl sealed::Sealed for ActAsConfirmCtx {}

/// The confirmation of a binding intent (M3 X3, ADR-0025 Amendment X3):
/// `ActAsAssertionIssue` decided on the project as loaded for the person,
/// the intent (`binding_set` / `binding_remove`), the kanflow project and
/// the receiving integration as loaded (`bound`: the integration is bound
/// to the project for that kanflow project).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActAsBindingConfirmCtx {
    project: crate::prove::ProjectCtx,
    intent: Intent,
    integration: LoadedConfirmIntegration,
}

impl ActAsBindingConfirmCtx {
    pub fn new(
        project: LoadedProject,
        intent: Intent,
        integration: LoadedConfirmIntegration,
    ) -> Self {
        Self {
            project: crate::prove::ProjectCtx::new(project),
            intent,
            integration,
        }
    }

    pub fn intent(&self) -> Intent {
        self.intent
    }
}

impl sealed::Sealed for ActAsBindingConfirmCtx {}

impl ResourceCtx for ActAsBindingConfirmCtx {
    type Id = (ProjectId, IntegrationId);
    fn resource(&self) -> Resource {
        Resource::ActAsConfirm(Box::new(ActAsConfirmView {
            target: Resource::Project(self.project.view()),
            intent: self.intent,
            integration_enabled: self.integration.enabled,
            bound: self.integration.bound,
        }))
    }
    fn id(&self) -> (ProjectId, IntegrationId) {
        (self.project.id(), self.integration.integration)
    }
}

impl ResourceCtx for ActAsConfirmCtx {
    type Id = (ProjectId, Option<i64>, IntegrationId);
    fn resource(&self) -> Resource {
        Resource::ActAsConfirm(Box::new(ActAsConfirmView {
            target: self.mr.person_resource(),
            intent: self.intent,
            integration_enabled: self.integration.enabled,
            bound: self.integration.bound,
        }))
    }
    fn id(&self) -> (ProjectId, Option<i64>, IntegrationId) {
        let (project, iid) = self.mr.id();
        (project, iid, self.integration.integration)
    }
}
