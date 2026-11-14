//! The auto-merge arm of `authorize()` (M4 C9, plan 04 M-5, ADR-0017
//! Amendments 2026-10-30 B / E and 2026-11-13). `authorize()` calls
//! [`decide`] first for every `Credential::AutoMerge` (as it does the CI
//! arm), so no other rule — the public actions, the roles, the people-only
//! actions — is ever reached by the auto-merge principal.
//!
//! Closed by default: the principal adds a merge request to the merge
//! queue (`MergeQueueEnqueue`) and the queue's landing merges its entry
//! (`MergeRequestMerge`), both on `Resource::AutoMerge` only, and only
//! while the project's `merge.auto` acts (`conditional` / `full`) and no
//! emergency stop is on. Every other action — approving, deploying,
//! protected-branch rules, the emergency stop itself, any read — is
//! `FG-AUTH-0003 reason=auto_merge_not_allowed`. Whether the merge request
//! is ready is `evaluate_merge()`'s, never decided here.

use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::model::{AutoMergeTarget, Resource};

/// Whether the auto-merge principal may perform `action` at all.
pub fn auto_merge_may(action: Action) -> bool {
    matches!(
        action,
        Action::MergeQueueEnqueue | Action::MergeRequestMerge
    )
}

/// Decide `action` on `resource` for the auto-merge principal.
pub(crate) fn decide(action: Action, resource: &Resource) -> Result<(), ForgeError> {
    let deny = |code: ErrorCode, reason: &'static str| {
        ForgeError::new(code)
            .with_context("action", action.name())
            .with_context("credential", "auto_merge")
            .with_context("reason", reason)
    };
    if !auto_merge_may(action) {
        return Err(deny(ErrorCode::Forbidden, "auto_merge_not_allowed"));
    }
    let Resource::AutoMerge(target) = resource else {
        return Err(deny(ErrorCode::Forbidden, "auto_merge_requires_target"));
    };
    decide_target(target).map_err(|(code, reason)| {
        let e = deny(code, reason);
        match (reason, target.stop) {
            ("auto_merge_emergency_stop", Some(scope)) => e.with_context("scope", scope.as_str()),
            ("auto_merge_mode", _) => e.with_context("mode", target.mode.as_str()),
            _ => e,
        }
    })
}

fn decide_target(t: &AutoMergeTarget) -> Result<(), (ErrorCode, &'static str)> {
    if t.project.pending_deletion {
        return Err((ErrorCode::PendingDeletion, "pending_deletion"));
    }
    if t.source_branch.branch().is_none() || t.target_branch.branch().is_none() {
        return Err((ErrorCode::Forbidden, "not_a_branch"));
    }
    if t.source_branch == t.target_branch {
        return Err((ErrorCode::Forbidden, "source_equals_target"));
    }
    if t.stop.is_some() {
        return Err((ErrorCode::Forbidden, "auto_merge_emergency_stop"));
    }
    if !t.mode.acts() {
        return Err((ErrorCode::Forbidden, "auto_merge_mode"));
    }
    Ok(())
}

/// A merge request as the auto-merge principal reaches it (the context of
/// `prove()`): the loaded project and merge request with the project's
/// `merge.auto` and emergency stops read with them. The id is the project
/// and the merge request's `iid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoMergeCtx {
    project: crate::LoadedProject,
    iid: i64,
    target: AutoMergeTarget,
}

impl AutoMergeCtx {
    /// The context of merge request `iid` (`mr`) of `project`, with
    /// `mode` and `stop` as read in the same transaction.
    pub fn new(
        project: crate::LoadedProject,
        iid: i64,
        mr: &crate::LoadedMergeRequest,
        mode: forge_core::auto_merge::AutoMergeMode,
        stop: Option<forge_core::auto_merge::StopScope>,
    ) -> Self {
        let target = AutoMergeTarget {
            project: crate::model::ProjectView {
                visibility: project.visibility,
                role: None,
                pending_deletion: project.pending_deletion,
            },
            source_branch: mr.source_branch.clone(), // clone: the target owns its names
            target_branch: mr.target_branch.clone(), // clone: as above
            mode,
            stop,
        };
        Self {
            project,
            iid,
            target,
        }
    }
}

impl crate::prove::sealed::Sealed for AutoMergeCtx {}

impl crate::prove::ResourceCtx for AutoMergeCtx {
    type Id = (forge_core::ProjectId, i64);
    fn resource(&self) -> Resource {
        Resource::AutoMerge(self.target.clone()) // clone: the resource owns its facts
    }
    fn id(&self) -> (forge_core::ProjectId, i64) {
        (self.project.id, self.iid)
    }
}
