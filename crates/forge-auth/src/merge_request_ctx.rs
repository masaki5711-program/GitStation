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

use forge_core::{DiscussionId, MergeRequestState, ProjectId, RefName, UserId};

use crate::model::{
    ActAsSubject, ActAsView, MergeRequestEdit, ProjectView, RefProtection, Resource, ViaView,
};
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
    /// The target branch's protection (`MergeRequestMerge`, M3 R4);
    /// `None` for every other action and for an unprotected target.
    target_protection: Option<RefProtection>,
    /// The integration's service account on the project and the merge
    /// request as an assertion is compared with, when the request acts
    /// for a person (M3 S2, ADR-0025 §3, §5.3).
    act_as: Option<(ViaView, Option<ActAsSubject>)>,
}

impl MergeRequestCtx {
    /// A merge request read, closed, reopened or about to be opened.
    pub fn new(project: LoadedProject, mr: LoadedMergeRequest) -> Self {
        Self {
            project,
            mr,
            edit: MergeRequestEdit::default(),
            target_protection: None,
            act_as: None,
        }
    }

    /// A merge request an update changes (`MergeRequestUpdate`).
    pub fn edited(project: LoadedProject, mr: LoadedMergeRequest, edit: MergeRequestEdit) -> Self {
        Self {
            project,
            mr,
            edit,
            target_protection: None,
            act_as: None,
        }
    }

    /// The same merge request as an act-as request reaches it: `via` and
    /// `subject` loaded by the route (or, under the lock, by the command)
    /// for the request's integration.
    #[must_use]
    pub fn acting(mut self, via: ViaView, subject: Option<ActAsSubject>) -> Self {
        self.act_as = Some((via, subject));
        self
    }

    /// The act-as facts this context was loaded with (`None` otherwise).
    pub fn act_as(&self) -> Option<&(ViaView, Option<ActAsSubject>)> {
        self.act_as.as_ref()
    }

    /// A merge request about to be merged (`MergeRequestMerge`, M3 R4):
    /// `target_protection` is the protection in effect on its target branch
    /// (`RefProtection::effective` of the rules matching it, loaded with
    /// the push path's `ProtectionLoader`).
    pub fn merging(
        project: LoadedProject,
        mr: LoadedMergeRequest,
        target_protection: Option<RefProtection>,
    ) -> Self {
        Self {
            project,
            mr,
            edit: MergeRequestEdit::default(),
            target_protection,
            act_as: None,
        }
    }

    /// The target branch's protection decided on (merges only).
    pub fn target_protection(&self) -> Option<RefProtection> {
        self.target_protection
    }

    /// The merge request as decided on.
    pub fn merge_request(&self) -> &LoadedMergeRequest {
        &self.mr
    }

    /// What an update asked to change (empty otherwise).
    pub fn edit(&self) -> &MergeRequestEdit {
        &self.edit
    }

    /// The project as loaded (the viewer flags build the write routes'
    /// contexts from it, `viewer.rs`).
    pub(crate) fn loaded_project(&self) -> LoadedProject {
        self.project
    }
}

impl sealed::Sealed for MergeRequestCtx {}

impl ResourceCtx for MergeRequestCtx {
    type Id = (ProjectId, Option<i64>);
    fn resource(&self) -> Resource {
        let inner = self.plain_resource();
        match &self.act_as {
            None => inner,
            Some((via, subject)) => Resource::ActAs(Box::new(ActAsView {
                inner,
                via: *via,
                subject: subject.clone(), // clone: the resource owns its facts
            })),
        }
    }
    fn id(&self) -> (ProjectId, Option<i64>) {
        (self.project.id, self.mr.iid)
    }
}

impl MergeRequestCtx {
    /// The merge request as the person sees it (no act-as wrapper).
    fn plain_resource(&self) -> Resource {
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
            target_protection: self.target_protection,
        }
    }

    /// The merge request as the person sees it, for the confirmation
    /// (`ActAsConfirmCtx`).
    pub(crate) fn person_resource(&self) -> Resource {
        self.plain_resource()
    }
}

/// One discussion as loaded (its row): who started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedDiscussion {
    pub id: DiscussionId,
    pub started_by: UserId,
}

/// A discussion of a stored merge request (`DiscussionResolve`, M3 R3).
/// The id is the project, the merge request's `iid` and the discussion
/// together, so a proof for one thread cannot resolve another. Sealed like
/// every [`ResourceCtx`]: built from one loaded project, merge request and
/// discussion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionCtx {
    project: LoadedProject,
    mr: LoadedMergeRequest,
    discussion: LoadedDiscussion,
}

impl DiscussionCtx {
    pub fn new(
        project: LoadedProject,
        mr: LoadedMergeRequest,
        discussion: LoadedDiscussion,
    ) -> Self {
        Self {
            project,
            mr,
            discussion,
        }
    }

    /// The merge request as decided on.
    pub fn merge_request(&self) -> &LoadedMergeRequest {
        &self.mr
    }

    /// The discussion as decided on.
    pub fn discussion(&self) -> LoadedDiscussion {
        self.discussion
    }
}

impl sealed::Sealed for DiscussionCtx {}

impl ResourceCtx for DiscussionCtx {
    type Id = (ProjectId, Option<i64>, DiscussionId);
    fn resource(&self) -> Resource {
        Resource::Discussion {
            project: ProjectView {
                visibility: self.project.visibility,
                role: self.project.role,
                pending_deletion: self.project.pending_deletion,
            },
            mr_author: self.mr.author,
            started_by: self.discussion.started_by,
        }
    }
    fn id(&self) -> (ProjectId, Option<i64>, DiscussionId) {
        (self.project.id, self.mr.iid, self.discussion.id)
    }
}
