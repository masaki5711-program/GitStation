//! `prove_spec_ref` — following one `.forge/repo.yml` reference (M3 X1,
//! plan 03 §4.4, ADR-0023 Amendment X1).
//!
//! The route proved `SpecRefRead` on the referencing project. Every project
//! a `repo:` entry names is then decided again here with `authorize()`, on
//! the referenced project's facts as the lookup layer loaded them **for the
//! same reader**, and with the proof's principal — never one the caller
//! passes. A reference therefore never lends the referencing project's
//! access to the referenced one: a reader who may not browse the spec
//! repository learns nothing about it (the route answers `unavailable`
//! whatever the refusal was). Pure: no I/O.
//!
//! [`prove_spec_ref_for_every_person`] (M3 X2, `forge/spec-drift`): forge's
//! own check is shown to **every** reader of the referencing project, so a
//! reference is followed only when every signed-in person may read the
//! referenced project — decided by `authorize()` for a signed-in person who
//! holds no role there (in practice: an internal project, not pending
//! deletion). Service accounts read the run by membership of the
//! referencing project only; they are the automation of a project whose
//! people may all read what is disclosed (ADR-0023 Amendment X2).

use forge_core::UserId;
use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
use crate::model::{AccountKind, Credential, Principal};
use crate::prove::{prove, Authorized, LoadedProject, ProjectCtx};

/// Decide `SpecRefRead` on `referenced` for the reader of `proof`. Denies
/// like `authorize()`; a proof for another action is `FG-SYS-0001
/// reason=proof_action_mismatch` (a caller bug, never a pass).
pub fn prove_spec_ref(
    proof: &Authorized<ProjectCtx>,
    referenced: LoadedProject,
) -> Result<Authorized<ProjectCtx>, ForgeError> {
    if proof.action() != Action::SpecRefRead {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_action_mismatch")
            .with_context("action", proof.action().name()));
    }
    prove(
        proof.principal(),
        Action::SpecRefRead,
        ProjectCtx::new(referenced),
    )
}

/// The least reader: a signed-in, active person in a session, no
/// administrator, with no role in the project decided. Its id is never a
/// row (ids start at 1) and no rule of `SpecRefRead` reads it.
fn every_person() -> Principal {
    Principal::authenticated(
        UserId::new(0),
        false,
        true,
        Credential::Session,
        AccountKind::Human,
    )
}

/// Decide `SpecRefRead` on `referenced` for every signed-in person: the
/// loader's role (read for no one, or for whoever) is dropped, so only the
/// project's visibility and deletion state decide. Denies like
/// `authorize()` (`FG-REPO-0005` for a private project, `FG-REPO-0006`
/// pending deletion).
pub fn prove_spec_ref_for_every_person(referenced: LoadedProject) -> Result<(), ForgeError> {
    prove(
        &every_person(),
        Action::SpecRefRead,
        ProjectCtx::new(LoadedProject {
            role: None,
            ..referenced
        }),
    )
    .map(drop)
}
