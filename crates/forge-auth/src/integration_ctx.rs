//! The contexts of the integration routes (M3 I1a, plan 11 §5 I1): one
//! integration of the administration ([`IntegrationCtx`], decided on the
//! instance) and one binding of a project ([`IntegrationBindingCtx`],
//! decided on the project). Sealed like every [`ResourceCtx`]: private
//! fields, one constructor, so the ids a proof carries and the facts it was
//! decided on come from the same loaded rows.

use forge_core::{IntegrationId, ProjectId};

use crate::model::{ActAsView, BindingSubject, Resource, ViaView};
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

/// A project's binding to one integration (`IntegrationBindingManage`,
/// `IntegrationBindingRemove`): decided on the project as loaded for the
/// caller; the id names the project and the integration of the path
/// (whether that integration exists is the command's, under its row lock —
/// a missing one is not revealed before the decision). The integration
/// routes of M3 X3 also name one binding ([`IntegrationBindingCtx::binding`])
/// and, for an act-as request, what its assertion is compared with
/// ([`IntegrationBindingCtx::acting`]: the resource is then
/// `Resource::ActAs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationBindingCtx {
    project: ProjectCtx,
    integration: IntegrationId,
    binding: Option<i64>,
    acting: Option<BindingSubject>,
}

impl IntegrationBindingCtx {
    pub fn new(project: LoadedProject, integration: IntegrationId) -> Self {
        Self {
            project: ProjectCtx::new(project),
            integration,
            binding: None,
            acting: None,
        }
    }

    /// The same, naming binding `id` of the integration (loaded with it).
    #[must_use]
    pub fn with_binding(mut self, id: i64) -> Self {
        self.binding = Some(id);
        self
    }

    /// The same as an act-as request reaches it: the binding the request
    /// writes, for the assertion (M3 X3, ADR-0025 Amendment X3).
    #[must_use]
    pub fn acting(mut self, subject: BindingSubject) -> Self {
        self.acting = Some(subject);
        self
    }

    /// The binding the route names (`None` on the project routes).
    pub fn binding(&self) -> Option<i64> {
        self.binding
    }

    /// The binding an act-as request writes (`None` for any other).
    pub fn acting_subject(&self) -> Option<&BindingSubject> {
        self.acting.as_ref()
    }
}

impl sealed::Sealed for IntegrationBindingCtx {}

impl ResourceCtx for IntegrationBindingCtx {
    type Id = (ProjectId, IntegrationId);
    fn resource(&self) -> Resource {
        let inner = Resource::Project(self.project.view());
        match &self.acting {
            None => inner,
            // The binding intents decide on the person's own role and the
            // assertion (ADR-0025 Amendment X3), never on the service
            // account's membership: no via facts are loaded.
            Some(subject) => Resource::ActAs(Box::new(ActAsView {
                inner,
                via: ViaView {
                    role: None,
                    bound: false,
                },
                subject: None,
                binding: Some(subject.clone()), // clone: the resource owns its facts
            })),
        }
    }
    fn id(&self) -> (ProjectId, IntegrationId) {
        (self.project.id(), self.integration)
    }
}
