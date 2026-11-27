//! The AI asset draft writer's arm of `authorize()` (plan 17 P17-10,
//! ADR-0039 Amendment P17-10). `authorize()` calls [`decide`] first for
//! every `Credential::AssetDraft` (as it does the autofix arm), so no
//! other rule — the public actions, the roles, the people-only actions —
//! is ever reached by it.
//!
//! Closed by default (an allow list, [`asset_draft_may`]): the writer may
//! create (`RepoPush`) the draft branch of one draft improvement merge
//! request — on `Resource::AssetDraft` only, and only when
//!
//! - the project is not being deleted and its `ai_assets.enabled` is on,
//! - the merge request is open and still a draft,
//! - its source branch is `refs/heads/improve/<id>` and the written ref is
//!   exactly `refs/heads/ai-draft/<id>` (`improve_draft::ai_draft_branch_for`),
//! - the write creates the branch (never a fast-forward, force or delete:
//!   a draft is proposed once, a person applies it),
//! - the branch is not protected.
//!
//! Every other action — marking a merge request ready, approving, merging,
//! commenting, any read or setting — is
//! `FG-AUTH-0003 reason=asset_draft_not_allowed`.

use forge_core::improve_draft::{ai_draft_branch_for, is_improve_branch};
use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::model::{AssetDraftTarget, RefUpdate, Resource};

/// Whether the AI asset draft writer may perform `action` at all.
pub fn asset_draft_may(action: Action) -> bool {
    matches!(action, Action::RepoPush)
}

/// Decide `action` on `resource` for the AI asset draft writer.
pub(crate) fn decide(action: Action, resource: &Resource) -> Result<(), ForgeError> {
    let deny = |code: ErrorCode, reason: &'static str| {
        ForgeError::new(code)
            .with_context("action", action.name())
            .with_context("credential", "asset_draft")
            .with_context("reason", reason)
    };
    if !asset_draft_may(action) {
        return Err(deny(ErrorCode::Forbidden, "asset_draft_not_allowed"));
    }
    let Resource::AssetDraft(target) = resource else {
        return Err(deny(ErrorCode::Forbidden, "asset_draft_requires_target"));
    };
    decide_target(target).map_err(|(code, reason)| deny(code, reason))
}

fn decide_target(t: &AssetDraftTarget) -> Result<(), (ErrorCode, &'static str)> {
    if t.project.pending_deletion {
        return Err((ErrorCode::PendingDeletion, "pending_deletion"));
    }
    if !t.ai_assets_enabled {
        return Err((ErrorCode::Forbidden, "asset_draft_ai_assets_disabled"));
    }
    if !is_improve_branch(&t.source_branch)
        || ai_draft_branch_for(&t.source_branch).as_ref() != Some(&t.branch)
    {
        return Err((ErrorCode::Forbidden, "asset_draft_branch_only"));
    }
    match t.update {
        RefUpdate::Create => {}
        RefUpdate::FastForward | RefUpdate::Force | RefUpdate::Delete => {
            return Err((ErrorCode::Forbidden, "asset_draft_create_only"))
        }
    }
    if t.protection.is_some() {
        return Err((ErrorCode::Forbidden, "asset_draft_protected_branch"));
    }
    if !t.mr_open {
        return Err((ErrorCode::Forbidden, "asset_draft_merge_request_not_open"));
    }
    if !t.mr_draft {
        return Err((ErrorCode::Forbidden, "asset_draft_not_draft"));
    }
    Ok(())
}

/// A draft improvement merge request's draft branch as the AI asset draft
/// writer reaches it (the context of `prove()`). The id is the project and
/// the merge request's `iid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetDraftCtx {
    project: forge_core::ProjectId,
    iid: i64,
    target: AssetDraftTarget,
}

impl AssetDraftCtx {
    /// The context of merge request `iid` of `project` with `target` read
    /// from the database right before the write.
    pub fn new(project: forge_core::ProjectId, iid: i64, target: AssetDraftTarget) -> Self {
        Self {
            project,
            iid,
            target,
        }
    }

    pub fn target(&self) -> &AssetDraftTarget {
        &self.target
    }
}

impl crate::prove::sealed::Sealed for AssetDraftCtx {}

impl crate::prove::ResourceCtx for AssetDraftCtx {
    type Id = (forge_core::ProjectId, i64);
    fn resource(&self) -> Resource {
        Resource::AssetDraft(self.target.clone()) // clone: the resource owns its facts
    }
    fn id(&self) -> (forge_core::ProjectId, i64) {
        (self.project, self.iid)
    }
}

#[cfg(test)]
#[path = "asset_draft_tests.rs"]
mod tests;
