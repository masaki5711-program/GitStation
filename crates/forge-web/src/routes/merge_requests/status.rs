//! The merge status of a merge request (M3 R6, ADR-0024 §7):
//! `GET …/merge-requests/{iid}/status` (`MergeRequestRead`) answers
//! `evaluate_merge()` on the facts as they are now — mergeable or not, and
//! **every** blocking reason in ADR-0024 §7's order — with the inputs an
//! agent needs to act on them (the approvals, the required checks, the
//! discussions, CODEOWNERS enforcement). Reads only; a merge decides again
//! under its own locks. This JSON is a stable contract
//! (`schema_version`, `docs/api/merge-status.md`, `gstn mr status --json`):
//! fields are added, never renamed or removed, within a version. U2 added
//! `viewer` (what the caller could do now: trial `authorize()` calls, no
//! rule of its own) and made `reasons[].reason` an enum of the names.

use axum::http::StatusCode;
use axum::response::Response;
use forge_auth::{MergeRequestCtx, MergeRequestViewer};
use forge_core::UserId;
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{BlockReason, MergeStatus};
use serde::Serialize;

use super::approvals::StoredMr;
use super::{repository, respond, review, viewer_read};
use crate::app::AppState;
use crate::registry::Dispatched;
use crate::routes::write::{bounded, json};

/// The path after `{iid}` of [`STATUS`].
pub const STATUS_SUFFIX: &str = "/status";
/// `GET` the merge status.
pub const STATUS: &str = "/api/v1/projects/{id}/merge-requests/{iid}/status";
/// `GET …/status`: the merge request.
pub const STATUS_OF: StoredMr = StoredMr(STATUS_SUFFIX);
/// The version of the contract.
pub const SCHEMA_VERSION: u32 = 1;

/// The name of a blocking reason (ADR-0024 §7): one per
/// `forge_review::BlockReason` variant, spelled as `BlockReason::as_str`
/// (`status_reason_schema_lists_every_block_reason`). A new reason is a
/// new name here — added, never renamed, within `schema_version` 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatusReasonName {
    NotOpen,
    Draft,
    MergeInProgress,
    SourceBranchMissing,
    MergeabilityUnknown,
    Conflict,
    FfNotPossible,
    NothingToMerge,
    ApprovalsMissing,
    CodeownerApprovalMissing,
    CodeownersInvalid,
    RequiredCheckMissing,
    RequiredCheckNotSuccessful,
    UnresolvedDiscussions,
    /// M3 I1d.
    WorkItemLinkMissing,
    ConfigChangeNeedsMaintainerApproval,
    ChangedPathsUnknown,
}

impl StatusReasonName {
    /// Every name, in ADR-0024 §7's order.
    pub const ALL: [Self; 17] = [
        Self::NotOpen,
        Self::Draft,
        Self::MergeInProgress,
        Self::SourceBranchMissing,
        Self::MergeabilityUnknown,
        Self::Conflict,
        Self::FfNotPossible,
        Self::NothingToMerge,
        Self::ApprovalsMissing,
        Self::CodeownerApprovalMissing,
        Self::CodeownersInvalid,
        Self::RequiredCheckMissing,
        Self::RequiredCheckNotSuccessful,
        Self::UnresolvedDiscussions,
        Self::WorkItemLinkMissing,
        Self::ConfigChangeNeedsMaintainerApproval,
        Self::ChangedPathsUnknown,
    ];

    /// The name of `reason` (exhaustive: a new `BlockReason` does not
    /// build until it is named here).
    pub fn of(reason: &BlockReason) -> Self {
        match reason {
            BlockReason::NotOpen => Self::NotOpen,
            BlockReason::Draft => Self::Draft,
            BlockReason::MergeInProgress => Self::MergeInProgress,
            BlockReason::SourceBranchMissing => Self::SourceBranchMissing,
            BlockReason::MergeabilityUnknown => Self::MergeabilityUnknown,
            BlockReason::Conflict => Self::Conflict,
            BlockReason::FfNotPossible => Self::FfNotPossible,
            BlockReason::NothingToMerge => Self::NothingToMerge,
            BlockReason::ApprovalsMissing { .. } => Self::ApprovalsMissing,
            BlockReason::CodeownerApprovalMissing { .. } => Self::CodeownerApprovalMissing,
            BlockReason::CodeownersInvalid { .. } => Self::CodeownersInvalid,
            BlockReason::RequiredCheckMissing { .. } => Self::RequiredCheckMissing,
            BlockReason::RequiredCheckNotSuccessful { .. } => Self::RequiredCheckNotSuccessful,
            BlockReason::UnresolvedDiscussions { .. } => Self::UnresolvedDiscussions,
            BlockReason::WorkItemLinkMissing => Self::WorkItemLinkMissing,
            BlockReason::ConfigChangeNeedsMaintainerApproval => {
                Self::ConfigChangeNeedsMaintainerApproval
            }
            BlockReason::ChangedPathsUnknown { .. } => Self::ChangedPathsUnknown,
        }
    }
}

/// One blocking reason.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusReasonBody {
    /// A name of ADR-0024 §7 (`docs/api/merge-status.md`).
    pub reason: StatusReasonName,
    /// `key=value` pairs (`rule=… required=… counted=…`, `name=…`,
    /// `count=…`, …); `null` for the plain reasons.
    #[schema(required = true)]
    pub detail: Option<String>,
}

/// One approval requirement (an approval rule, or `@protected_branch`).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusRuleBody {
    pub name: String,
    pub applies: bool,
    pub required: u32,
    pub counted: u32,
}

/// One live approval as counted now.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusApprovalBody {
    pub approver_id: i64,
    pub username: String,
    pub counted: bool,
    #[schema(required = true)]
    pub uncounted_reason: Option<String>,
}

/// The approval side.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusApprovalsBody {
    pub rules: Vec<StatusRuleBody>,
    pub approvals: Vec<StatusApprovalBody>,
    /// CODEOWNERS rules the changed paths need: `pattern`, satisfied.
    pub codeowners: Vec<StatusCodeownerBody>,
}

/// One CODEOWNERS rule the changes need.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusCodeownerBody {
    pub pattern: String,
    pub owners: Vec<String>,
    pub satisfied: bool,
}

/// One required check and its run on the head.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusCheckBody {
    pub name: String,
    /// `null`: no run reported (never success).
    #[schema(required = true)]
    pub status: Option<String>,
    #[schema(required = true)]
    pub conclusion: Option<String>,
}

/// The discussions condition.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusDiscussionsBody {
    pub unresolved: u32,
}

/// The work item link condition (M3 I1d).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusWorkItemLinkBody {
    /// An open link to a work item of the project's integration binding
    /// exists (`false`: `work_item_link_missing`).
    pub linked: bool,
}

/// CODEOWNERS enforcement now.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusCodeownersBody {
    pub enforced: bool,
    /// While suspended: when enforcement comes back by itself (UTC).
    #[schema(required = true)]
    pub suspended_until: Option<String>,
}

/// What the caller could do now (U2): each flag is `authorize()` on the
/// context the action's route decides on (`forge_auth::MergeRequestViewer`)
/// — a hint for the screen, never a permission (the route decides again).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusViewerBody {
    /// `POST …/approve` would be allowed (never for the author).
    pub can_approve: bool,
    /// `POST …/unapprove` would be allowed (pair it with `approved_by_me`:
    /// there may be nothing to withdraw).
    pub can_unapprove: bool,
    /// `POST …/merge` would be allowed (the target's minimum push role;
    /// whether the merge request may merge now is `mergeable`).
    pub can_merge: bool,
    /// `PATCH …/{iid}` with `draft: false` would be allowed (meaningful
    /// only for a draft).
    pub can_mark_ready: bool,
    /// The caller has a live approval of the merge request.
    pub approved_by_me: bool,
}

/// `GET …/status` (the stable contract, `schema_version` 1).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeRequestStatusBody {
    pub schema_version: u32,
    #[schema(minimum = 1)]
    pub iid: i64,
    /// `null` before the first version (nothing can merge yet).
    #[schema(required = true)]
    pub head_sha: Option<String>,
    /// Without `refs/heads/`.
    pub target_branch: String,
    /// The method evaluated: the project's default.
    pub method: String,
    /// Nothing blocks now (a merge decides again under its locks).
    pub mergeable: bool,
    /// Every blocking reason, in the fixed order (the first is the one-line
    /// "why it is stopped").
    pub reasons: Vec<StatusReasonBody>,
    /// `null` before the first version.
    #[schema(required = true)]
    pub approvals: Option<StatusApprovalsBody>,
    /// The checks the target's merge rule requires (empty: none).
    pub checks: Vec<StatusCheckBody>,
    /// `null` when the merge rule does not require resolved discussions.
    #[schema(required = true)]
    pub discussions: Option<StatusDiscussionsBody>,
    /// M3 I1d (added within `schema_version` 1): `null` when the merge rule
    /// does not require a work item link.
    #[schema(required = true)]
    pub work_item_link: Option<StatusWorkItemLinkBody>,
    pub codeowners: StatusCodeownersBody,
    /// What the caller could do now (added within `schema_version` 1, U2).
    pub viewer: StatusViewerBody,
}

pub(super) fn status_body(
    iid: i64,
    s: &MergeStatus,
    viewer: MergeRequestViewer,
    caller: Option<UserId>,
) -> MergeRequestStatusBody {
    let approved_by_me = match (&s.approvals, caller) {
        (Some(report), Some(me)) => report.approvals.iter().any(|a| a.approver == me),
        (None, _) | (_, None) => false,
    };
    let suspended_until = match &s.settings.codeowners {
        forge_review::CodeownersEnforcement::Suspended { until, .. } => Some(until.clone()), // clone: the body owns it
        forge_review::CodeownersEnforcement::Enforced
        | forge_review::CodeownersEnforcement::Expired { .. } => None,
    };
    MergeRequestStatusBody {
        schema_version: SCHEMA_VERSION,
        iid,
        head_sha: s.head_sha.as_ref().map(|h| h.as_str().to_owned()),
        target_branch: s
            .target_branch
            .branch()
            .unwrap_or(s.target_branch.as_str())
            .to_owned(),
        method: s.method.as_str().to_owned(),
        mergeable: s.mergeable,
        reasons: s
            .reasons
            .iter()
            .map(|r| StatusReasonBody {
                reason: StatusReasonName::of(r),
                detail: r.detail(),
            })
            .collect(),
        approvals: s.approvals.as_ref().map(|report| StatusApprovalsBody {
            rules: report
                .rules
                .iter()
                .map(|r| StatusRuleBody {
                    name: r.name.clone(), // clone: the report is borrowed
                    applies: r.applies,
                    required: r.required,
                    counted: r.counted,
                })
                .collect(),
            approvals: report
                .approvals
                .iter()
                .map(|a| StatusApprovalBody {
                    approver_id: a.approver.get(),
                    username: a.username.clone(), // clone: as above
                    counted: a.counted,
                    uncounted_reason: a.uncounted_reason.map(|r| r.as_str().to_owned()),
                })
                .collect(),
            codeowners: report
                .codeowners
                .iter()
                .map(|c| StatusCodeownerBody {
                    pattern: c.pattern.clone(), // clone: as above
                    owners: c.owners.clone(),   // clone: as above
                    satisfied: c.satisfied,
                })
                .collect(),
        }),
        checks: s
            .checks
            .iter()
            .map(|c| StatusCheckBody {
                name: c.name.as_str().to_owned(),
                status: c.run.map(|r| r.status.as_str().to_owned()),
                conclusion: c
                    .run
                    .and_then(|r| r.conclusion)
                    .map(|k| k.as_str().to_owned()),
            })
            .collect(),
        discussions: s
            .discussions
            .map(|unresolved| StatusDiscussionsBody { unresolved }),
        work_item_link: s
            .work_item_link
            .map(|linked| StatusWorkItemLinkBody { linked }),
        codeowners: StatusCodeownersBody {
            enforced: s.settings.codeowners.enforced(),
            suspended_until,
        },
        viewer: StatusViewerBody {
            can_approve: viewer.can_approve,
            can_unapprove: viewer.can_unapprove,
            can_merge: viewer.can_merge,
            can_mark_ready: viewer.can_mark_ready,
            approved_by_me,
        },
    }
}

/// `GET …/status` → `200` the merge status now.
pub async fn status(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    // Owned before the first await: the request (its body) is not `Sync`.
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let (project, iid) = *w.proof.id();
        let iid = iid.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid")
        })?;
        let review = review(&state)?;
        let answer = review
            .merge_status(&w.ctx.request_id, &w.proof, repo?, None)
            .await?;
        // The protection the merge route's loader reads (U2 viewer flags):
        // the rules the status read with the approval facts, else (no
        // version yet) read here; a failed read closes the flags only.
        let protection = match answer.target_protection {
            Some(read) => Some(read),
            None => {
                let target = &w.proof.ctx().merge_request().target_branch;
                let read = bounded(
                    &state,
                    "merge_target_protection",
                    review.target_protection(project, target),
                )
                .await;
                viewer_read(&w.ctx, "merge_target_protection", read)
            }
        };
        let principal = w.proof.principal();
        let viewer = protection.map_or(MergeRequestViewer::CLOSED, |p| {
            w.proof.ctx().viewer(principal, p)
        });
        json(status_body(iid, &answer, viewer, principal.user()))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}
