//! forge-core: domain primitives and runtime foundations shared by every
//! server-side crate. Depends only on `forge-errors` (CLAUDE.md §3.9).

pub mod audit_details;
pub mod authenticator;
pub mod config;
pub mod dedup_key;
pub mod event_type;
pub mod ids;
pub mod namespace_path;
pub mod namespace_resolver;
#[cfg(any(test, feature = "test-support"))]
pub mod path_vectors;
pub mod push;
pub mod push_log;
pub mod readiness;
pub mod ref_name;
pub mod ref_pattern;
pub mod ref_protection;
pub mod repository;
pub mod request_id;
pub mod role;
pub mod scope;
pub mod telemetry;
pub mod token;
pub mod user_kind;
pub mod web_path;

pub use audit_details::AuditDetails;
pub use authenticator::{Authenticator, TokenIdentity};
pub use dedup_key::{DedupKey, DedupPart};
pub use event_type::{EventType, VisibilityClass};
pub use ids::{GroupId, NamespaceId, ProjectId, ProtectedRefId, RepoId, TombstoneId, UserId};
pub use namespace_path::{LookupPath, NamespacePath, PathSegment};
pub use namespace_resolver::{
    NamespaceKind, NamespaceOwner, NamespaceResolver, ProjectAccess, ProjectAccessLoader,
    ProjectRecord, Resolution, Resolved, ResolvedTarget,
};
pub use push::{
    ObjectId, PushRecord, PushRecorded, PushRecorder, PushReplay, PushUpdateKind, PushedRef, Pusher,
};
pub use push_log::{PushApplied, PushAppliedRef, PUSH_APPLIED_EVENT};
pub use ref_name::RefName;
pub use ref_pattern::RefPattern;
pub use ref_protection::{ProtectionLoader, ProtectionRule, ProtectionRules};
pub use repository::{RepositoryKind, RepositoryState};
pub use request_id::RequestId;
pub use role::{Role, Visibility};
pub use scope::Scope;
pub use user_kind::UserKind;
pub use web_path::{
    log_safe_path, parse_positive_id, parse_web_path, GitRoute, InstanceRoute, ParsedPath,
    RouteWord, TailSegments,
};

#[cfg(test)]
mod audit_details_tests;
#[cfg(test)]
mod dedup_key_tests;
#[cfg(test)]
mod event_type_tests;
#[cfg(test)]
mod namespace_path_tests;
#[cfg(test)]
mod push_tests;
#[cfg(test)]
mod ref_pattern_tests;
#[cfg(test)]
mod repository_tests;
#[cfg(test)]
mod role_tests;
#[cfg(test)]
mod scope_tests;
#[cfg(test)]
mod token_tests;
#[cfg(test)]
mod user_kind_tests;
#[cfg(test)]
mod web_path_tests;
