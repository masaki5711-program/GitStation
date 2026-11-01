//! forge-auth: the single authorization decision point (CLAUDE.md §3-4,
//! ADR-0010).
//!
//! - [`authorize`] — pure decision over [`Principal`], [`Action`], [`Resource`]
//! - [`prove`] — calls `authorize()` and returns the unforgeable proof
//!   [`Authorized`] a handler takes (ADR-0015); it adds no rule of its own
//! - [`matrix`] — the permission matrix (specification as data; every row is
//!   asserted by the tests and summarized for the `authz-coverage` gate)
//!
//! Authentication providers and tokens join this crate in M1 phase 5 (DB).

mod action;
mod authorize;
pub mod matrix;
mod matrix_rows;
mod model;
mod prove;

pub use action::Action;
pub use authorize::authorize;
pub use model::{
    Credential, MembershipChange, MembershipScope, NamespaceView, Principal, ProjectView, RefKind,
    RefProtection, RefUpdate, ResolvedVia, Resource, Role, Scope, UnresolvedReason, Visibility,
};
pub use prove::{
    prove, Authorized, InstanceCtx, LoadedNamespace, LoadedProject, LoadedTarget, ProjectCtx,
    ResolvedCtx, ResourceCtx, UnresolvedCtx,
};

#[cfg(test)]
#[path = "authorize_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "prove_tests.rs"]
mod prove_tests;
