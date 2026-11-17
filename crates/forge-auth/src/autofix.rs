//! The autofix bot's arm of `authorize()` (M4 C14, ADR-0017 §2.5 and its
//! Amendment C14). `authorize()` calls [`decide`] first for every
//! `Credential::Autofix` (as it does the CI and auto-merge arms), so no
//! other rule — the public actions, the roles, the people-only actions —
//! is ever reached by the bot.
//!
//! Closed by default (an allow list, [`autofix_may`]): the bot may push
//! (`RepoPush`) one commit to the source branch of the merge request it
//! was asked to fix — on `Resource::Autofix` only, and only when
//!
//! - the project is not being deleted,
//! - the ref is a branch, other than the target, moved by a fast-forward,
//! - the branch is **not protected** (any protection refuses, whoever the
//!   author is: the bot never writes a protected branch, ADR-0017 decision 1),
//! - the merge request is open and the project's autofix is enabled,
//! - the author and the person who enabled autofix are both active and
//!   Developer or above now (bot ∩ author ∩ enabler: never more than the
//!   people behind it could do themselves).
//!
//! Every other action — approving, merging, queueing, reserving, protected
//! refs and paths, any setting, deploying, acting as someone, any read —
//! is `FG-AUTH-0003 reason=autofix_not_allowed`.

use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::model::{AutofixTarget, RefUpdate, Resource, Role};

/// Whether the autofix bot may perform `action` at all.
pub fn autofix_may(action: Action) -> bool {
    matches!(action, Action::RepoPush)
}

/// Decide `action` on `resource` for the autofix bot.
pub(crate) fn decide(action: Action, resource: &Resource) -> Result<(), ForgeError> {
    let deny = |code: ErrorCode, reason: &'static str| {
        ForgeError::new(code)
            .with_context("action", action.name())
            .with_context("credential", "autofix")
            .with_context("reason", reason)
    };
    if !autofix_may(action) {
        return Err(deny(ErrorCode::Forbidden, "autofix_not_allowed"));
    }
    let Resource::Autofix(target) = resource else {
        return Err(deny(ErrorCode::Forbidden, "autofix_requires_target"));
    };
    decide_target(target).map_err(|(code, reason)| deny(code, reason))
}

fn at_least_developer(role: Option<Role>, active: bool) -> bool {
    active && role.is_some_and(|r| r >= Role::Developer)
}

fn decide_target(t: &AutofixTarget) -> Result<(), (ErrorCode, &'static str)> {
    if t.project.pending_deletion {
        return Err((ErrorCode::PendingDeletion, "pending_deletion"));
    }
    if t.source_branch.branch().is_none() || t.target_branch.branch().is_none() {
        return Err((ErrorCode::Forbidden, "not_a_branch"));
    }
    if t.source_branch == t.target_branch {
        return Err((ErrorCode::Forbidden, "source_equals_target"));
    }
    match t.update {
        RefUpdate::FastForward => {}
        RefUpdate::Create | RefUpdate::Force | RefUpdate::Delete => {
            return Err((ErrorCode::Forbidden, "autofix_not_fast_forward"))
        }
    }
    if t.protection.is_some() {
        return Err((ErrorCode::Forbidden, "autofix_protected_branch"));
    }
    if !t.mr_open {
        return Err((ErrorCode::Forbidden, "autofix_merge_request_not_open"));
    }
    if !t.autofix_enabled {
        return Err((ErrorCode::Forbidden, "autofix_disabled"));
    }
    if !at_least_developer(t.author_role, t.author_active) {
        return Err((ErrorCode::Forbidden, "autofix_author_role"));
    }
    if !at_least_developer(t.enabler_role, t.enabler_active) {
        return Err((ErrorCode::Forbidden, "autofix_enabler_role"));
    }
    Ok(())
}

/// A merge request's source branch as the autofix bot reaches it (the
/// context of `prove()`). The id is the project and the merge request's
/// `iid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutofixCtx {
    project: forge_core::ProjectId,
    iid: i64,
    target: AutofixTarget,
}

impl AutofixCtx {
    /// The context of merge request `iid` of `project` with `target` read
    /// from the database in one transaction.
    pub fn new(project: forge_core::ProjectId, iid: i64, target: AutofixTarget) -> Self {
        Self {
            project,
            iid,
            target,
        }
    }

    pub fn target(&self) -> &AutofixTarget {
        &self.target
    }
}

impl crate::prove::sealed::Sealed for AutofixCtx {}

impl crate::prove::ResourceCtx for AutofixCtx {
    type Id = (forge_core::ProjectId, i64);
    fn resource(&self) -> Resource {
        Resource::Autofix(self.target.clone()) // clone: the resource owns its facts
    }
    fn id(&self) -> (forge_core::ProjectId, i64) {
        (self.project, self.iid)
    }
}
