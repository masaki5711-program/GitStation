//! The merge request fixtures of the permission matrix (M3 R1a, ADR-0024
//! §12). Kept apart from `matrix.rs` so each review MR adds its shapes
//! here; the rows themselves are in `matrix_rows.rs`.

use forge_core::MergeRequestState;
#[cfg(test)]
use forge_core::RefName;

#[cfg(test)]
use crate::matrix::{ME, OTHER};
#[cfg(test)]
use crate::model::{MergeRequestEdit, ProjectView, RefProtection, Resource, Role, Visibility};

/// The project a merge request fixture lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MrProject {
    Private,
    Internal,
    /// A private project pending deletion.
    Deleted,
}

/// The branches of a merge request fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MrBranches {
    /// `refs/heads/feature` → `refs/heads/main`.
    FeatureToMain,
    /// `refs/heads/main` → `refs/heads/main`.
    Same,
    /// `refs/heads/feature` → `refs/tags/v1.0`.
    TagTarget,
    /// `refs/merge-requests/1/head` → `refs/heads/main`.
    ReservedSource,
}

/// The target an update asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MrTarget {
    /// No change.
    Unchanged,
    /// `refs/heads/release` (a valid new target).
    Release,
    /// The source branch itself.
    Source,
    /// A tag.
    Tag,
}

/// One merge request as a row sees it; the caller's role on the project
/// comes from the actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MrFixture {
    pub project: MrProject,
    /// The caller is the author.
    pub own: bool,
    pub state: MergeRequestState,
    pub draft: bool,
    pub branches: MrBranches,
    /// The draft flag an update asks for (`None` = unchanged).
    pub set_draft: Option<bool>,
    pub target: MrTarget,
    /// The target branch's minimum push role (`None` = unprotected; read
    /// by `MergeRequestMerge` only, M3 R4).
    pub protected: Option<crate::model::Role>,
    /// M3 R6: the target's protection forbids direct pushes (never read by
    /// `MergeRequestMerge`: merging is how such a branch changes).
    pub no_direct_push: bool,
}

impl MrFixture {
    /// Another user's open, ready merge request feature → main on a
    /// private project.
    pub(crate) const fn other() -> Self {
        Self {
            project: MrProject::Private,
            own: false,
            state: MergeRequestState::Open,
            draft: false,
            branches: MrBranches::FeatureToMain,
            set_draft: None,
            target: MrTarget::Unchanged,
            protected: None,
            no_direct_push: false,
        }
    }

    /// The caller's own open merge request (also the one a create opens).
    pub(crate) const fn own() -> Self {
        Self {
            own: true,
            ..Self::other()
        }
    }

    #[cfg(test)]
    pub(crate) fn resource(self, role: Option<Role>) -> Resource {
        let name = |raw: &str| RefName::parse(raw).expect("fixture ref names are valid");
        let (source, target) = match self.branches {
            MrBranches::FeatureToMain => ("refs/heads/feature", "refs/heads/main"),
            MrBranches::Same => ("refs/heads/main", "refs/heads/main"),
            MrBranches::TagTarget => ("refs/heads/feature", "refs/tags/v1.0"),
            MrBranches::ReservedSource => ("refs/merge-requests/1/head", "refs/heads/main"),
        };
        let new_target = match self.target {
            MrTarget::Unchanged => None,
            MrTarget::Release => Some(name("refs/heads/release")),
            MrTarget::Source => Some(name(source)),
            MrTarget::Tag => Some(name("refs/tags/v1.0")),
        };
        let (visibility, pending_deletion) = match self.project {
            MrProject::Private => (Visibility::Private, false),
            MrProject::Internal => (Visibility::Internal, false),
            MrProject::Deleted => (Visibility::Private, true),
        };
        Resource::MergeRequest {
            project: ProjectView {
                visibility,
                role,
                pending_deletion,
            },
            author: if self.own { ME } else { OTHER },
            state: self.state,
            draft: self.draft,
            source_branch: name(source),
            target_branch: name(target),
            edit: MergeRequestEdit {
                target_branch: new_target,
                draft: self.set_draft,
            },
            target_protection: RefProtection::effective(self.protected).map(|p| {
                if self.no_direct_push {
                    p.forbidding_direct_push()
                } else {
                    p
                }
            }),
        }
    }
}

/// One discussion of a merge request as a row sees it (M3 R3, ADR-0024
/// §12): the project, whether the caller authored the merge request and
/// whether the caller started the thread; the role comes from the actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiscussionFixture {
    pub project: MrProject,
    /// The caller is the merge request's author.
    pub own_mr: bool,
    /// The caller started the thread.
    pub own_thread: bool,
}

impl DiscussionFixture {
    #[cfg(test)]
    pub(crate) fn resource(self, role: Option<Role>) -> Resource {
        let (visibility, pending_deletion) = match self.project {
            MrProject::Private => (Visibility::Private, false),
            MrProject::Internal => (Visibility::Internal, false),
            MrProject::Deleted => (Visibility::Private, true),
        };
        Resource::Discussion {
            project: ProjectView {
                visibility,
                role,
                pending_deletion,
            },
            mr_author: if self.own_mr { ME } else { OTHER },
            started_by: if self.own_thread { ME } else { OTHER },
        }
    }
}
