//! forge-auth: the single authorization decision point (CLAUDE.md §3-4,
//! ADR-0010).
//!
//! - [`authorize`] — pure decision over [`Principal`], [`Action`], [`Resource`]
//! - [`prove`] — calls `authorize()` and returns the unforgeable proof
//!   [`Authorized`] a handler takes (ADR-0015); it adds no rule of its own
//! - the viewer flags ([`MergeRequestViewer`], [`MergeSettingsViewer`]) —
//!   trial `authorize()` calls on the write routes' contexts (U2)
//! - [`authorize_ref_updates`] — every ref of a push through `authorize()`,
//!   on the project-level `RepoPush` proof (M1 phase 9 MR4)
//! - [`matrix`] — the permission matrix (specification as data; every row is
//!   asserted by the tests and summarized for the `authz-coverage` gate)
//!
//! Authentication providers and tokens join this crate in M1 phase 5 (DB).

mod action;
mod authorize;
mod codeowners_ctx;
mod contexts;
mod feed_ctx;
mod integration_ctx;
pub mod matrix;
mod matrix_review;
mod matrix_rows;
mod matrix_rows_approval;
mod matrix_rows_checks;
mod matrix_rows_conditions;
mod matrix_rows_dev_status;
mod matrix_rows_discussion;
mod matrix_rows_feed;
mod matrix_rows_integrations;
mod matrix_rows_merge;
mod matrix_rows_people;
mod matrix_rows_service_accounts;
mod matrix_rows_work_items;
mod merge_request_ctx;
mod model;
mod prove;
mod ref_updates;
mod viewer;
// ── M3 I2 ──
mod matrix_rows_webhooks;
mod webhook_ctx;
// ── end M3 I2 ──

pub use action::Action;
pub use authorize::authorize;
pub use codeowners_ctx::{CodeownersToggleCtx, MergeSettingsCtx};
pub use feed_ctx::{prove_visible, IntegrationFeedCtx, LoadedIntegrationFeed};
pub use integration_ctx::{IntegrationBindingCtx, IntegrationCtx};
pub use merge_request_ctx::{DiscussionCtx, LoadedDiscussion, LoadedMergeRequest, MergeRequestCtx};
pub use model::{
    AccountKind, AdmissionAccount, CodeownersToggle, Credential, IntegrationFeedView,
    MembershipChange, MembershipScope, MergeRequestEdit, MergeRequestState, NamespaceView,
    Principal, ProjectView, RefKind, RefProtection, RefUpdate, ResolvedVia, Resource, Role, Scope,
    ServiceAccountView, TombstoneRelease, TransferTarget, UnresolvedReason, Visibility,
};
pub use prove::{
    prove, AccountFacts, Authorized, CheckNamespaceGrantCtx, CheckRunCtx, GroupCtx, GroupMemberCtx,
    GroupTransferCtx, InstanceCtx, LoadedGroup, LoadedMembership, LoadedNamespace, LoadedParent,
    LoadedProject, LoadedProtection, LoadedTarget, LoadedTombstone, LoadedTransferTarget,
    LoadedUserNamespace, NotificationPreferencesCtx, ParentCtx, ProjectCtx, ProjectMemberCtx,
    RefProtectionCtx, ResolvedCtx, ResourceCtx, ServiceAccountCtx, ServiceAccountTokenCtx,
    ServiceAccountTokenRequestCtx, SessionCtx, TokenCtx, TokenOwnerCtx, TokenRequestCtx,
    TombstoneCtx, TransferCtx, UnresolvedCtx, UserNamespaceCtx, UserRenameCtx, UserTargetCtx,
    VisibilityChangeCtx,
};
pub use ref_updates::{authorize_ref_updates, RefUpdateCtx};
pub use viewer::{MergeRequestViewer, MergeSettingsViewer, TRIAL_SUSPENSION_MINUTES};
pub use webhook_ctx::{prove_webhook_delivery, WebhookCtx, WebhookDeliveryCtx};

#[cfg(test)]
#[path = "authorize_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "prove_tests.rs"]
mod prove_tests;

#[cfg(test)]
mod discussion_tests;

#[cfg(test)]
mod merge_request_tests;

#[cfg(test)]
mod feed_tests;

#[cfg(test)]
mod service_account_tests;

#[cfg(test)]
#[path = "ref_updates_tests.rs"]
mod ref_updates_tests;

#[cfg(test)]
#[path = "viewer_tests.rs"]
mod viewer_tests;
#[cfg(test)]
mod webhook_tests;
