//! forge-auth: the single authorization decision point (CLAUDE.md §3-4,
//! ADR-0010).
//!
//! - [`authorize`] — pure decision over [`Principal`], [`Action`], [`Resource`]
//! - [`matrix`] — the permission matrix (specification as data; every row is
//!   asserted by the tests and summarized for the `authz-coverage` gate)
//!
//! Authentication providers and tokens join this crate in M1 phase 5 (DB).

mod action;
mod authorize;
pub mod matrix;
mod model;

pub use action::Action;
pub use authorize::authorize;
pub use model::{
    Principal, ProjectView, RefProtection, RefUpdate, Resource, Role, Scope, Visibility,
};

#[cfg(test)]
#[path = "authorize_tests.rs"]
mod tests;
