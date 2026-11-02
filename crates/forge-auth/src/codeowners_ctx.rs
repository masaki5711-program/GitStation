//! The context of `CodeownersEnforcementToggle` (M3 R6, ADR-0024 §9): the
//! project as loaded, the highest minimum push role of its protection
//! rules (read with the project) and the change asked for. Sealed like
//! every [`ResourceCtx`]: one constructor, so the proof's id and the facts
//! it was decided on come from one load.

use forge_core::ProjectId;

use crate::model::{CodeownersToggle, ProjectView, Resource, Role};
use crate::prove::{sealed, LoadedProject, ResourceCtx};

/// A CODEOWNERS enforcement change of one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeownersToggleCtx {
    project: LoadedProject,
    highest_protection: Option<Role>,
    change: CodeownersToggle,
}

impl CodeownersToggleCtx {
    pub fn new(
        project: LoadedProject,
        highest_protection: Option<Role>,
        change: CodeownersToggle,
    ) -> Self {
        Self {
            project,
            highest_protection,
            change,
        }
    }

    /// The change the proof was decided for.
    pub fn change(&self) -> CodeownersToggle {
        self.change
    }
}

impl sealed::Sealed for CodeownersToggleCtx {}

impl ResourceCtx for CodeownersToggleCtx {
    type Id = ProjectId;
    fn resource(&self) -> Resource {
        Resource::CodeownersEnforcement {
            project: ProjectView {
                visibility: self.project.visibility,
                role: self.project.role,
                pending_deletion: self.project.pending_deletion,
            },
            highest_protection: self.highest_protection,
            change: self.change,
        }
    }
    fn id(&self) -> ProjectId {
        self.project.id
    }
}
