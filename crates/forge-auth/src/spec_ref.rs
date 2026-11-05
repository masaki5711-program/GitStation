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

use forge_errors::{ErrorCode, ForgeError};

use crate::action::Action;
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
