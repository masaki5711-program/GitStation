//! Deciding every ref of a push (M1 phase 9 MR4, ADR-0008 option B):
//! [`authorize_ref_updates`] calls [`authorize`] once per ref, so the
//! single decision point stays the only place a push rule lives; it holds
//! no rule of its own.
//!
//! It takes the proof of the project-level `RepoPush` decision
//! (`Authorized<ProjectCtx>`, made by the dispatcher before the request
//! body was read): the per-ref resources are built from the same loaded
//! project view, so a handler cannot decide refs against another project
//! or skip the project check. Each [`RefUpdateCtx`] derives its protection
//! from the push's one rule snapshot (`ProtectionRules`, loaded once per
//! push) through `RefProtection::effective` (strictest rule wins) — a
//! caller cannot build a protection by hand.

use forge_core::{ProtectionRules, RefName};
use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::authorize::authorize;
use crate::model::{Principal, RefProtection, RefUpdate, Resource};
use crate::prove::{Authorized, ProjectCtx};

/// One ref update of a push, as `authorize()` decides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdateCtx {
    name: RefName,
    update: RefUpdate,
    protection: Option<RefProtection>,
}

impl RefUpdateCtx {
    /// `update` must come from the push itself (zero sides, the ancestry
    /// check; a tag that exists already moves as `Force`); the protection
    /// is every rule of `rules` matching `name`, strictest first.
    pub fn new(name: RefName, update: RefUpdate, rules: &ProtectionRules) -> Self {
        let protection = RefProtection::effective(rules.levels(&name));
        Self {
            name,
            update,
            protection,
        }
    }

    pub fn name(&self) -> &RefName {
        &self.name
    }

    pub fn update(&self) -> RefUpdate {
        self.update
    }

    /// The protection in effect (`None` = unprotected).
    pub fn protection(&self) -> Option<RefProtection> {
        self.protection
    }
}

/// `authorize(principal, RepoPush, Ref { … })` for every update, in order
/// (one result per update). `project` must be the proof of the push's own
/// project-level `RepoPush` decision; any other proof is `FG-SYS-0001
/// reason=proof_action_mismatch` (a programming error, never a pass).
pub fn authorize_ref_updates(
    principal: &Principal,
    project: &Authorized<ProjectCtx>,
    updates: &[RefUpdateCtx],
) -> Result<Vec<Result<(), ForgeError>>, ForgeError> {
    if project.action() != Action::RepoPush {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_action_mismatch")
            .with_context("route_action", Action::RepoPush.name())
            .with_context("proof_action", project.action().name()));
    }
    let view = project.ctx().view();
    Ok(updates
        .iter()
        .map(|u| {
            authorize(
                principal,
                Action::RepoPush,
                &Resource::Ref {
                    project: view,
                    name: u.name.clone(), // clone: the resource owns its name
                    update: u.update,
                    protection: u.protection,
                },
            )
        })
        .collect())
}
