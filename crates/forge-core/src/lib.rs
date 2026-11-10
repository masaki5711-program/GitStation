//! forge-core: domain primitives and runtime foundations shared by every
//! server-side crate. Depends only on `forge-errors` (CLAUDE.md §3.9).

pub mod act_as;
pub mod approval;
pub mod audit_details;
pub mod authenticator;
pub mod checks;
pub mod config;
pub mod cursor;
pub mod dedup_key;
pub mod discussion;
pub mod event_type;
// ── M3 I1c: the event feed's visible set and cursor position ──
pub mod feed;
// ── end M3 I1c ──
pub mod ids;
pub mod integration;
pub mod list_page;
pub mod merge;
pub mod merge_conditions;
pub mod merge_request;
pub mod mirror;
pub mod move_details;
pub mod namespace_path;
pub mod namespace_resolver;
// ── M3 N1: notification kinds and delivery states ──
pub mod notification;
// ── end M3 N1 ──
#[cfg(any(test, feature = "test-support"))]
pub mod path_vectors;
pub mod push;
pub mod push_log;
pub mod readiness;
pub mod ref_name;
pub mod ref_pattern;
pub mod ref_protection;
// ── M3 X1: `.forge/repo.yml` and its spec references ──
pub mod repo_config;
// ── end M3 X1 ──
pub mod repository;
pub mod request_id;
pub mod risk;
pub mod role;
pub mod scope;
pub mod session_token;
pub mod spec_version;
// ── M3 X4: requirement blocks, `@req` tags, the index, spec-lint and spec-trace ──
pub mod req_tags;
// Moved to forge-doc (plan 13 stage 2 MR1); the path stays for every caller.
pub use forge_doc::spec_doc;
pub mod spec_index;
// ── end M3 X4 ──
pub mod telemetry;
pub mod token;
pub mod user_kind;
pub mod web_keys;
pub mod web_path;
// ── M3 I2: webhooks (ADR-0016 §8) ──
pub mod webhook;
// ── end M3 I2 ──
pub mod work_item;

pub use approval::{ApprovalRuleChange, UnapproveReason, UncountedReason};
pub use audit_details::{
    AuditDetails, ChangedField, IdentityLinkVia, ServiceAccountField, SessionRevokeReason,
};
pub use authenticator::{Authenticator, SessionAuthenticator, SessionIdentity, TokenIdentity};
pub use checks::{
    CheckConclusion, CheckResultsSource, CheckRun, CheckRunsOnHead, CheckStatus,
    MAX_CHECK_ANNOTATIONS, MAX_CHECK_RUNS_PER_COMMIT,
};
pub use cursor::CursorKeys;
pub use dedup_key::{DedupKey, DedupPart};
pub use discussion::{CommentAction, DiscussionKind, DiscussionSide};
pub use event_type::{EventType, VisibilityClass};
pub use feed::{
    DevStatusPosition, FeedBinding, FeedMaterial, FeedPosition, VisibleProject, VisibleSet,
};
pub use ids::{
    ApprovalId, ApprovalRuleId, DiscussionId, GroupId, IntegrationId, MergeRequestId, NamespaceId,
    NoteId, ProjectId, ProtectedRefId, RepoId, TombstoneId, UserId, WebhookDeliveryId, WebhookId,
};
pub use integration::{IntegrationChange, IntegrationField, IntegrationKind};
pub use list_page::{GroupListing, GroupPageLoader, ProjectPageLoader};
pub use merge::{MergeMethod, MergeState, SourceOutcome};
pub use merge_conditions::{
    CheckName, CheckNameError, CheckNamespace, MergeRule, MergeRuleChange, MergeSettingsChange,
    MergeSettingsValues,
};
pub use merge_request::{
    CheckReason, MergeRequestChange, MergeRequestField, MergeRequestState, MergeRequestTransition,
    MergeRequestVersionChange, MergeabilityStatus,
};
pub use move_details::{MoveSummary, RoleChange, TombstoneKind};
pub use namespace_path::{LookupPath, NamespacePath, PathSegment};
pub use namespace_resolver::{
    GroupRecord, NamespaceKind, NamespaceOwner, NamespaceResolver, ProjectAccess,
    ProjectAccessLoader, ProjectRecord, ProjectRepository, Resolution, Resolved, ResolvedTarget,
};
pub use notification::{NotificationKind, NotificationState};
pub use push::{
    ObjectId, PushRecord, PushRecorded, PushRecorder, PushReplay, PushUpdateKind, PushedCommits,
    PushedRef, Pusher, MAX_PUSHED_COMMITS, PUSH_RECORD_BUDGET,
};
pub use push_log::{PushApplied, PushAppliedRef, PUSH_APPLIED_EVENT};
pub use ref_name::RefName;
pub use ref_pattern::RefPattern;
pub use ref_protection::{ProtectionLoader, ProtectionRule, ProtectionRules};
pub use repository::RepositoryState;
pub use request_id::RequestId;
pub use role::{Role, Visibility};
pub use scope::Scope;
pub use user_kind::UserKind;
pub use web_path::{
    log_safe_path, parse_positive_id, parse_web_path, GitRoute, InstanceRoute, ParsedPath,
    RouteWord, TailSegments,
};
pub use webhook::{
    DisableReason, WebhookChange, WebhookDeliverySet, WebhookField, WebhookMaterial,
};
pub use work_item::{
    find_refs, LinkActorKind, LinkSource, LinkTarget, UnlinkSource, WorkItem, WorkItemChange,
    WorkItemRef,
};

#[cfg(test)]
mod audit_details_tests;
#[cfg(test)]
mod checks_tests;
#[cfg(test)]
mod cursor_tests;
#[cfg(test)]
mod dedup_key_tests;
#[cfg(test)]
mod event_type_tests;
#[cfg(test)]
mod feed_tests;
#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod merge_request_tests;
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
mod session_token_tests;
#[cfg(test)]
mod token_tests;
#[cfg(test)]
mod user_kind_tests;
#[cfg(test)]
mod web_keys_tests;
#[cfg(test)]
mod web_path_tests;
#[cfg(test)]
mod webhook_tests;
#[cfg(test)]
mod work_item_tests;
