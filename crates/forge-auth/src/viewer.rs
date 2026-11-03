//! What the caller could do next (U2, ADR-0024 §12): the merge request
//! screen and the merge settings screen show an action's control only when
//! that action would be allowed now. Each flag is a **trial call of
//! [`authorize`]** on the context the action's own route builds — the same
//! loaded project and merge request, the protection its loader reads, the
//! change it asks for — so there is no rule here, no new `Action`, and a
//! flag can never disagree with the route (`viewer_tests.rs` holds every
//! matrix row of these actions against the flags).
//!
//! A flag is a hint for the screen, never a permission: the route decides
//! again on what it loads, and the command again under its lock.

use crate::authorize::authorize;
use crate::model::{CodeownersToggle, MergeRequestEdit, Principal, RefProtection, Role};
use crate::prove::{LoadedProject, ProjectCtx, ResourceCtx};
use crate::{Action, CodeownersToggleCtx, MergeRequestCtx, MergeSettingsCtx};

/// The suspension the trial of `can_suspend_codeowners` asks for: a length
/// `authorize()` accepts (1..=1440 minutes). Who may suspend does not
/// depend on the length; the length is the request's to choose.
pub const TRIAL_SUSPENSION_MINUTES: u32 = 60;

/// The merge request actions of the caller (`GET …/{iid}/status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeRequestViewer {
    /// `POST …/approve` (`MergeRequestApprove`).
    pub can_approve: bool,
    /// `POST …/unapprove` (`MergeRequestUnapprove`).
    pub can_unapprove: bool,
    /// `POST …/merge` (`MergeRequestMerge`, with the target's protection).
    pub can_merge: bool,
    /// `PATCH …/{iid}` with `draft: false` (`MergeRequestUpdate`).
    pub can_mark_ready: bool,
}

/// The merge settings actions of the caller (`GET …/merge-settings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeSettingsViewer {
    /// `PUT …/merge-settings` that does not loosen the approvals.
    pub can_manage: bool,
    /// `PUT …/merge-settings` that loosens them (committers' approvals
    /// counted, approvals carried on push): a role at or above every
    /// protection of the project.
    pub can_loosen: bool,
    /// `PUT …/codeowners-enforcement` suspending CODEOWNERS.
    pub can_suspend_codeowners: bool,
}

/// One trial decision. A denial is the answer, not a failure: nothing is
/// hidden by dropping it (the route returns it with its code).
fn allowed<R: ResourceCtx>(principal: &Principal, action: Action, ctx: &R) -> bool {
    authorize(principal, action, &ctx.resource()).is_ok()
}

impl MergeRequestCtx {
    /// The caller's merge request actions, decided on this merge request
    /// as loaded and `target_protection` (what the merge route's loader
    /// reads for the target branch).
    pub fn viewer(
        &self,
        principal: &Principal,
        target_protection: Option<RefProtection>,
    ) -> MergeRequestViewer {
        let project = self.loaded_project();
        let mr = self.merge_request();
        let stored = MergeRequestCtx::new(project, mr.clone()); // clone: each trial owns its context
        let merging = MergeRequestCtx::merging(project, mr.clone(), target_protection); // clone: as above
        let ready = MergeRequestCtx::edited(
            project,
            mr.clone(), // clone: as above
            MergeRequestEdit {
                target_branch: None,
                draft: Some(false),
            },
        );
        MergeRequestViewer {
            can_approve: allowed(principal, Action::MergeRequestApprove, &stored),
            can_unapprove: allowed(principal, Action::MergeRequestUnapprove, &stored),
            can_merge: allowed(principal, Action::MergeRequestMerge, &merging),
            can_mark_ready: allowed(principal, Action::MergeRequestUpdate, &ready),
        }
    }
}

fn settings_viewer(
    principal: &Principal,
    project: LoadedProject,
    highest: Option<Role>,
) -> MergeSettingsViewer {
    let change = |loosens| MergeSettingsCtx::new(project, highest, loosens);
    let suspend = CodeownersToggleCtx::new(
        project,
        highest,
        CodeownersToggle::Suspend {
            minutes: Some(TRIAL_SUSPENSION_MINUTES),
        },
    );
    MergeSettingsViewer {
        can_manage: allowed(principal, Action::MergeSettingsManage, &change(false)),
        can_loosen: allowed(principal, Action::MergeSettingsManage, &change(true)),
        can_suspend_codeowners: allowed(principal, Action::CodeownersEnforcementToggle, &suspend),
    }
}

impl ProjectCtx {
    /// The caller's merge settings actions on this project, with
    /// `highest_protection` as the settings routes' loaders read it.
    pub fn merge_settings_viewer(
        &self,
        principal: &Principal,
        highest_protection: Option<Role>,
    ) -> MergeSettingsViewer {
        settings_viewer(principal, self.loaded(), highest_protection)
    }
}

impl MergeSettingsCtx {
    /// The caller's merge settings actions, on the facts this context was
    /// loaded with (the answer of a settings write).
    pub fn viewer(&self, principal: &Principal) -> MergeSettingsViewer {
        let (project, highest) = self.loaded();
        settings_viewer(principal, project, highest)
    }
}

impl CodeownersToggleCtx {
    /// The caller's merge settings actions, on the facts this context was
    /// loaded with (the answer of a CODEOWNERS switch).
    pub fn viewer(&self, principal: &Principal) -> MergeSettingsViewer {
        let (project, highest) = self.loaded();
        settings_viewer(principal, project, highest)
    }
}
