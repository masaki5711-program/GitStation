//! Acting for a person (M3 S2, ADR-0025 §5): the confirmation and the
//! claim, and the act-as facts the read and approve routes' loaders add.
//!
//! - `GET /api/v1/projects/{id}/merge-requests/{iid}/act-as-confirmation`
//!   `?intent=&sha=&integration=` (`ActAsConfirmView`): what the
//!   confirmation screen (U3) shows — forge's data only: the merge
//!   request, its head (all digits) and the target's tip, whether `sha`
//!   is the head now, the intent and the receiving integration. A
//!   person's web session only.
//! - `POST /-/confirm/projects/{id}/merge-requests/{iid}/assertions`
//!   `{intent, sha, integration, state}` (`ActAsAssertionIssue`, CSRF):
//!   the person pressed it; a `pending` assertion is written (no secret
//!   yet). 30 per person in 10 minutes (`FG-WEB-0010`).
//! - `POST /api/v1/integrations/{id}/act-as/claim` `{state}`
//!   (`ActAsAssertionClaim`): the integration's own service account claims
//!   it; the assertion's secret is created now and answered once
//!   (`Cache-Control: no-store`). 120 per integration a minute.
//!
//! The answer of a claim and the request's `state` are never logged.

use std::time::{Duration, Instant};

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{
    ActAsConfirmCtx, ActAsSubject, ActAsTipGate, IntegrationFeedCtx, LoadedConfirmIntegration,
    MergeRequestCtx, Principal, ProjectCtx,
};
use forge_core::act_as::Intent;
use forge_core::{parse_positive_id, IntegrationId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::namespaces::CallerLimiter;
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` the confirmation's facts.
pub const CONFIRMATION: &str = "/api/v1/projects/{id}/merge-requests/{iid}/act-as-confirmation";
/// The path after `{iid}` of [`CONFIRMATION`].
pub const CONFIRMATION_SUFFIX: &str = "/act-as-confirmation";
/// `POST` the person's confirmation.
pub const ISSUE: &str = "/-/confirm/projects/{id}/merge-requests/{iid}/assertions";
/// The prefix of [`ISSUE`] before `{id}`.
pub const ISSUE_PREFIX: &str = "/-/confirm/projects/";
/// `POST` the integration's claim.
pub const CLAIM: &str = "/api/v1/integrations/{id}/act-as/claim";
/// The bodies of the two writes are small.
pub const MAX_BODY: usize = 4 * 1024;

pub const CONFIRMATION_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "intent",
        description: "approve or unapprove",
        integer_range: None,
    },
    QueryParam {
        name: "sha",
        description: "the head the integration showed (all digits)",
        integer_range: None,
    },
    QueryParam {
        name: "integration",
        description: "the id of the integration that receives the assertion",
        integer_range: Some((1, i64::MAX)),
    },
];

/// Confirmations per person (ADR-0025 §5.3).
pub fn issue_limiter() -> CallerLimiter {
    CallerLimiter::new("act_as_issue", 30, Duration::from_secs(600))
}

/// Claims per integration's service account (ADR-0025 §5.3).
pub fn claim_limiter() -> CallerLimiter {
    CallerLimiter::new("act_as_claim", 120, Duration::from_secs(60))
}

fn act_as(state: &AppState) -> Result<&forge_integ::ActAs, ForgeError> {
    state.act_as.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Forbidden).with_context("reason", "act_as_not_configured")
    })
}

fn review(state: &AppState) -> Result<&forge_review::Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

fn principal(parts: &Parts) -> Option<&Principal> {
    parts.extensions.get::<Principal>()
}

/// A project as an act-as request reaches it: the service account's facts
/// added for an act-as principal (the plain context otherwise).
pub(crate) async fn project_ctx(
    state: &AppState,
    parts: &Parts,
    ctx: ProjectCtx,
    project: ProjectId,
) -> Result<ProjectCtx, ForgeError> {
    let Some(p) = principal(parts).filter(|p| p.act_as_credential().is_some()) else {
        return Ok(ctx);
    };
    let via = bounded(state, "act_as_via", act_as(state)?.via(p, project)).await?;
    Ok(match via {
        Some(via) => ctx.acting(via),
        None => ctx,
    })
}

/// A stored merge request as an act-as request reaches it: the service
/// account's facts and what an assertion is compared with (its head, its
/// target and the target's tip now, the body's `sha`).
pub(crate) async fn merge_request_ctx(
    state: &AppState,
    parts: &Parts,
    ctx: MergeRequestCtx,
    stored: &forge_review::StoredMergeRequest,
    record: &forge_core::ProjectRecord,
    requested_sha: Option<&str>,
) -> Result<MergeRequestCtx, ForgeError> {
    let Some(p) = principal(parts).filter(|p| p.act_as_credential().is_some()) else {
        return Ok(ctx);
    };
    let Some(person) = p.user() else {
        return Ok(ctx);
    };
    let project = stored.project_id;
    let Some(via) = bounded(state, "act_as_via", act_as(state)?.via(p, project)).await? else {
        return Ok(ctx);
    };
    let subject = |target_sha: Option<String>| ActAsSubject {
        project,
        merge_request: stored.id,
        head_sha: stored.head_sha.clone(), // clone: the subject owns its values
        target_ref: stored.target_branch.clone(),
        target_sha,
        requested_sha: requested_sha.map(str::to_owned),
    };
    // Review (authz CRITICAL): git is reached only after everything but the
    // tip was decided — never for a read, an unbound or invisible project,
    // or a person without the role.
    let untipped = ctx.clone().acting(via, Some(subject(None))); // clone: the gate's trial context
    let target_sha = match untipped.act_as_tip_gate(p) {
        ActAsTipGate::Skip(assumed) => assumed,
        ActAsTipGate::Fetch => bounded(
            state,
            "act_as_target_tip",
            review(state)?.target_tip(project, record.repository, person, stored),
        )
        .await?
        .map(|t| t.as_str().to_owned()),
    };
    Ok(ctx.acting(via, Some(subject(target_sha))))
}

/// `{id}`, `{iid}` of a confirmation path (`prefix` before `{id}`,
/// `suffix` after `{iid}`).
fn mr_ids(raw: &str, prefix: &str, suffix: &str) -> Result<(ProjectId, i64), ForgeError> {
    let (id, rest) = raw
        .strip_prefix(prefix)
        .and_then(|r| r.split_once("/merge-requests/"))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(id).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let iid = rest
        .strip_suffix(suffix)
        .filter(|i| !i.contains('/'))
        .and_then(parse_positive_id)
        .ok_or_else(|| param_invalid("iid", "not_positive_decimal"))?;
    Ok((ProjectId::new(id), iid))
}

fn intent_param(raw: &str) -> Result<Intent, ForgeError> {
    Intent::parse(raw).ok_or_else(|| param_invalid("intent", "unknown_intent"))
}

fn sha_param(raw: &str) -> Result<forge_core::ObjectId, ForgeError> {
    forge_core::ObjectId::parse(raw).map_err(|_| param_invalid("sha", "not_an_object_id"))
}

fn integration_param(raw: i64) -> Result<IntegrationId, ForgeError> {
    (raw > 0)
        .then(|| IntegrationId::new(raw))
        .ok_or_else(|| param_invalid("integration", "not_positive_decimal"))
}

/// The confirmation's context: the merge request for the person, the
/// intent, the integration (absent, or the merge request absent: the
/// same 404).
async fn confirm_lookup(
    state: &AppState,
    (project, iid): (ProjectId, i64),
    intent: Intent,
    integration: IntegrationId,
    viewer: Option<UserId>,
) -> Result<Lookup<ActAsConfirmCtx>, ForgeError> {
    let Some(record) = crate::routes::projects::load_bounded(state, project, viewer).await? else {
        return Ok(absent());
    };
    if record.id != project {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "loaded_another_project")
            .with_context("project_id", project.get()));
    }
    let Some(stored) = bounded(
        state,
        "merge_request_load",
        review(state)?.merge_request(project, iid),
    )
    .await?
    else {
        return Ok(absent());
    };
    let found: Option<LoadedConfirmIntegration> = bounded(
        state,
        "act_as_confirm_integration",
        act_as(state)?.confirm_integration(integration, project),
    )
    .await?;
    let Some(found) = found else {
        return Ok(absent());
    };
    let ctx = ActAsConfirmCtx::new(
        crate::routes::projects::loaded(&record),
        forge_review::loaded_merge_request(&stored),
        intent,
        found,
    );
    Ok(Lookup::Found {
        ctx,
        canonical: None,
        tombstone: None,
        project: Some(record),
        group: None,
    })
}

/// The query of [`CONFIRMATION`], strictly.
struct ConfirmQuery {
    intent: Intent,
    sha: forge_core::ObjectId,
    integration: IntegrationId,
}

fn confirm_query(query: Option<&str>) -> Result<ConfirmQuery, ForgeError> {
    let (mut intent, mut sha, mut integration) = (None, None, None);
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (slot, param) = match key {
            "intent" => (&mut intent, "intent"),
            "sha" => (&mut sha, "sha"),
            "integration" => (&mut integration, "integration"),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if slot.replace(value).is_some() {
            return Err(param_invalid(param, "repeated"));
        }
    }
    let missing = |p| param_invalid(p, "missing");
    Ok(ConfirmQuery {
        intent: intent_param(intent.ok_or_else(|| missing("intent"))?)?,
        sha: sha_param(sha.ok_or_else(|| missing("sha"))?)?,
        integration: integration_param(
            integration
                .and_then(parse_positive_id)
                .ok_or_else(|| param_invalid("integration", "not_positive_decimal"))?,
        )?,
    })
}

/// `GET …/act-as-confirmation`'s loader.
pub struct ConfirmationOf;

impl Loader<AppState> for ConfirmationOf {
    type Ctx = ActAsConfirmCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ActAsConfirmCtx> {
        Box::pin(async move {
            let ids = mr_ids(parts.uri.path(), "/api/v1/projects/", CONFIRMATION_SUFFIX)?;
            let q = confirm_query(parts.uri.query())?;
            confirm_lookup(state, ids, q.intent, q.integration, viewer).await
        })
    }
}

/// `GET …/act-as-confirmation` (forge's data only: the screen shows no
/// text the integration sent).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfirmationBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    #[schema(minimum = 1)]
    pub iid: i64,
    pub title: String,
    /// Without `refs/heads/`.
    pub source_branch: String,
    pub target_branch: String,
    /// The merge request's head now, all digits (`null`: no version yet).
    #[schema(required = true)]
    pub head_sha: Option<String>,
    /// The target's tip now (`null`: the branch is missing).
    #[schema(required = true)]
    pub target_sha: Option<String>,
    /// `sha` is the head now (otherwise the screen offers no button).
    pub sha_matches: bool,
    /// `approve` or `unapprove`.
    pub intent: String,
    #[schema(minimum = 1)]
    pub integration_id: i64,
    /// The integration's kind (`kanflow`): who receives the assertion.
    pub integration_kind: String,
    /// The username of the integration's service account (its name in
    /// forge).
    pub integration_name: String,
    /// "<display name> (<username>) として承認します".
    pub person_display_name: String,
    pub person_username: String,
}

fn branch_name(r: &forge_core::RefName) -> String {
    r.branch().unwrap_or(r.as_str()).to_owned()
}

/// `GET …/act-as-confirmation` → `200` the confirmation's facts.
pub async fn confirmation(state: AppState, request: Dispatched<ActAsConfirmCtx>) -> Response {
    let query = request.raw_query().map(str::to_owned);
    let record = request.project().cloned();
    let caller = request.caller().cloned();
    let w = request.into_write();
    let built = async {
        let q = confirm_query(query.as_deref())?;
        let (project, iid, integration) = *w.proof.id();
        let iid = iid.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid")
        })?;
        let record = record.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "confirm_without_project")
        })?;
        let person = w.proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_user")
        })?;
        let review = review(&state)?;
        let stored = bounded(
            &state,
            "merge_request_load",
            review.merge_request(project, iid),
        )
        .await?
        .ok_or_else(|| ForgeError::new(ErrorCode::NotFound).with_context("reason", "absent"))?;
        let tip = bounded(
            &state,
            "act_as_target_tip",
            review.target_tip(project, record.repository, person, &stored),
        )
        .await?;
        let (kind, name) = bounded(
            &state,
            "act_as_integration_label",
            act_as(&state)?.integration_label(integration),
        )
        .await?
        .ok_or_else(|| ForgeError::new(ErrorCode::NotFound).with_context("reason", "absent"))?;
        json(ConfirmationBody {
            project_id: project.get(),
            iid,
            title: stored.title.clone(), // clone: the body owns it
            source_branch: branch_name(&stored.source_branch),
            target_branch: branch_name(&stored.target_branch),
            sha_matches: stored.head_sha.as_deref() == Some(q.sha.as_str()),
            head_sha: stored.head_sha.clone(),
            target_sha: tip.map(|t| t.as_str().to_owned()),
            intent: q.intent.as_str().to_owned(),
            integration_id: integration.get(),
            integration_kind: kind,
            integration_name: name,
            person_display_name: caller
                .as_ref()
                .map(|c| c.display_name.clone())
                .unwrap_or_default(),
            person_username: caller.map(|c| c.username).unwrap_or_default(),
        })
    }
    .await;
    match built {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `POST /-/confirm/…/assertions`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueAssertion {
    /// `approve` or `unapprove`.
    pub intent: String,
    /// The head the screen showed (another is FG-REVIEW-0008 stale_view).
    pub sha: String,
    #[schema(minimum = 1)]
    pub integration: i64,
    /// The integration's state from the screen's URL (22..=128 of
    /// `[A-Za-z0-9_-]`; only its SHA-256 is kept).
    pub state: String,
}

/// `POST /-/confirm/…/assertions` → `201`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssuedBody {
    /// Always `issued`: the screen says "完了しました。この画面を閉じてください".
    pub status: String,
}

/// The confirmation's loader (with its body).
pub struct ToIssue;

impl BodyLoader<AppState, IssueAssertion> for ToIssue {
    type Ctx = ActAsConfirmCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        body: &'a IssueAssertion,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ActAsConfirmCtx> {
        Box::pin(async move {
            let ids = mr_ids(parts.uri.path(), ISSUE_PREFIX, "/assertions")?;
            let intent = intent_param(&body.intent)?;
            sha_param(&body.sha)?;
            let integration = integration_param(body.integration)?;
            confirm_lookup(state, ids, intent, integration, viewer).await
        })
    }
}

/// `POST /-/confirm/…/assertions` → `201` a pending assertion.
pub async fn issue(
    state: AppState,
    request: Dispatched<ActAsConfirmCtx, IssueAssertion>,
) -> Response {
    let record = request.project().cloned();
    let session = request.session().map(|s| s.session_id);
    let w = request.into_write();
    let built = async {
        let person = w.proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_user")
        })?;
        state
            .act_as_issue_limiter
            .check(person, Instant::now())
            .map_err(|retry| state.act_as_issue_limiter.refused(retry))?;
        let (project, iid, _) = *w.proof.id();
        let iid = iid.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_iid")
        })?;
        let record = record.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "confirm_without_project")
        })?;
        let sha = sha_param(&w.input.sha)?;
        let review = review(&state)?;
        let stored = bounded(
            &state,
            "merge_request_load",
            review.merge_request(project, iid),
        )
        .await?
        .ok_or_else(|| ForgeError::new(ErrorCode::NotFound).with_context("reason", "absent"))?;
        let tip = bounded(
            &state,
            "act_as_target_tip",
            review.target_tip(project, record.repository, person, &stored),
        )
        .await?
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::HeadChanged)
                .with_context("reason", "stale_view")
                .with_context("detail", "target_missing")
        })?;
        act_as(&state)?
            .issue(
                &w.proof,
                &w.ctx.request_id,
                session,
                stored.id,
                sha.as_str(),
                tip.as_str(),
                &w.input.state,
            )
            .await?;
        json(IssuedBody {
            status: "issued".to_owned(),
        })
    }
    .await;
    match built {
        Ok(body) => no_store(StatusCode::CREATED, body),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `POST …/act-as/claim`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimAssertion {
    /// The state the integration put into the confirmation's URL.
    pub state: String,
}

/// Who confirmed.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimSubBody {
    #[schema(minimum = 1)]
    pub user_id: i64,
    /// Only with the `user_employee_number` scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub employee_number: Option<String>,
}

/// `POST …/act-as/claim` → `200`, once.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimBody {
    /// `fga_<public_id>_<secret>`: send it as `Forge-Assertion` with
    /// `Forge-Act-As: user:<sub.user_id>`. Returned only by this answer.
    pub assertion: String,
    pub sub: ClaimSubBody,
    pub intent: String,
    #[schema(minimum = 1)]
    pub project_id: i64,
    #[schema(minimum = 1)]
    pub iid: i64,
    pub head_sha: String,
    /// `refs/heads/…`.
    pub target_ref: String,
    /// UTC, RFC 3339 (five minutes after the confirmation).
    pub expires_at: String,
}

/// The claim's loader: the integration of the path, for the caller.
pub struct ClaimOf;

impl BodyLoader<AppState, ClaimAssertion> for ClaimOf {
    type Ctx = IntegrationFeedCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _body: &'a ClaimAssertion,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, IntegrationFeedCtx> {
        Box::pin(async move {
            let id = crate::routes::write::id_in(
                parts.uri.path(),
                "/api/v1/integrations/",
                "/act-as/claim",
            )
            .map(IntegrationId::new)
            .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
            let loaded =
                bounded(state, "act_as_claim_load", act_as(state)?.claim_target(id)).await?;
            Ok(match loaded {
                // An act-as request never reaches this loader (the claim's
                // action is never acted for).
                Some(loaded) => crate::routes::protected_refs::found(IntegrationFeedCtx::new(
                    loaded, viewer, false,
                )),
                None => absent(),
            })
        })
    }
}

/// `POST …/act-as/claim` → `200` the assertion, once.
pub async fn claim(
    state: AppState,
    request: Dispatched<IntegrationFeedCtx, ClaimAssertion>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let via = w.proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_user")
        })?;
        state
            .act_as_claim_limiter
            .check(via, Instant::now())
            .map_err(|retry| state.act_as_claim_limiter.refused(retry))?;
        let answer = act_as(&state)?
            .claim(&w.proof, &w.ctx.request_id, w.credential_id, &w.input.state)
            .await?;
        json(ClaimBody {
            assertion: answer.assertion.reveal(),
            sub: ClaimSubBody {
                user_id: answer.user.get(),
                employee_number: answer.employee_number,
            },
            intent: answer.intent.as_str().to_owned(),
            project_id: answer.project.get(),
            iid: answer.iid,
            head_sha: answer.head_sha,
            target_ref: answer.target_ref,
            expires_at: answer.expires_at,
        })
    }
    .await;
    match built {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
