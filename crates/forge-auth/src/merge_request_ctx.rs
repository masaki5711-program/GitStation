//! The context of the merge request routes (M3 R1a, ADR-0024 §12): one
//! merge request of one project, as loaded for the caller — a stored one
//! (its `iid`) or the one about to be opened (no `iid` yet) — and what an
//! update asks to change. Sealed like every [`ResourceCtx`]: private
//! fields, built from one loaded project and one loaded merge request, so
//! the id a proof carries and the facts it was decided on cannot disagree.
//!
//! ```compile_fail
//! use forge_auth::{LoadedMergeRequest, LoadedProject, MergeRequestCtx};
//! fn mixed(project: LoadedProject, mr: LoadedMergeRequest) -> MergeRequestCtx {
//!     MergeRequestCtx { project, mr, edit: Default::default() }
//! }
//! ```

use forge_core::{MergeRequestState, ProjectId, RefName, UserId};

use crate::model::{MergeRequestEdit, ProjectView, Resource};
use crate::prove::{sealed, LoadedProject, ResourceCtx};

/// One merge request as loaded (one row): `iid` is `None` for the one a
/// create is about to open (then `author` is the caller and `state` open).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedMergeRequest {
    pub iid: Option<i64>,
    pub author: UserId,
    pub state: MergeRequestState,
    pub draft: bool,
    pub source_branch: RefName,
    pub target_branch: RefName,
}

/// A merge request of a project (`MergeRequestCreate`, `…Read`, `…Update`,
/// `…Close`, `…Reopen`). The id is the project and the `iid` together, so
/// a proof for one merge request cannot act on another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRequestCtx {
    project: LoadedProject,
    mr: LoadedMergeRequest,
    edit: MergeRequestEdit,
}

impl MergeRequestCtx {
    /// A merge request read, closed, reopened or about to be opened.
    pub fn new(project: LoadedProject, mr: LoadedMergeRequest) -> Self {
        Self {
            project,
            mr,
            edit: MergeRequestEdit::default(),
        }
    }

    /// A merge request an update changes (`MergeRequestUpdate`).
    pub fn edited(project: LoadedProject, mr: LoadedMergeRequest, edit: MergeRequestEdit) -> Self {
        Self { project, mr, edit }
    }

    /// The merge request as decided on.
    pub fn merge_request(&self) -> &LoadedMergeRequest {
        &self.mr
    }

    /// What an update asked to change (empty otherwise).
    pub fn edit(&self) -> &MergeRequestEdit {
        &self.edit
    }
}

impl sealed::Sealed for MergeRequestCtx {}

impl ResourceCtx for MergeRequestCtx {
    type Id = (ProjectId, Option<i64>);
    fn resource(&self) -> Resource {
        Resource::MergeRequest {
            project: ProjectView {
                visibility: self.project.visibility,
                role: self.project.role,
                pending_deletion: self.project.pending_deletion,
            },
            author: self.mr.author,
            state: self.mr.state,
            draft: self.mr.draft,
            source_branch: self.mr.source_branch.clone(), // clone: the resource owns its names
            target_branch: self.mr.target_branch.clone(),
            edit: self.edit.clone(),
        }
    }
    fn id(&self) -> (ProjectId, Option<i64>) {
        (self.project.id, self.mr.iid)
    }
}
