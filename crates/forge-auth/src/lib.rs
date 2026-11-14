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

mod act_as;
mod act_as_ctx;
mod action;
mod authorize;
mod auto_merge;
mod codeowners_ctx;
// M4 C5: the CI outputs (ADR-0029 decision 5).
mod ci_output;
mod contexts;
mod feed_ctx;
mod integration_ctx;
// M4 C0: CI runner and job principals (ADR-0029).
mod job;
pub mod matrix;
mod matrix_review;
mod matrix_rows;
mod matrix_rows_act_as;
mod matrix_rows_activity;
mod matrix_rows_approval;
mod matrix_rows_bindings;
mod matrix_rows_checks;
mod matrix_rows_ci;
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
mod repo_copy;
mod viewer;
// ── M3 I2 ──
mod matrix_rows_webhooks;
mod webhook_ctx;
// ── end M3 I2 ──
// ── M3 O2: the push mirror ──
mod matrix_rows_mirror;
// ── end M3 O2 ──
// Plan 13 Q13: the document lint's migration setting.
mod matrix_rows_doc_lint;
// M4 C2: CI pipelines.
mod matrix_rows_merge_queue;
// M4 C9: auto-merge.
mod matrix_rows_auto_merge;
mod matrix_rows_pipelines;
// M4 C5: CI outputs.
mod matrix_rows_ci_outputs;
// M4 C6: CI variables.
mod matrix_rows_ci_variables;
// M4 C11: deployments.
mod deploy_ctx;
mod matrix_rows_deploy;
// ── M3 X1 ──
mod matrix_rows_spec_refs;
// Plan 06 D-5 safety net: whole copies of a repository by agents.
mod matrix_rows_repo_copy;
mod spec_ref;
// ── end M3 X1 ──

pub use act_as::{act_as_admission, act_as_route};
pub use act_as_ctx::{ActAsBindingConfirmCtx, ActAsConfirmCtx, LoadedConfirmIntegration};
pub use action::{ActAsRule, Action, CiRule};
pub use authorize::authorize;
pub use auto_merge::{auto_merge_may, AutoMergeCtx};
pub use ci_output::{authorize_ci_artifact, authorize_ci_output, CiOutputAllowed};
pub use codeowners_ctx::{CodeownersToggleCtx, MergeSettingsCtx};
pub use deploy_ctx::{DeploymentCtx, LoadedDeployment};
pub use feed_ctx::{prove_visible, IntegrationFeedCtx, LoadedIntegrationFeed};
pub use integration_ctx::{IntegrationBindingCtx, IntegrationCtx};
pub use job::authorize_pinned_copy;
pub use merge_request_ctx::{DiscussionCtx, LoadedDiscussion, LoadedMergeRequest, MergeRequestCtx};
pub use model::{
    AccountKind, ActAsConfirmView, ActAsCredential, ActAsSubject, ActAsView, AdmissionAccount,
    AssertionFacts, AssertionSubject, AutoMergeTarget, BindingSubject, CodeownersToggle, CopyMark,
    Credential, DeploymentView, IntegrationFeedView, JobCredential, JobTokenState,
    MembershipChange, MembershipScope, MergeRequestEdit, MergeRequestState, NamespaceView,
    PersonalDisclosure, PinnedCopy, Principal, ProjectView, RefKind, RefProtection, RefUpdate,
    ResolvedVia, Resource, Role, RunnerCredential, RunnerJob, Scope, ServiceAccountView,
    TombstoneRelease, TransferTarget, UnresolvedReason, ViaView, Visibility,
};
pub use prove::{
    prove, AccountFacts, Authorized, CheckNamespaceGrantCtx, CheckRunCtx, GroupCtx, GroupMemberCtx,
    GroupTransferCtx, InstanceCtx, JobSelfCtx, LoadedGroup, LoadedMembership, LoadedNamespace,
    LoadedParent, LoadedProject, LoadedProtection, LoadedTarget, LoadedTombstone,
    LoadedTransferTarget, LoadedUserNamespace, NotificationPreferencesCtx, ParentCtx,
    PinnedCopyCtx, ProjectCtx, ProjectEventsCtx, ProjectMemberCtx, RefProtectionCtx, ResolvedCtx,
    ResourceCtx, RunnerJobCtx, ServiceAccountCtx, ServiceAccountTokenCtx,
    ServiceAccountTokenRequestCtx, SessionCtx, TokenCtx, TokenOwnerCtx, TokenRequestCtx,
    TombstoneCtx, TransferCtx, UnresolvedCtx, UserActivityCtx, UserMergeRequestsCtx,
    UserNamespaceCtx, UserRenameCtx, UserTargetCtx, VisibilityChangeCtx,
};
pub use ref_updates::{authorize_ref_updates, RefUpdateCtx};
pub use repo_copy::authorize_repo_copy;
pub use spec_ref::{prove_spec_ref, prove_spec_ref_for_every_person};
pub use viewer::{
    ActAsTipGate, ActAsTrial, ActAsViewer, MergeRequestViewer, MergeSettingsViewer,
    TRIAL_SUSPENSION_MINUTES,
};
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
#[path = "spec_ref_tests.rs"]
mod spec_ref_tests;
#[cfg(test)]
#[path = "viewer_tests.rs"]
mod viewer_tests;

#[cfg(test)]
mod act_as_tests;
#[cfg(test)]
mod job_tests;
#[cfg(test)]
mod webhook_tests;
// M3 X5: the context API's action and the agent audience (plan 06 D-5).
#[cfg(test)]
mod context_tests;
