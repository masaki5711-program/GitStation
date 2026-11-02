//! Deciding every ref of a push (M1 phase 9 MR4, ADR-0008 option B):
//! [`authorize_ref_updates`] calls [`authorize`] once per ref, so the
//! single decision point stays the only place a push rule lives; it holds
//! no rule of its own.
//!
//! It takes the proof of the project-level `RepoPush` decision
//! (`Authorized<ProjectCtx>`, made by the dispatcher before the request
//! body was read) and nothing else about the caller: the principal is the
//! one the proof was decided for, and the per-ref resources are built from
//! the same loaded project view, so a handler cannot decide refs for
//! another caller, against another project, or skip the project check.
//! Each [`RefUpdateCtx`] derives its protection from the push's one rule
//! snapshot (`ProtectionRules`, loaded once per push) through
//! `RefProtection::effective` (strictest rule wins), and the snapshot must
//! have been loaded for the proof's project.

use forge_core::{ProjectId, ProtectionRules, RefName};
use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::authorize::authorize;
use crate::model::{RefProtection, RefUpdate, Resource};
use crate::prove::{Authorized, ProjectCtx, ResourceCtx};

/// One ref update of a push, as `authorize()` decides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdateCtx {
    name: RefName,
    update: RefUpdate,
    protection: Option<RefProtection>,
    /// The project of the rules the protection came from.
    rules_project: Option<ProjectId>,
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
            rules_project: rules.project(),
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

/// `authorize(<the proof's principal>, RepoPush, Ref { … })` for every
/// update, in order (one result per update). `FG-SYS-0001` (a programming
/// error, never a pass) when `project` is not a `RepoPush` proof
/// (`reason=proof_action_mismatch`) or an update's rules were not loaded
/// for the proof's project (`reason=rules_project_mismatch`).
pub fn authorize_ref_updates(
    project: &Authorized<ProjectCtx>,
    updates: &[RefUpdateCtx],
) -> Result<Vec<Result<(), ForgeError>>, ForgeError> {
    if project.action() != Action::RepoPush {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_action_mismatch")
            .with_context("route_action", Action::RepoPush.name())
            .with_context("proof_action", project.action().name()));
    }
    let id = project.ctx().id();
    if let Some(other) = updates.iter().find(|u| u.rules_project != Some(id)) {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "rules_project_mismatch")
            .with_context("project_id", id.get())
            .with_context(
                "rules_project_id",
                other.rules_project.map_or(0, ProjectId::get),
            ));
    }
    let view = project.ctx().view();
    let principal = project.principal();
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
