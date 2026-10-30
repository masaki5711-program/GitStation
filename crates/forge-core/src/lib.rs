//! forge-core: domain primitives and runtime foundations shared by every
//! server-side crate. Depends only on `forge-errors` (CLAUDE.md §3.9).

pub mod audit_details;
pub mod config;
pub mod ids;
pub mod namespace_path;
#[cfg(any(test, feature = "test-support"))]
pub mod path_vectors;
pub mod readiness;
pub mod ref_name;
pub mod request_id;
pub mod role;
pub mod scope;
pub mod telemetry;
pub mod token;
pub mod user_kind;

pub use audit_details::AuditDetails;
pub use ids::{GroupId, NamespaceId, ProjectId, RepoId, TombstoneId, UserId};
pub use namespace_path::PathSegment;
pub use ref_name::RefName;
pub use request_id::RequestId;
pub use role::{Role, Visibility};
pub use scope::Scope;
pub use user_kind::UserKind;

#[cfg(test)]
mod audit_details_tests;
#[cfg(test)]
mod namespace_path_tests;
#[cfg(test)]
mod role_tests;
#[cfg(test)]
mod scope_tests;
#[cfg(test)]
mod token_tests;
#[cfg(test)]
mod user_kind_tests;
