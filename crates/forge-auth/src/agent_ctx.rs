//! The context of the marketplace's publishing routes (plan 16 A5,
//! ADR-0032 Amendment A5): one part version as the route loaded it — what
//! it is (slug, version, the parts project, the pinned commit and tag
//! object) and the facts `authorize()` decides on ([`AgentPartView`]).
//! Sealed like every [`ResourceCtx`]: one constructor, so the target the
//! proof carries and the facts it was decided on come from one load.

use forge_core::ProjectId;

use crate::model::{AgentPartView, AgentResource, Resource};
use crate::prove::{sealed, ResourceCtx};

/// What a publishing route acts on: one version of one part, pinned to the
/// commit (and tag object) the route loaded. Publishing freezes exactly
/// this commit, whatever the tag names afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPartTarget {
    pub slug: String,
    pub version: String,
    /// The parts project the tag is in (the part's `source_project_id`).
    pub source_project: ProjectId,
    /// The commit the tag peeled to when the route loaded it.
    pub source_sha: String,
    /// The object the tag named (an annotated tag: the tag object).
    pub tag_object: String,
}

/// A part version for `AgentPartPublish`, `AgentPartPromote` or
/// `AgentPartRevoke`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPartCtx {
    target: AgentPartTarget,
    view: AgentPartView,
}

impl AgentPartCtx {
    pub fn new(target: AgentPartTarget, view: AgentPartView) -> Self {
        Self { target, view }
    }

    /// The facts `authorize()` decided on.
    pub fn view(&self) -> &AgentPartView {
        &self.view
    }

    /// The target (also the proof's id).
    pub fn target(&self) -> &AgentPartTarget {
        &self.target
    }
}

impl sealed::Sealed for AgentPartCtx {}

impl ResourceCtx for AgentPartCtx {
    type Id = AgentPartTarget;
    fn resource(&self) -> Resource {
        // clone: authorize() decides on its own copy of the facts
        Resource::Agent(AgentResource::Part(self.view.clone()))
    }
    fn id(&self) -> AgentPartTarget {
        self.target.clone() // clone: the proof carries its own target
    }
}

#[cfg(test)]
mod tests {
    use forge_core::{GroupSight, UserId};

    use super::*;
    use crate::model::Role;

    /// The context hands authorize() exactly the loaded facts and the
    /// proof exactly the loaded target.
    #[test]
    fn the_context_carries_the_loaded_facts() {
        let view = AgentPartView {
            group_role: Some(Role::Maintainer),
            group_sight: GroupSight::ROLE_ONLY,
            tag_pusher: Some(UserId::new(2)),
            tag_authors: vec![UserId::new(3)],
            network: true,
        };
        let target = AgentPartTarget {
            slug: "guard-hook".to_owned(),
            version: "1.0.0".to_owned(),
            source_project: ProjectId::new(7),
            source_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            tag_object: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        };
        let ctx = AgentPartCtx::new(target.clone(), view.clone());
        assert_eq!(
            ctx.resource(),
            Resource::Agent(AgentResource::Part(view.clone()))
        );
        assert_eq!(ctx.id(), target);
        assert_eq!(ctx.view(), &view);
    }
}
