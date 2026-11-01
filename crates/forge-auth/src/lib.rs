//! forge-auth: the single authorization decision point (CLAUDE.md §3-4,
//! ADR-0010).
//!
//! - [`authorize`] — pure decision over [`Principal`], [`Action`], [`Resource`]
//! - [`prove`] — calls `authorize()` and returns the unforgeable proof
//!   [`Authorized`] a handler takes (ADR-0015); it adds no rule of its own
//! - [`authorize_ref_updates`] — every ref of a push through `authorize()`,
//!   on the project-level `RepoPush` proof (M1 phase 9 MR4)
//! - [`matrix`] — the permission matrix (specification as data; every row is
//!   asserted by the tests and summarized for the `authz-coverage` gate)
//!
//! Authentication providers and tokens join this crate in M1 phase 5 (DB).

mod action;
mod authorize;
mod contexts;
pub mod matrix;
mod matrix_rows;
mod model;
mod prove;
mod ref_updates;

pub use action::Action;
pub use authorize::authorize;
pub use model::{
    Credential, MembershipChange, MembershipScope, NamespaceView, Principal, ProjectView, RefKind,
    RefProtection, RefUpdate, ResolvedVia, Resource, Role, Scope, TombstoneRelease, TransferTarget,
    UnresolvedReason, Visibility,
};
pub use prove::{
    prove, AccountFacts, Authorized, GroupCtx, GroupMemberCtx, GroupTransferCtx, InstanceCtx,
    LoadedGroup, LoadedMembership, LoadedNamespace, LoadedParent, LoadedProject, LoadedProtection,
    LoadedTarget, LoadedTombstone, LoadedTransferTarget, LoadedUserNamespace, ParentCtx,
    ProjectCtx, ProjectMemberCtx, RefProtectionCtx, ResolvedCtx, ResourceCtx, TombstoneCtx,
    TransferCtx, UnresolvedCtx, UserNamespaceCtx, UserRenameCtx, VisibilityChangeCtx,
};
pub use ref_updates::{authorize_ref_updates, RefUpdateCtx};

#[cfg(test)]
#[path = "authorize_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "prove_tests.rs"]
mod prove_tests;

#[cfg(test)]
#[path = "ref_updates_tests.rs"]
mod ref_updates_tests;
