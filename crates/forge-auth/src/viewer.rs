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

impl MergeRequestViewer {
    /// Every flag `false`: what a screen shows when the facts behind the
    /// flags could not be read (U2 review: a hint fails closed).
    pub const CLOSED: Self = Self {
        can_approve: false,
        can_unapprove: false,
        can_merge: false,
        can_mark_ready: false,
    };
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

impl MergeSettingsViewer {
    /// Every flag `false` (as [`MergeRequestViewer::CLOSED`]).
    pub const CLOSED: Self = Self {
        can_manage: false,
        can_loosen: false,
        can_suspend_codeowners: false,
    };
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

/// One trial of an act-as action (M3 S2, ADR-0025 §9): whether it would
/// be allowed with a matching assertion, and the log `reason` when not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActAsTrial {
    pub allowed: bool,
    pub reason: Option<String>,
}

/// What a service account acting for a person could do on a merge request
/// (the `viewer.act_as` of `GET …/{iid}/status`): approve and unapprove,
/// each decided by `authorize()` as if the person had confirmed this head
/// and target. `requires_assertion` is always `true` for these actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActAsViewer {
    pub can_approve: ActAsTrial,
    pub can_unapprove: ActAsTrial,
    pub requires_assertion: bool,
}

impl MergeRequestCtx {
    /// The act-as trials of `principal` (`None` when it does not act for a
    /// person, or the context was loaded without the act-as facts).
    pub fn act_as_viewer(&self, principal: &Principal) -> Option<ActAsViewer> {
        let credential = principal.act_as_credential()?;
        let (via, subject) = self.act_as()?;
        let subject = subject.as_ref()?;
        let person = principal.user()?;
        let trial = |intent: forge_core::act_as::Intent, action: Action| -> ActAsTrial {
            let (Some(head), Some(tip)) = (subject.head_sha.as_ref(), subject.target_sha.as_ref())
            else {
                return ActAsTrial {
                    allowed: false,
                    reason: Some("nothing_to_confirm".to_owned()),
                };
            };
            let mut hypothetical = credential.clone(); // clone: the trial's own credential
            hypothetical.assertion = Some(crate::model::AssertionFacts {
                id: 0,
                public_id: "trial".to_owned(),
                intent,
                project: subject.project,
                merge_request: subject.merge_request,
                head_sha: head.clone(), // clone: the trial owns its facts
                target_ref: subject.target_ref.clone(),
                target_sha: tip.clone(),
            });
            let p = Principal::act_as(
                person,
                principal.active(),
                principal.account_kind(),
                hypothetical,
            );
            let ctx = MergeRequestCtx::new(self.loaded_project(), self.merge_request().clone()) // clone: each trial owns its context
                .acting(
                    *via,
                    Some(crate::model::ActAsSubject {
                        requested_sha: Some(head.clone()), // clone: as above
                        ..subject.clone()
                    }),
                );
            match authorize(&p, action, &ctx.resource()) {
                Ok(()) => ActAsTrial {
                    allowed: true,
                    reason: None,
                },
                Err(e) => ActAsTrial {
                    allowed: false,
                    reason: e
                        .context()
                        .iter()
                        .find(|(k, _)| *k == "reason")
                        .map(|(_, v)| v.clone())
                        .or_else(|| Some(e.code().id().to_owned())),
                },
            }
        };
        Some(ActAsViewer {
            can_approve: trial(
                forge_core::act_as::Intent::Approve,
                Action::MergeRequestApprove,
            ),
            can_unapprove: trial(
                forge_core::act_as::Intent::Unapprove,
                Action::MergeRequestUnapprove,
            ),
            requires_assertion: true,
        })
    }
}
