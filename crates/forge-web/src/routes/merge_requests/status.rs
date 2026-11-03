//! The merge status of a merge request (M3 R6, ADR-0024 §7):
//! `GET …/merge-requests/{iid}/status` (`MergeRequestRead`) answers
//! `evaluate_merge()` on the facts as they are now — mergeable or not, and
//! **every** blocking reason in ADR-0024 §7's order — with the inputs an
//! agent needs to act on them (the approvals, the required checks, the
//! discussions, CODEOWNERS enforcement). Reads only; a merge decides again
//! under its own locks. This JSON is a stable contract
//! (`schema_version`, `docs/api/merge-status.md`, `gstn mr status --json`):
//! fields are added, never renamed or removed, within a version.

use axum::http::StatusCode;
use axum::response::Response;
use forge_auth::MergeRequestCtx;
use forge_errors::{ErrorCode, ForgeError};
use forge_review::MergeStatus;
use serde::Serialize;

use super::approvals::StoredMr;
use super::{repository, respond, review};
use crate::app::AppState;
use crate::registry::Dispatched;
use crate::routes::write::json;

/// The path after `{iid}` of [`STATUS`].
pub const STATUS_SUFFIX: &str = "/status";
/// `GET` the merge status.
pub const STATUS: &str = "/api/v1/projects/{id}/merge-requests/{iid}/status";
/// `GET …/status`: the merge request.
pub const STATUS_OF: StoredMr = StoredMr(STATUS_SUFFIX);
/// The version of the contract.
pub const SCHEMA_VERSION: u32 = 1;

/// One blocking reason.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusReasonBody {
    /// A name of ADR-0024 §7: `not_open`, `draft`, `merge_in_progress`,
    /// `source_branch_missing`, `mergeability_unknown`, `conflict`,
    /// `ff_not_possible`, `nothing_to_merge`, `approvals_missing`,
    /// `codeowner_approval_missing`, `codeowners_invalid`,
    /// `required_check_missing`, `required_check_not_successful`,
    /// `unresolved_discussions`, `config_change_needs_maintainer_approval`,
    /// `changed_paths_unknown`.
    pub reason: String,
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

/// CODEOWNERS enforcement now.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusCodeownersBody {
    pub enforced: bool,
    /// While suspended: when enforcement comes back by itself (UTC).
    #[schema(required = true)]
    pub suspended_until: Option<String>,
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
    pub codeowners: StatusCodeownersBody,
}

pub(super) fn status_body(iid: i64, s: &MergeStatus) -> MergeRequestStatusBody {
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
                reason: r.as_str().to_owned(),
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
        codeowners: StatusCodeownersBody {
            enforced: s.settings.codeowners.enforced(),
            suspended_until,
        },
    }
}

/// `GET …/status` → `200` the merge status now.
pub async fn status(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    // Owned before the first await: the request (its body) is not `Sync`.
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let iid = w.proof.id().1.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid")
        })?;
        let answer = review(&state)?
            .merge_status(&w.ctx.request_id, &w.proof, repo?, None)
            .await?;
        json(status_body(iid, &answer))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}
