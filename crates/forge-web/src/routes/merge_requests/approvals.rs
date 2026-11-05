//! Approvals and approval rules (M3 R5, ADR-0024 §8, §9, §12):
//!
//! - `POST …/merge-requests/{iid}/approve` (`MergeRequestApprove`: people
//!   only, Developer, never the author, a token needs `merge_approve`) with
//!   the head the caller saw (`sha`); `POST …/{iid}/unapprove`
//!   (`MergeRequestUnapprove`) withdraws the caller's own approval;
//!   `GET …/{iid}/approvals` (`MergeRequestRead`) evaluates the approvals
//!   now (how each counts, the rules, the CODEOWNERS rules the changes
//!   need, what blocks the merge).
//! - `GET /api/v1/projects/{id}/approval-rules` (`ProjectRead`),
//!   `PUT|DELETE /api/v1/projects/{id}/approval-rules/{name}`
//!   (`ApprovalRuleManage`: people only, Maintainer).
//!
//! The registry proves the action on what the loader read; forge-review
//! decides again inside the transaction.
//!
//! U2 follow-up: a rule names its approvers (`users`, `groups`: id and
//! name, at the rule's own visibility — no lookup route) and its
//! `updated_at`; a rule write names the `updated_at` it read
//! (`expected_updated_at`, `null` to create only; another one is
//! FG-REVIEW-0018).

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{MergeRequestCtx, MergeRequestEdit, ProjectCtx};
use forge_core::{parse_positive_id, ObjectId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::approvals::{ApprovalAnswer, ApprovalState, RuleAsk};
use forge_review::{ApproveOutcome, ApproverNames, ReviewRequest, RuleOutcome, StoredApprovalRule};
use serde::{Deserialize, Serialize};

use super::loaders::{found, ids, lookup_ids_acting, project_of};
use super::{repository, respond, review, review_request};
use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader};
use crate::routes::projects::PREFIX;
use crate::routes::write::{absent, bounded, json};

/// The path after `{iid}` of [`APPROVE`].
pub const APPROVE_SUFFIX: &str = "/approve";
/// `POST` approve a merge request's head.
pub const APPROVE: &str = "/api/v1/projects/{id}/merge-requests/{iid}/approve";
/// The path after `{iid}` of [`UNAPPROVE`].
pub const UNAPPROVE_SUFFIX: &str = "/unapprove";
/// `POST` withdraw one's approval.
pub const UNAPPROVE: &str = "/api/v1/projects/{id}/merge-requests/{iid}/unapprove";
/// The path after `{iid}` of [`APPROVALS`].
pub const APPROVALS_SUFFIX: &str = "/approvals";
/// `GET` the approval state.
pub const APPROVALS: &str = "/api/v1/projects/{id}/merge-requests/{iid}/approvals";
/// `GET` a project's approval rules.
pub const RULES: &str = "/api/v1/projects/{id}/approval-rules";
/// `PUT` / `DELETE` one approval rule.
pub const RULE: &str = "/api/v1/projects/{id}/approval-rules/{name}";
/// The body cap of the approval routes.
pub const MAX_BODY: usize = 16 * 1024;

/// `POST …/approve`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproveMergeRequest {
    /// The head commit the approver reviewed (required): another head is
    /// refused (FG-REVIEW-0008 stale_view).
    pub sha: String,
}

/// One approval.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalBody {
    #[schema(minimum = 1)]
    pub approval_id: i64,
    #[schema(minimum = 1)]
    pub approver_id: i64,
    /// The head and target branch (without `refs/heads/`) it was given for.
    pub head_sha: String,
    pub target_branch: String,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// What an approve or unapprove did.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAnswerBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// `approved`, `withdrawn` or `unchanged`.
    pub outcome: String,
    /// Approve: whether the approval counts now (never read it as final:
    /// what may merge is `blocked_by` of `GET …/approvals`).
    pub counted: bool,
    /// Why it does not count (`committer`, `committers_unknown`,
    /// `not_in_rule`, …); `null` when it counts or for an unapprove.
    #[schema(required = true)]
    pub uncounted_reason: Option<String>,
    #[schema(required = true)]
    pub approval: Option<ApprovalBody>,
}

/// One live approval as evaluated now.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalStatusBody {
    #[schema(minimum = 1)]
    pub approval_id: i64,
    #[schema(minimum = 1)]
    pub approver_id: i64,
    pub username: String,
    pub counted: bool,
    #[schema(required = true)]
    pub uncounted_reason: Option<String>,
    /// `committer`: the audit row of a push by the approver that carried a
    /// commit of the merge request (runbook merge-blocked).
    #[schema(required = true)]
    pub committer_push_audit_id: Option<i64>,
}

/// One approval rule's state on this merge request.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RuleStatusBody {
    pub name: String,
    /// The rule's target pattern matches the merge request's target.
    pub applies: bool,
    pub required: u32,
    pub counted: u32,
}

/// One CODEOWNERS rule the changed paths need.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeownerStatusBody {
    pub line: u32,
    pub pattern: String,
    pub owners: Vec<String>,
    pub paths: usize,
    pub satisfied: bool,
}

/// One reason the merge is blocked on the approval side.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BlockBody {
    /// `approvals_missing`, `codeowner_approval_missing`,
    /// `codeowners_invalid`, `config_change_needs_maintainer_approval` or
    /// `changed_paths_unknown`.
    pub reason: String,
    #[schema(required = true)]
    pub detail: Option<String>,
}

/// `GET …/approvals`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalStateBody {
    #[schema(minimum = 1)]
    pub iid: i64,
    /// The merge request's head (`null` before its first version: nothing
    /// to approve, nothing evaluated).
    #[schema(required = true)]
    pub head_sha: Option<String>,
    pub target_branch: String,
    pub approvals: Vec<ApprovalStatusBody>,
    pub rules: Vec<RuleStatusBody>,
    pub codeowners: Vec<CodeownerStatusBody>,
    /// A configuration path changed: whether a Maintainer's counted
    /// approval is there (`null`: none changed).
    #[schema(required = true)]
    pub config_change_approved: Option<bool>,
    pub blocked_by: Vec<BlockBody>,
    /// The trial merge of the head into the target's tip conflicts now:
    /// the merge request cannot merge whatever the approvals say (the
    /// conflict itself is the mergeability's and the merge's to report).
    pub conflict: bool,
}

/// `PUT …/approval-rules/{name}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutApprovalRule {
    /// A protected-ref pattern (`refs/heads/main`, `refs/heads/release/*`);
    /// absent: every target.
    pub target_pattern: Option<String>,
    /// 1..=20.
    pub required_approvals: i32,
    /// Usernames who may approve for the rule.
    #[serde(default)]
    pub users: Vec<String>,
    /// Group full paths whose direct members may approve for the rule
    /// (no users and no groups: any eligible person).
    #[serde(default)]
    pub groups: Vec<String>,
    /// The rule's `updated_at` the caller read, echoed verbatim (an opaque
    /// string), or `null` to create a rule no rule has the name of.
    /// Required (U2 follow-up): another rule under the lock — changed,
    /// removed, or created by someone else — is FG-REVIEW-0018 and nothing
    /// is written.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schema(required = true)]
    pub expected_updated_at: Option<String>,
}

/// An approver user of a rule, named.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproverUserBody {
    #[schema(minimum = 1)]
    pub id: i64,
    /// What a rule write names the user by.
    pub username: String,
    pub display_name: String,
}

/// An approver group of a rule, named.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproverGroupBody {
    #[schema(minimum = 1)]
    pub id: i64,
    /// What a rule write names the group by.
    pub full_path: String,
    pub display_name: String,
}

/// One approval rule.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRuleBody {
    #[schema(minimum = 1)]
    pub approval_rule_id: i64,
    pub name: String,
    #[schema(required = true)]
    pub target_pattern: Option<String>,
    pub required_approvals: i32,
    pub user_ids: Vec<i64>,
    pub group_ids: Vec<i64>,
    /// The same approvers with their names (U2 follow-up), in the order of
    /// `user_ids` / `group_ids`.
    pub users: Vec<ApproverUserBody>,
    pub groups: Vec<ApproverGroupBody>,
    /// UTC, RFC 3339 to the microsecond: an opaque token — send it back
    /// verbatim as `expected_updated_at`.
    pub updated_at: String,
}

/// `GET …/approval-rules`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRulesBody {
    pub rules: Vec<ApprovalRuleBody>,
}

fn rule_body(r: &StoredApprovalRule, names: &ApproverNames) -> ApprovalRuleBody {
    ApprovalRuleBody {
        approval_rule_id: r.id.get(),
        name: r.name.clone(), // clone: the body owns its values
        target_pattern: r.target_pattern.as_ref().map(|p| p.as_str().to_owned()),
        required_approvals: r.required_approvals,
        user_ids: r.users.iter().map(|u| u.get()).collect(),
        group_ids: r.groups.iter().map(|g| g.get()).collect(),
        users: r
            .users
            .iter()
            .filter_map(|id| names.users.get(id))
            .map(|u| ApproverUserBody {
                id: u.id.get(),
                username: u.username.clone(), // clone: as above
                display_name: u.display_name.clone(),
            })
            .collect(),
        groups: r
            .groups
            .iter()
            .filter_map(|id| names.groups.get(id))
            .map(|g| ApproverGroupBody {
                id: g.id.get(),
                full_path: g.full_path.clone(), // clone: as above
                display_name: g.display_name.clone(),
            })
            .collect(),
        updated_at: r.updated_at.clone(),
    }
}

/// The approvers' names of a rule answer (review round): a name is never a
/// reason the answer fails. A failed read, or an approver without a name (a
/// group whose namespace row is missing), leaves those approvers out of
/// `users` / `groups` (their ids stay in `user_ids` / `group_ids`, so the
/// screen sees the gap and offers no save) and logs one warn
/// `forge.web.approver_names_incomplete` with the error code and the
/// request (runbook merge-blocked).
async fn approver_names(
    state: &AppState,
    ctx: &crate::Ctx,
    proof: &forge_auth::Authorized<ProjectCtx>,
    rules: &[StoredApprovalRule],
) -> ApproverNames {
    let read = async {
        bounded(
            state,
            "approver_names",
            review(state)?.approver_names(proof, rules),
        )
        .await
    }
    .await;
    let (names, failure) = match read {
        Ok(names) => (names, None),
        Err(e) => (ApproverNames::default(), Some(e)),
    };
    let missing_users = rules
        .iter()
        .flat_map(|r| r.users.iter())
        .filter(|id| !names.users.contains_key(id))
        .count();
    let missing_groups = rules
        .iter()
        .flat_map(|r| r.groups.iter())
        .filter(|id| !names.groups.contains_key(id))
        .count();
    if failure.is_some() || missing_users > 0 || missing_groups > 0 {
        let e = failure.unwrap_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "approver_name_missing")
        });
        tracing::warn!(
            target: "forge_web",
            event = "forge.web.approver_names_incomplete",
            error_code = e.code().id(),
            request_id = %ctx.request_id,
            project_id = proof.id().get(),
            missing_users,
            missing_groups,
            error = %e,
            "approvers without a name are left out of users / groups (runbook merge-blocked)"
        );
    }
    names
}

fn answer_body(iid: i64, a: &ApprovalAnswer) -> ApprovalAnswerBody {
    let (outcome, counted, reason) = match a.outcome {
        ApproveOutcome::Approved {
            counted,
            uncounted_reason,
        } => ("approved", counted, uncounted_reason),
        ApproveOutcome::Withdrawn => ("withdrawn", false, None),
        ApproveOutcome::Unchanged => ("unchanged", false, None),
    };
    ApprovalAnswerBody {
        iid,
        outcome: outcome.to_owned(),
        counted,
        uncounted_reason: reason.map(|r| r.as_str().to_owned()),
        approval: a.approval.as_ref().map(|s| ApprovalBody {
            approval_id: s.id.get(),
            approver_id: s.approver.get(),
            head_sha: s.head_sha.as_str().to_owned(),
            target_branch: s
                .target_branch
                .branch()
                .unwrap_or(s.target_branch.as_str())
                .to_owned(),
            created_at: s.created_at.clone(), // clone: the row is borrowed
        }),
    }
}

fn state_body(iid: i64, s: &ApprovalState) -> ApprovalStateBody {
    let target = s
        .target_branch
        .branch()
        .unwrap_or(s.target_branch.as_str())
        .to_owned();
    let Some(report) = &s.report else {
        return ApprovalStateBody {
            iid,
            head_sha: None,
            target_branch: target,
            approvals: Vec::new(),
            rules: Vec::new(),
            codeowners: Vec::new(),
            config_change_approved: None,
            blocked_by: Vec::new(),
            conflict: s.conflict,
        };
    };
    ApprovalStateBody {
        iid,
        head_sha: s.head_sha.as_ref().map(|h| h.as_str().to_owned()),
        target_branch: target,
        approvals: report
            .approvals
            .iter()
            .map(|a| ApprovalStatusBody {
                approval_id: a.approval.get(),
                approver_id: a.approver.get(),
                username: a.username.clone(), // clone: the report is borrowed
                counted: a.counted,
                uncounted_reason: a.uncounted_reason.map(|r| r.as_str().to_owned()),
                committer_push_audit_id: a.committer_push,
            })
            .collect(),
        rules: report
            .rules
            .iter()
            .map(|r| RuleStatusBody {
                name: r.name.clone(), // clone: as above
                applies: r.applies,
                required: r.required,
                counted: r.counted,
            })
            .collect(),
        codeowners: report
            .codeowners
            .iter()
            .map(|c| CodeownerStatusBody {
                line: c.line,
                pattern: c.pattern.clone(), // clone: as above
                owners: c.owners.clone(),   // clone: as above
                paths: c.paths,
                satisfied: c.satisfied,
            })
            .collect(),
        config_change_approved: report.config_change,
        blocked_by: report
            .blocks
            .iter()
            .map(|b| BlockBody {
                reason: b.as_str().to_owned(),
                detail: b.detail(),
            })
            .collect(),
        conflict: s.conflict,
    }
}

/// The merge request of an approval route (`suffix`: the path after
/// `{iid}`).
pub struct StoredMr(pub(super) &'static str);

/// `POST …/approve`.
pub const TO_APPROVE: StoredMr = StoredMr(APPROVE_SUFFIX);
/// `POST …/unapprove`.
pub const TO_UNAPPROVE: StoredMr = StoredMr(UNAPPROVE_SUFFIX);
/// `GET …/approvals`.
pub const APPROVALS_OF: StoredMr = StoredMr(APPROVALS_SUFFIX);

impl Loader<AppState> for StoredMr {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let ids = ids(parts.uri.path(), self.0)?;
            lookup_ids_acting(state, parts, ids, viewer, MergeRequestEdit::default(), None).await
        })
    }
}

impl BodyLoader<AppState, ApproveMergeRequest> for StoredMr {
    type Ctx = MergeRequestCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a ApproveMergeRequest,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, MergeRequestCtx> {
        Box::pin(async move {
            let ids = ids(parts.uri.path(), self.0)?;
            // M3 S2: an act-as approve's assertion is compared with the
            // body's sha too (`sha_mismatch`).
            lookup_ids_acting(
                state,
                parts,
                ids,
                viewer,
                MergeRequestEdit::default(),
                Some(body.sha.as_str()),
            )
            .await
        })
    }
}

fn proof_iid(proof: &forge_auth::Authorized<MergeRequestCtx>) -> Result<i64, ForgeError> {
    proof.id().1.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid")
    })
}

/// `POST …/approve` → `200` what the approval did.
pub async fn approve(
    state: AppState,
    request: Dispatched<MergeRequestCtx, ApproveMergeRequest>,
) -> Response {
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let iid = proof_iid(&w.proof)?;
        let sha =
            ObjectId::parse(&w.input.sha).map_err(|_| param_invalid("sha", "not_an_object_id"))?;
        let answer = review(&state)?
            .approve(&review_request(&w), &w.proof, repo?, &sha)
            .await?;
        json(answer_body(iid, &answer))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/unapprove` → `200` what the withdrawal did.
pub async fn unapprove(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    let repo = repository(&request).ok();
    let w = request.into_write();
    let built = async {
        let iid = proof_iid(&w.proof)?;
        let answer = review(&state)?
            .unapprove(&review_request(&w), &w.proof, repo)
            .await?;
        json(answer_body(iid, &answer))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET …/approvals` → `200` the approval state now.
pub async fn approvals(state: AppState, request: Dispatched<MergeRequestCtx>) -> Response {
    // Owned before the first await: the request (its body) is not `Sync`.
    let repo = repository(&request);
    let w = request.into_write();
    let built = async {
        let iid = proof_iid(&w.proof)?;
        let answer = review(&state)?
            .approval_state(&w.ctx.request_id, &w.proof, repo?)
            .await?;
        json(state_body(iid, &answer))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// The project id of `/api/v1/projects/{id}/approval-rules[/{name}]` and the
/// name (raw path).
fn rule_ids(raw_path: &str) -> Result<(ProjectId, Option<String>), ForgeError> {
    let (id, rest) = raw_path
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once("/approval-rules"))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(id).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let name = match rest {
        "" => None,
        other => {
            let name = other
                .strip_prefix('/')
                .filter(|n| !n.is_empty() && !n.contains('/'))
                .ok_or_else(|| param_invalid("name", "format"))?;
            Some(name.to_owned())
        }
    };
    Ok((ProjectId::new(id), name))
}

async fn project_lookup(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
) -> Result<crate::registry::Lookup<ProjectCtx>, ForgeError> {
    let (id, _) = rule_ids(parts.uri.path())?;
    Ok(match project_of(state, id, viewer).await? {
        Some((project, record)) => found(ProjectCtx::new(project), record),
        None => absent(),
    })
}

/// `GET …/approval-rules` and `DELETE …/approval-rules/{name}`: the project.
pub struct RulesProject;

impl Loader<AppState> for RulesProject {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(project_lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, PutApprovalRule> for RulesProject {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _body: &'a PutApprovalRule,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(project_lookup(state, parts, viewer))
    }
}

/// `GET …/approval-rules` → `200` the rules, by name.
pub async fn rules(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    // Owned before the first await: the request (its body) is not `Sync`.
    let w = request.into_write();
    let built = async {
        let rules = bounded(
            &state,
            "approval_rules",
            review(&state)?.approval_rules(&w.proof),
        )
        .await?;
        let names = approver_names(&state, &w.ctx, &w.proof, &rules).await;
        json(ApprovalRulesBody {
            rules: rules.iter().map(|r| rule_body(r, &names)).collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

fn rule_request<B>(w: &crate::registry::WriteParts<ProjectCtx, B>) -> ReviewRequest {
    ReviewRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

/// `PUT …/approval-rules/{name}` → `201` created or `200` replaced.
pub async fn put_rule(
    state: AppState,
    request: Dispatched<ProjectCtx, PutApprovalRule>,
) -> Response {
    let parts_path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let (_, name) = rule_ids(&parts_path)?;
        let name = name.ok_or_else(|| param_invalid("name", "format"))?;
        let ask = RuleAsk {
            name,
            target_pattern: w.input.target_pattern.clone(), // clone: the ask owns its values
            required_approvals: w.input.required_approvals,
            users: w.input.users.clone(),   // clone: as above
            groups: w.input.groups.clone(), // clone: as above
        };
        let (outcome, rule) = review(&state)?
            .put_approval_rule(
                &rule_request(&w),
                &w.proof,
                &ask,
                w.input.expected_updated_at.as_deref(),
            )
            .await?;
        let names = approver_names(&state, &w.ctx, &w.proof, std::slice::from_ref(&rule)).await;
        Ok((outcome, json(rule_body(&rule, &names))?))
    }
    .await;
    match built {
        Ok((RuleOutcome::Created, body)) => respond(&w.ctx, StatusCode::CREATED, Ok(body)),
        Ok((RuleOutcome::Replaced | RuleOutcome::Unchanged | RuleOutcome::Removed, body)) => {
            respond(&w.ctx, StatusCode::OK, Ok(body))
        }
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `DELETE …/approval-rules/{name}` → `204`.
pub async fn delete_rule(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let parts_path = request.raw_path().to_owned();
    let w = request.into_write();
    let done = async {
        let (_, name) = rule_ids(&parts_path)?;
        let name = name.ok_or_else(|| param_invalid("name", "format"))?;
        review(&state)?
            .remove_approval_rule(&rule_request(&w), &w.proof, &name)
            .await
    }
    .await;
    match done {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
