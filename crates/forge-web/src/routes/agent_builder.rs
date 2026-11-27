//! The agent builder (plan 16 §6 A7, ADR-0032 Amendment A7): a project's
//! Claude Code configuration (`.claude/**`, `CLAUDE.md`, `.mcp.json`,
//! `.gitstation/agent.lock.json`) built from published, signed parts.
//!
//! - `GET /api/v1/projects/{id}/agent-builder` (`AgentBuilderDryRun`: a
//!   Developer, an agent's token included): the project's newest builds.
//! - `POST …/agent-builder/recommend` (`AgentBuilderRecommend`, decided in
//!   the route like a copy of the default tip after the dispatcher's
//!   project-level `RepoRead`): the rules-only recommendation, with the
//!   local LLM's reasons when the worker has them for the default branch's
//!   tip (plan 16 A11: `source: "rules+llm"`). The reasons only add words
//!   and an order to the rules' parts and suggest other catalog parts
//!   (`suggested`, never selected): the selection is the rules' one. No
//!   reasons now (the LLM is off, down, paused, timed out, or the tip has
//!   no open specification document) is the rules-only result with
//!   `degraded: true` and `llm.state` — never an error, never made-up
//!   reasons (`FG-AGENT-0011` in `llm.error_code` for a failure). The read
//!   is recorded so the worker answers it (`forge.agent_recommend_asks`).
//! - `POST …/agent-builder/preview` `{selection}` (`AgentBuilderDryRun`):
//!   the Dry-Run — what the build adds, changes and removes, each part's
//!   capabilities, the lock. Nothing is written but the build's record.
//! - `POST …/agent-builder/compile` `{selection, title?}`
//!   (`AgentBuilderCompile`: a Developer, a **person** — an agent never
//!   rewrites its own guard rails): the same plan written as ONE commit by
//!   the builder bot (`forge-agent-builder`) on a new branch
//!   `agent-builder/<12 hex of the lock digest>` from the default branch's
//!   tip, the ref update decided like a push of the caller's
//!   (`authorize_ref_updates`, the protected-ref rules), the secret rules
//!   on every written file, the push recorded for the caller, then the
//!   merge request into the default branch opened for the caller
//!   (`MergeRequestCreate`). A build whose branch exists is `FG-GIT-0027
//!   reason=branch_exists` (the same build was asked for already); a build
//!   that changes nothing is `FG-GIT-0031`.
//!
//! - `GET …/agent-builder/updates` (`AgentBuilderDryRun`, plan 16 A9):
//!   whether the configuration at the default branch's tip has an update —
//!   each locked part's version and the one the update picks (the newest
//!   stable version that is not revoked; a revoked one without a
//!   replacement is dropped), and the update's Dry-Run.
//! - `POST …/agent-builder/update` `{adapter?}` (`AgentBuilderCompile`, a
//!   person): queue the update merge request (`202`, the job
//!   `agent.update_merge_request` in `forge serve`); `adapter` moves to
//!   another base model (the generation migration). Nothing to update is
//!   `409 FG-AGENT-0015`. A revocation queues the same job for every
//!   project whose lock uses the revoked version (forge-marketplace).
//!
//! The merge request is never merged by itself without a person:
//! `forge/agent-lock` verifies it and `forge/risk` names it
//! `agent_config_locked`, and auto-merge waits for a Maintainer who is not
//! its author (`agent_config_approval`, ADR-0017 Amendment A7).

use std::sync::Arc;

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{
    authorize_ref_updates, prove, Action, LoadedMergeRequest, MergeRequestCtx, MergeRequestState,
    ProjectCtx, RefUpdate, RefUpdateCtx, ResourceCtx as _,
};
use forge_core::llm::agent_recommend::{findings_of, merge, ReasonsState, RulePick};
use forge_core::{
    telemetry, ObjectId, ProjectId, PushApplied, PushRecord, PushUpdateKind, PushedCommits,
    PushedRef, Pusher, RefName, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::commit::CommitIdentity;
use forge_git::file_commit::{FilePath, TreeChange, COMMIT_DEADLINE};
use forge_git::secret_scan::{self, SecretScan};
use forge_marketplace::{
    BuildOutcome, BuildPlan, Marketplace, Reasons, RecommendedAt, Selected, StoredBuild,
    BUILDER_EMAIL, BUILDER_NAME,
};
use forge_review::{NewFields, Review, ReviewRequest};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, WriteParts};
use crate::routes::browse::{load_project_lookup, Target};
use crate::routes::git::principal_key;
use crate::routes::git_push::{record_applied, user_agent, PushLog, PUSH_CHECK_TIMEOUT};
use crate::routes::merge_requests::{body_of, MergeRequestBody};
use crate::routes::projects::{self, PREFIX as PROJECT_PREFIX};
use crate::routes::write::{bounded, id_in, json, no_store};

/// `GET` the builds.
pub const STATUS: &str = "/api/v1/projects/{id}/agent-builder";
/// `POST` the recommendation.
pub const RECOMMEND: &str = "/api/v1/projects/{id}/agent-builder/recommend";
/// `POST` the Dry-Run.
pub const PREVIEW: &str = "/api/v1/projects/{id}/agent-builder/preview";
/// `POST` the build (the merge request).
pub const COMPILE: &str = "/api/v1/projects/{id}/agent-builder/compile";
/// `GET` the available update (plan 16 A9).
pub const UPDATES: &str = "/api/v1/projects/{id}/agent-builder/updates";
/// `POST` the update merge request (plan 16 A9).
pub const UPDATE: &str = "/api/v1/projects/{id}/agent-builder/update";

/// The body cap: a selection of at most 200 parts.
pub const MAX_BODY: usize = 64 * 1024;
/// Builds the status lists.
const STATUS_LIMIT: i64 = 20;

/// One selected part version.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectionItem {
    pub slug: String,
    /// The exact published version (`1.2.3`).
    pub version: String,
    /// `user` (default: picked on the screen) or `rule` (the
    /// recommendation's).
    #[serde(default)]
    pub selected_by: Option<String>,
    /// Why (the recommendation's reason; at most 200 characters).
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST …/preview` and `…/compile`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildRequest {
    /// The parts: exactly one adapter (the base model) and what it needs.
    pub selection: Vec<SelectionItem>,
    /// `…/compile` only: the merge request's title (1..=255 characters;
    /// default "Build the agent configuration").
    #[serde(default)]
    pub title: Option<String>,
}

/// `POST …/update` (plan 16 A9).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateRequest {
    /// The base model to move to (an adapter part's slug: the generation
    /// migration); absent: the lock's own.
    #[serde(default)]
    pub adapter: Option<String>,
}

/// One part of an update.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePartBody {
    pub slug: String,
    pub kind: String,
    /// The version the lock uses (null: a part the update adds).
    #[schema(required = true)]
    pub locked_version: Option<String>,
    /// The locked version is revoked.
    pub locked_revoked: bool,
    /// The version the update selects (null: the part is dropped).
    #[schema(required = true)]
    pub next_version: Option<String>,
}

/// `GET …/updates`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatesBody {
    /// An update would change the configuration.
    pub available: bool,
    /// Why not (`no_lock`, `up_to_date`); null when available.
    #[schema(required = true)]
    pub reason: Option<String>,
    /// Each locked part and what the update picks (empty when not available).
    pub parts: Vec<UpdatePartBody>,
    /// The update's Dry-Run (null when not available).
    #[schema(required = true)]
    pub preview: Option<BuildPreviewBody>,
    /// The caller may ask for the update merge request (`POST …/update`:
    /// `AgentBuilderCompile`, a person and a Developer). A hint for the
    /// screen's button; the route decides again.
    pub can_update: bool,
}

/// `POST …/update` (202).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateQueuedBody {
    /// The job was queued (false: see `reason`).
    pub queued: bool,
    #[schema(required = true, minimum = 1)]
    pub job_id: Option<i64>,
    /// Why nothing was queued: `already_open` (the same update's merge
    /// request is open: `merge_request_iid`) or `already_asked` (the same
    /// person asked the same update; its job is queued or done). Null when
    /// queued.
    #[schema(required = true)]
    pub reason: Option<String>,
    #[schema(required = true, minimum = 1)]
    pub merge_request_iid: Option<i64>,
    pub parts: Vec<UpdatePartBody>,
    /// The update as planned now (the job plans again when it runs).
    pub preview: BuildPreviewBody,
}

/// `POST …/recommend` (no field yet; the body is `{}`).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecommendRequest {}

/// One changed file of a build.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildChangeBody {
    pub path: String,
    /// `added`, `modified` or `removed`.
    pub change: String,
}

/// One part of a build.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildPartBody {
    pub slug: String,
    pub kind: String,
    pub version: String,
    /// `beta` or `stable`.
    pub channel: String,
    /// exec, network, permissions, fs-write (the screen's marks;
    /// `permissions` widens Claude Code's permissions).
    pub capabilities: Vec<String>,
}

/// The Dry-Run.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildPreviewBody {
    /// The branch the merge request would target (without `refs/heads/`).
    pub target_branch: String,
    /// Its tip the plan was made against.
    pub base_sha: String,
    /// `sha256:<hex>` of the lock.
    pub lock_digest: String,
    /// `.gitstation/agent.lock.json` as it would be written.
    pub lock: String,
    pub changes: Vec<BuildChangeBody>,
    pub parts: Vec<BuildPartBody>,
}

/// Why the merge request was not opened (the commit stands).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildMergeRequestError {
    pub error_code: String,
    /// The user message in the request's language.
    pub message: String,
}

/// The build as written.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildCompiledBody {
    pub preview: BuildPreviewBody,
    /// The new branch (without `refs/heads/`).
    pub branch: String,
    /// The builder bot's commit.
    pub commit_sha: String,
    /// The merge request opened (null when it could not be opened).
    pub merge_request: Option<MergeRequestBody>,
    pub merge_request_error: Option<BuildMergeRequestError>,
}

/// One recorded build.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildRecordBody {
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(minimum = 1)]
    pub requested_by: i64,
    /// `[{"slug","version","selected_by","reason"}]`.
    #[schema(value_type = Object)]
    pub selection: serde_json::Value,
    #[schema(required = true)]
    pub lock_digest: Option<String>,
    pub dry_run: bool,
    #[schema(required = true, minimum = 1)]
    pub merge_request_iid: Option<i64>,
    /// `requested`, `succeeded` or `failed`.
    pub state: String,
    #[schema(required = true)]
    pub error_code: Option<String>,
    pub request_id: String,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// `GET …/agent-builder`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildStatusBody {
    /// Newest first, at most 20.
    pub builds: Vec<BuildRecordBody>,
}

/// One recommended part.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecommendedBody {
    pub slug: String,
    pub kind: String,
    pub version: String,
    /// `rule`.
    pub selected_by: String,
    /// `base_model`, `current_lock` or `required_by=<slug>`.
    pub reason: String,
    /// The local LLM's reason (an AI proposal, at most 200 characters);
    /// `null` without one.
    pub llm_reason: Option<String>,
}

/// A catalog part the local LLM suggests: never selected (a person ticks
/// it and sees its capabilities in the Dry-Run).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestedBody {
    pub slug: String,
    pub kind: String,
    /// The newest stable version.
    pub version: String,
    /// What the part may do (`exec`, `network`, …): shown beside the AI's
    /// reason so the reason is never the only thing a person reads.
    pub capabilities: Vec<String>,
    /// `llm`.
    pub selected_by: String,
    /// The local LLM's reason (an AI proposal, at most 200 characters).
    pub reason: String,
}

/// The local LLM's part of the recommendation (plan 16 A11).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecommendLlmBody {
    /// `ready` (the reasons are merged in), `pending` (asked; look again
    /// shortly), `unavailable` (the LLM did not answer), `paused`,
    /// `refused` (nothing was sent: no open specification document, or one
    /// held a secret), `withheld` (a confidential advice for an agent).
    pub state: String,
    /// `FG-AGENT-0011` when the LLM did not answer.
    pub error_code: Option<String>,
    /// The recorded cause (an FG code, e.g. `FG-LLM-0001`), if any.
    pub cause: Option<String>,
    /// `ready`: the AI's summary of what the specifications need (a
    /// proposal; the person decides).
    pub summary: Option<String>,
    pub advice_id: Option<i64>,
    /// `name:tag` of the model.
    pub model: Option<String>,
    pub prompt_version: Option<i32>,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub created_at: Option<String>,
    /// The model's findings that named no catalog part (dropped).
    pub dropped: usize,
}

/// `POST …/recommend`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecommendBody {
    /// `rules+llm` (the local LLM's reasons are merged in) or `rules`.
    pub source: String,
    /// `true` when the answer has no LLM reasons (any `llm.state` but
    /// `ready`) or they are less trustworthy (the advice was degraded, or
    /// findings were dropped).
    pub degraded: bool,
    /// The rules' selection: the base model first, then the parts with an
    /// LLM reason (most confident first), then the rest.
    pub parts: Vec<RecommendedBody>,
    /// Other catalog parts the LLM suggests (never selected).
    pub suggested: Vec<SuggestedBody>,
    /// Parts of the current lock with no stable version to pick.
    pub unavailable: Vec<String>,
    pub llm: RecommendLlmBody,
}

/// Loads the project of `…/{id}/agent-builder[/…]`.
pub struct BuilderOf {
    suffix: &'static str,
}

/// `GET …/agent-builder`.
pub const STATUS_OF: BuilderOf = BuilderOf {
    suffix: "/agent-builder",
};
/// `POST …/recommend`.
pub const RECOMMEND_OF: BuilderOf = BuilderOf {
    suffix: "/agent-builder/recommend",
};
/// `POST …/preview`.
pub const PREVIEW_OF: BuilderOf = BuilderOf {
    suffix: "/agent-builder/preview",
};
/// `POST …/compile`.
pub const COMPILE_OF: BuilderOf = BuilderOf {
    suffix: "/agent-builder/compile",
};
/// `GET …/updates`.
pub const UPDATES_OF: BuilderOf = BuilderOf {
    suffix: "/agent-builder/updates",
};
/// `POST …/update`.
pub const UPDATE_OF: BuilderOf = BuilderOf {
    suffix: "/agent-builder/update",
};

impl BuilderOf {
    async fn lookup(
        &self,
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<crate::registry::Lookup<ProjectCtx>, ForgeError> {
        let Some(id) = id_in(parts.uri.path(), PROJECT_PREFIX, self.suffix).map(ProjectId::new)
        else {
            return Err(param_invalid("id", "not_positive_decimal"));
        };
        load_project_lookup(state, id, viewer).await
    }
}

impl Loader<AppState> for BuilderOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, BuildRequest> for BuilderOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a BuildRequest,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, UpdateRequest> for BuilderOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a UpdateRequest,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, RecommendRequest> for BuilderOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a RecommendRequest,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

fn marketplace(state: &AppState) -> Result<&Marketplace, ForgeError> {
    state.marketplace.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "marketplace_unconfigured")
    })
}

fn review(state: &AppState) -> Result<&Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

fn selection_of(input: &BuildRequest) -> Vec<Selected> {
    input
        .selection
        .iter()
        .map(|s| Selected {
            slug: s.slug.clone(),       // clone: the plan owns its selection
            version: s.version.clone(), // clone: as above
            selected_by: s.selected_by.clone().unwrap_or_else(|| "user".to_owned()), // clone: as above
            reason: s.reason.clone(), // clone: as above
        })
        .collect()
}

fn selection_json(selection: &[Selected]) -> serde_json::Value {
    serde_json::Value::Array(
        selection
            .iter()
            .map(|s| {
                serde_json::json!({
                    "slug": s.slug, "version": s.version,
                    "selected_by": s.selected_by, "reason": s.reason,
                })
            })
            .collect(),
    )
}

fn short(name: &RefName) -> String {
    name.branch().unwrap_or(name.as_str()).to_owned()
}

fn preview_body(plan: &BuildPlan) -> BuildPreviewBody {
    BuildPreviewBody {
        target_branch: short(&plan.target),
        base_sha: plan.base.as_str().to_owned(),
        lock_digest: plan.compiled.lock_digest.clone(), // clone: the answer owns it
        lock: String::from_utf8_lossy(&plan.compiled.lock).into_owned(),
        changes: plan
            .changes
            .iter()
            .map(|c| BuildChangeBody {
                path: c.path.clone(), // clone: as above
                change: c.change.to_owned(),
            })
            .collect(),
        parts: plan
            .parts
            .iter()
            .map(|p| BuildPartBody {
                slug: p.slug.clone(),                 // clone: as above
                kind: p.kind.clone(),                 // clone: as above
                version: p.version.clone(),           // clone: as above
                channel: p.channel.clone(),           // clone: as above
                capabilities: p.capabilities.clone(), // clone: as above
            })
            .collect(),
    }
}

fn record_body(b: StoredBuild) -> BuildRecordBody {
    BuildRecordBody {
        id: b.id,
        requested_by: b.requested_by,
        selection: b.selection,
        lock_digest: b.lock_digest,
        dry_run: b.dry_run,
        merge_request_iid: b.mr_iid,
        state: b.state,
        error_code: b.error_code,
        request_id: b.request_id,
        created_at: b.created_at,
    }
}

/// Keep the build's record; a failure to keep it is logged once (the
/// answer stands: the record is a history, not the build).
#[allow(clippy::too_many_arguments)] // one record's fields
async fn keep_record(
    state: &AppState,
    ctx: &crate::Ctx,
    project: ProjectId,
    user: UserId,
    plan: Option<&BuildPlan>,
    selection: &serde_json::Value,
    dry_run: bool,
    outcome: BuildOutcome<'_>,
) {
    let market = match marketplace(state) {
        Ok(m) => m,
        Err(e) => {
            telemetry::log_failure(
                &e.with_context("route", "agent_builder")
                    .with_context("phase", "record_build"),
                &ctx.request_id,
            );
            return;
        }
    };
    let kept = bounded(
        state,
        "agent_build_record",
        market.record_build(
            project,
            user,
            plan,
            selection,
            dry_run,
            &outcome,
            ctx.request_id.as_str(),
            "build",
        ),
    )
    .await;
    if let Err(e) = kept {
        telemetry::log_failure(
            &e.with_context("route", "agent_builder")
                .with_context("phase", "record_build")
                .with_context("project_id", project.get()),
            &ctx.request_id,
        );
    }
}

fn caller(principal: &forge_auth::Principal) -> Result<UserId, ForgeError> {
    principal.user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "builder_route_without_user")
    })
}

/// `GET …/agent-builder` → `200` the newest builds.
pub async fn status(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let project = *request.proof().id();
    let w = request.into_write();
    let built = async {
        let builds = bounded(
            &state,
            "agent_build_list",
            marketplace(&state)?.builds(project, STATUS_LIMIT),
        )
        .await?;
        json(BuildStatusBody {
            builds: builds.into_iter().map(record_body).collect(),
        })
    }
    .await;
    match built {
        Ok(v) => no_store(StatusCode::OK, v),
        Err(e) => w
            .ctx
            .fail(e.with_context("route", "agent_builder_status"))
            .into_response(),
    }
}

/// `POST …/agent-builder/recommend` → `200` (decided in the route, module
/// docs).
pub async fn recommend(
    state: AppState,
    request: Dispatched<ProjectCtx, RecommendRequest>,
) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    match recommend_inner(&state, request).await {
        Ok(v) => no_store(StatusCode::OK, v),
        Err(e) => ctx
            .fail(e.with_context("route", "agent_builder_recommend"))
            .into_response(),
    }
}

async fn recommend_inner(
    state: &AppState,
    request: Dispatched<ProjectCtx, RecommendRequest>,
) -> Result<serde_json::Value, ForgeError> {
    let target = Target::of(state, &request)?;
    let key = principal_key(&request)?;
    // Who reads the advice's words (plan 19 §5: a confidential advice is a
    // person's only), decided by forge-auth like every advice read.
    let declared_agent = crate::routes::context::audience(&request)?.is_agent();
    let reader = forge_auth::advice_reader(request.principal(), declared_agent);
    // Nothing of the request (not `Sync`) is held across an await.
    let w = request.into_write();
    crate::routes::repo_copy::decide_agent_recommend(
        &target.smart,
        &w.proof,
        target.repository.id,
        &key,
    )
    .await?;
    let market = marketplace(state)?;
    let found = bounded(
        state,
        "agent_recommend",
        market.recommend_at(target.repository.id),
    )
    .await?;
    let read = bounded(
        state,
        "agent_recommend_reasons",
        market.reasons(
            target.project,
            found.tip.as_ref(),
            w.ctx.request_id.as_str(),
            reader,
        ),
    )
    .await;
    let reasons = match read {
        Ok(r) => r,
        Err(e) => {
            // The rules' answer still stands: degraded, never an error.
            let cause = e.code().id().to_owned();
            telemetry::log_failure(
                &e.with_context("route", "agent_builder_recommend")
                    .with_context("step", "llm_reasons"),
                &w.ctx.request_id,
            );
            Reasons {
                state: ReasonsState::Unavailable,
                error_code: Some(ErrorCode::RecommendDegraded),
                cause: Some(cause),
                advice: None,
            }
        }
    };
    json(recommend_body(&found, reasons))
}

/// The answer of `…/recommend` (module docs): the rules' selection with the
/// reasons merged in when there are any.
pub(crate) fn recommend_body(found: &RecommendedAt, reasons: Reasons) -> RecommendBody {
    let picks: Vec<RulePick> = found
        .recommendation
        .parts
        .iter()
        .map(|p| RulePick {
            slug: p.slug.clone(),       // clone: the merge owns its picks
            kind: p.kind.clone(),       // clone: as above
            version: p.version.clone(), // clone: as above
            reason: p.reason.clone(),   // clone: as above
        })
        .collect();
    let ready = reasons.state == ReasonsState::Ready && reasons.advice.is_some();
    let (findings, unreadable) = match (&reasons.advice, ready) {
        (Some(a), true) => findings_of(&a.findings),
        _ => (Vec::new(), 0),
    };
    let merged = merge(&picks, &found.catalog, &findings);
    let dropped = merged.dropped + unreadable;
    let advice = if ready { reasons.advice } else { None };
    let advice_degraded = advice.as_ref().is_some_and(|a| a.degraded);
    RecommendBody {
        source: if ready { "rules+llm" } else { "rules" }.to_owned(),
        degraded: !ready || advice_degraded || dropped > 0,
        parts: merged
            .parts
            .into_iter()
            .map(|p| RecommendedBody {
                slug: p.pick.slug,
                kind: p.pick.kind,
                version: p.pick.version,
                selected_by: "rule".to_owned(),
                reason: p.pick.reason,
                llm_reason: p.llm_reason,
            })
            .collect(),
        suggested: merged
            .suggested
            .into_iter()
            .map(|s| SuggestedBody {
                slug: s.slug,
                kind: s.kind,
                version: s.version,
                capabilities: s.capabilities,
                selected_by: "llm".to_owned(),
                reason: s.reason,
            })
            .collect(),
        unavailable: found.recommendation.unavailable.clone(), // clone: the body owns it
        llm: RecommendLlmBody {
            state: reasons.state.as_str().to_owned(),
            error_code: reasons.error_code.map(|c| c.id().to_owned()),
            cause: reasons.cause,
            summary: advice.as_ref().map(|a| a.summary.clone()), // clone: the body owns it
            advice_id: advice.as_ref().map(|a| a.advice_id),
            model: advice.as_ref().map(|a| a.model.clone()), // clone: as above
            prompt_version: advice.as_ref().map(|a| a.prompt_version),
            created_at: advice.map(|a| a.created_at),
            dropped,
        },
    }
}

/// `POST …/agent-builder/preview` → `200` the Dry-Run.
pub async fn preview(state: AppState, request: Dispatched<ProjectCtx, BuildRequest>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    match preview_inner(&state, request).await {
        Ok(v) => no_store(StatusCode::OK, v),
        Err(e) => ctx
            .fail(e.with_context("route", "agent_builder_preview"))
            .into_response(),
    }
}

async fn preview_inner(
    state: &AppState,
    request: Dispatched<ProjectCtx, BuildRequest>,
) -> Result<serde_json::Value, ForgeError> {
    let target = Target::of(state, &request)?;
    let user = caller(request.principal())?;
    if request.input().title.is_some() {
        return Err(param_invalid("title", "compile_only"));
    }
    // Nothing of the request (not `Sync`) is held across an await.
    let w = request.into_write();
    let selection = selection_of(&w.input);
    let planned = marketplace(state)?
        .plan_build(target.project, target.repository.id, &selection)
        .await;
    match planned {
        // Review A7: a Dry-Run that worked changes nothing and is not
        // kept (an agent's token may preview at will); a failure is.
        Ok(plan) => json(preview_body(&plan)),
        Err(e) => {
            keep_record(
                state,
                &w.ctx,
                target.project,
                user,
                None,
                &selection_json(&selection),
                true,
                BuildOutcome::Failed {
                    error_code: e.code().id(),
                },
            )
            .await;
            Err(e.with_context("project_id", target.project.get()))
        }
    }
}

fn update_parts(plan: &forge_marketplace::UpdatePlan) -> Vec<UpdatePartBody> {
    plan.lines
        .iter()
        .map(|l| UpdatePartBody {
            slug: l.slug.clone(),             // clone: the answer owns it
            kind: l.kind.clone(),             // clone: as above
            locked_version: l.locked.clone(), // clone: as above
            locked_revoked: l.locked_revoked,
            next_version: l.next.clone(), // clone: as above
        })
        .collect()
}

/// The reason of an `FG-AGENT-0015` (`no_lock`, `up_to_date`).
fn not_needed_reason(e: &ForgeError) -> Option<String> {
    (e.code() == ErrorCode::AgentUpdateNotNeeded).then(|| {
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map_or_else(|| "up_to_date".to_owned(), |(_, v)| v.clone()) // clone: the answer owns it
    })
}

/// `GET …/agent-builder/updates` → `200` (plan 16 A9).
pub async fn updates(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    let target = Target::of(&state, &request);
    // A trial decision of the update's own action (plan 16 A12: the
    // screen shows the button only when the API says so).
    let can_update = forge_auth::authorize(
        request.principal(),
        Action::AgentBuilderCompile,
        &request.proof().ctx().resource(),
    )
    .is_ok();
    // Nothing of the request (not `Sync`) is held across an await.
    drop(request.into_write());
    let built = async {
        let target = target?;
        let planned = marketplace(&state)?
            .plan_update(target.project, target.repository.id, None)
            .await;
        match planned {
            Ok(plan) => json(UpdatesBody {
                available: true,
                reason: None,
                parts: update_parts(&plan),
                preview: Some(preview_body(&plan.build)),
                can_update,
            }),
            Err(e) => match not_needed_reason(&e) {
                Some(reason) => json(UpdatesBody {
                    available: false,
                    reason: Some(reason),
                    parts: Vec::new(),
                    preview: None,
                    can_update,
                }),
                None => Err(e.with_context("project_id", target.project.get())),
            },
        }
    }
    .await;
    match built {
        Ok(v) => no_store(StatusCode::OK, v),
        Err(e) => ctx
            .fail(e.with_context("route", "agent_builder_updates"))
            .into_response(),
    }
}

/// `POST …/agent-builder/update` → `202` the update queued (plan 16 A9).
pub async fn update(state: AppState, request: Dispatched<ProjectCtx, UpdateRequest>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    match update_inner(&state, request).await {
        Ok(v) => no_store(StatusCode::ACCEPTED, v),
        Err(e) => ctx
            .fail(e.with_context("route", "agent_builder_update"))
            .into_response(),
    }
}

async fn update_inner(
    state: &AppState,
    request: Dispatched<ProjectCtx, UpdateRequest>,
) -> Result<serde_json::Value, ForgeError> {
    let target = Target::of(state, &request)?;
    let user = caller(request.principal())?;
    let adapter = request.input().adapter.clone(); // clone: the plan and the job own it
    if let Some(a) = &adapter {
        let fine = !a.is_empty()
            && a.len() <= 64
            && a.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !fine {
            return Err(param_invalid("adapter", "slug"));
        }
    }
    // Nothing of the request (not `Sync`) is held across an await.
    let w = request.into_write();
    let market = marketplace(state)?;
    let plan = market
        .plan_update(target.project, target.repository.id, adapter.as_deref())
        .await
        .map_err(|e| e.with_context("project_id", target.project.get()))?;
    // Review A9: the same update's merge request decides first — open: say
    // so; closed by a person: never reopened (FG-AGENT-0016).
    let existing = bounded(
        state,
        "agent_update_existing",
        market.existing_update(target.project, &plan),
    )
    .await?;
    let (job, reason, iid) = match existing {
        Some((forge_core::MergeRequestState::Open, iid)) => {
            (None, Some("already_open".to_owned()), Some(iid))
        }
        Some((
            forge_core::MergeRequestState::Closed | forge_core::MergeRequestState::Merged,
            iid,
        )) => {
            return Err(ForgeError::new(ErrorCode::AgentUpdateNotOpened)
                .with_context("reason", "closed_by_person")
                .with_context("iid", iid)
                .with_context("project_id", target.project.get()))
        }
        None => {
            let job = bounded(
                state,
                "agent_update_queue",
                market.ask_update(
                    target.project,
                    user,
                    adapter.as_deref(),
                    &plan,
                    &w.ctx.request_id,
                ),
            )
            .await?;
            let reason = job.is_none().then(|| "already_asked".to_owned());
            (job, reason, None)
        }
    };
    tracing::info!(
        target: "forge_web::agent_builder",
        event = "forge.agent_builder.update_queued",
        request_id = %w.ctx.request_id,
        project_id = target.project.get(),
        lock_digest = %plan.build.compiled.lock_digest,
        queued = job.is_some(),
        job_id = job.unwrap_or(0),
    );
    json(UpdateQueuedBody {
        queued: job.is_some(),
        job_id: job,
        reason,
        merge_request_iid: iid,
        parts: update_parts(&plan),
        preview: preview_body(&plan.build),
    })
}

/// The secret rules on each file the build writes (a published part never
/// brings a credential into a repository unseen).
fn scan_writes(writes: &[(String, Vec<u8>)]) -> Result<(), ForgeError> {
    for (path, bytes) in writes {
        if let SecretScan::Found(rule) = secret_scan::scan(bytes) {
            return Err(ForgeError::new(ErrorCode::PushSecretDetected)
                .with_context("event", "forge.git.push_checked")
                .with_context("verdict", "refused")
                .with_context("findings", 1)
                .with_context("first_rule", rule)
                .with_context("first_path", path.clone())); // clone: the error owns it
        }
    }
    Ok(())
}

/// `POST …/agent-builder/compile` → `201` the commit and the merge request.
pub async fn compile(state: AppState, request: Dispatched<ProjectCtx, BuildRequest>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    match compile_inner(&state, request).await {
        Ok(body) => no_store(StatusCode::CREATED, body),
        Err(err) => ctx
            .fail(err.with_context("route", "agent_builder_compile"))
            .into_response(),
    }
}

async fn compile_inner(
    state: &AppState,
    request: Dispatched<ProjectCtx, BuildRequest>,
) -> Result<serde_json::Value, ForgeError> {
    let target = Target::of(state, &request)?;
    let smart = Arc::clone(&target.smart);
    let Some(push) = smart.push.clone() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "push_unconfigured")
        );
    };
    let user = caller(request.principal())?;
    let Some(project_row) = request.project().cloned() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_project")
        );
    };
    let title = match &request.input().title {
        Some(t) if t.trim().is_empty() || t.chars().count() > 255 => {
            return Err(param_invalid("title", "length"))
        }
        Some(t) if t.chars().any(char::is_control) => {
            return Err(param_invalid("title", "control"))
        }
        Some(t) => t.clone(), // clone: the merge request owns it
        None => "Build the agent configuration".to_owned(),
    };
    let peer = request.peer();
    let agent = user_agent(&request);
    let w = request.into_write();
    let ctx = w.ctx.clone(); // clone: the log and the answer keep it
    let repo = target.repository.id;
    let fail = |e: ForgeError| {
        e.with_context("project_id", target.project.get())
            .with_context("repo_id", repo.get())
    };
    let selection = selection_of(&w.input);
    let market = marketplace(state)?;
    let plan = match market.plan_build(target.project, repo, &selection).await {
        Ok(plan) => plan,
        Err(e) => {
            keep_record(
                state,
                &ctx,
                target.project,
                user,
                None,
                &selection_json(&selection),
                false,
                BuildOutcome::Failed {
                    error_code: e.code().id(),
                },
            )
            .await;
            return Err(fail(e));
        }
    };
    let written = write_build(state, &w, &target, &plan, push.clone(), user, peer, agent).await; // clone: a shared handle
    let (branch, commit) = match written {
        Ok(done) => done,
        Err(e) => {
            keep_record(
                state,
                &ctx,
                target.project,
                user,
                Some(&plan),
                &plan.selection,
                false,
                BuildOutcome::Failed {
                    error_code: e.code().id(),
                },
            )
            .await;
            return Err(fail(e));
        }
    };

    let review = review(state)?;
    let (merge_request, merge_request_error, mr_id) = match bounded(
        state,
        "agent_build_merge_request",
        open_merge_request(review, &w, &project_row, &branch, &plan.target, &title),
    )
    .await
    {
        Ok((body, id)) => (Some(body), None, Some(id)),
        Err(e) => {
            let code = e.code();
            telemetry::log_failure(
                &fail(e)
                    .with_context("route", "agent_builder_compile")
                    .with_context("phase", "open_merge_request")
                    .with_context("commit", commit.as_str().to_owned()),
                &ctx.request_id,
            );
            (
                None,
                Some(BuildMergeRequestError {
                    error_code: code.id().to_owned(),
                    message: code.user_message(ctx.lang).to_owned(),
                }),
                None,
            )
        }
    };
    // A commit without its merge request is not a finished build (asking
    // again resumes it).
    let failed_code = merge_request_error.as_ref().map(|e| e.error_code.clone()); // clone: the record owns it
    let outcome = match &failed_code {
        Some(code) => BuildOutcome::Failed { error_code: code },
        None => BuildOutcome::Succeeded {
            lock_digest: &plan.compiled.lock_digest,
            merge_request: mr_id,
        },
    };
    keep_record(
        state,
        &ctx,
        target.project,
        user,
        Some(&plan),
        &plan.selection,
        false,
        outcome,
    )
    .await;
    tracing::info!(
        target: "forge_web::agent_builder",
        event = "forge.agent_builder.compiled",
        request_id = %ctx.request_id,
        project_id = target.project.get(),
        repo_id = repo.get(),
        branch = %branch.as_str(),
        commit = %commit.as_str(),
        lock_digest = %plan.compiled.lock_digest,
        parts = plan.parts.len(),
        changes = plan.changes.len(),
        merge_request_opened = merge_request.is_some(),
    );
    json(BuildCompiledBody {
        preview: preview_body(&plan),
        branch: short(&branch),
        commit_sha: commit.as_str().to_owned(),
        merge_request,
        merge_request_error,
    })
}

/// The bot's commit on the new branch and the push's record (module docs).
#[allow(clippy::too_many_arguments)] // the request's parts, read before any await
async fn write_build(
    state: &AppState,
    w: &WriteParts<ProjectCtx, BuildRequest>,
    target: &Target,
    plan: &BuildPlan,
    push: crate::routes::git::PushServices,
    user: UserId,
    peer: Option<std::net::IpAddr>,
    agent: Option<String>,
) -> Result<(RefName, ObjectId), ForgeError> {
    let smart = &target.smart;
    let repo = target.repository.id;
    if plan.writes.is_empty() && plan.removes.is_empty() {
        return Err(ForgeError::new(ErrorCode::FileCommitUnchanged)
            .with_context("reason", "build_unchanged")
            .with_context("lock_digest", plan.compiled.lock_digest.clone())); // clone: the error owns it
    }
    scan_writes(&plan.writes)?;
    let branch = forge_marketplace::builder::builder_branch(&plan.compiled.lock_digest)?;
    // The ref update decided like a push of the caller's.
    let push_proof = prove(w.proof.principal(), Action::RepoPush, w.proof.ctx().clone())?; // clone: a proof owns its context
    let rules = bounded(
        state,
        "protection_load",
        push.protection.load_protection(target.project),
    )
    .await?;
    let decided = authorize_ref_updates(
        &push_proof,
        &[RefUpdateCtx::new(branch.clone(), RefUpdate::Create, &rules)], // clone: the context owns its name
    )?;
    if let Some(Err(denial)) = decided.into_iter().next() {
        return Err(denial);
    }
    // Review A7: a build whose request ended after the commit (the client
    // went away, the merge request could not be opened) is resumed — the
    // branch is reused when its tip holds exactly this build's lock, and
    // only the merge request is opened again. Any other branch of the name
    // is a conflict.
    if let Some(tip) = smart
        .git
        .read_ref(&smart.storage, repo, &branch, PUSH_CHECK_TIMEOUT)
        .await?
    {
        let at_tip = smart
            .browse
            .agent_config(
                repo,
                tip.as_str(),
                forge_marketplace::builder::MAX_TIP_FILES,
                forge_marketplace::builder::MAX_TIP_BYTES,
            )
            .await?;
        if at_tip.lock.as_deref() == Some(plan.compiled.lock.as_slice())
            && plan
                .compiled
                .files
                .iter()
                .all(|(p, b)| at_tip.files.get(p) == Some(b))
        {
            return Ok((branch, tip));
        }
        return Err(ForgeError::new(ErrorCode::FileCommitConflict)
            .with_context("reason", "branch_exists")
            .with_context("branch", branch.as_str().to_owned()));
    }
    let writes: Vec<(FilePath, &[u8])> = plan
        .writes
        .iter()
        .map(|(p, b)| FilePath::parse(p).map(|p| (p, b.as_slice())))
        .collect::<Result<_, _>>()?;
    let removes: Vec<FilePath> = plan
        .removes
        .iter()
        .map(|p| FilePath::parse(p))
        .collect::<Result<_, _>>()?;
    let bot = CommitIdentity::new(BUILDER_NAME, BUILDER_EMAIL, BUILDER_NAME)?;
    let message = format!(
        "chore(agent): build the agent configuration\n\nGenerated by the GitStation agent builder from {} signed part version(s).\nLock: {}\n",
        plan.parts.len(),
        plan.compiled.lock_digest
    );
    let written = tokio::time::timeout(
        COMMIT_DEADLINE,
        smart.git.commit_tree_change(
            &smart.storage,
            repo,
            &TreeChange {
                parent: &plan.base,
                branch: &branch,
                writes: &writes,
                removes: &removes,
                message: &message,
                author: &bot,
                committer: &bot,
            },
            PUSH_CHECK_TIMEOUT,
        ),
    )
    .await;
    smart.browse.invalidate(repo);
    let commit = match written {
        Ok(commit) => commit?,
        Err(_) => {
            return Err(ForgeError::new(ErrorCode::GitTimeout)
                .with_context("operation", "agent_build_commit")
                .with_context("timeout_ms", COMMIT_DEADLINE.as_millis()))
        }
    };

    // Recorded like a push of the caller's, in a task the request does not own.
    let log = PushLog {
        request_id: w.ctx.request_id.clone(), // clone: the record task owns its copy
        project_id: target.project,
        repo_id: repo.get(),
        lang: w.ctx.lang,
        route: "agent_builder_compile",
    };
    let record = PushRecord {
        repository_id: repo,
        updates: vec![PushedRef {
            name: branch.clone(), // clone: the record owns its name
            old: ObjectId::parse(&"0".repeat(commit.as_str().len()))?,
            new: commit.clone(), // clone: as above
            kind: PushUpdateKind::Create,
            commits: vec![commit.clone()], // clone: as above
            total_commits: 1,
            total_commits_partial: false,
        }],
    };
    let pusher = Pusher {
        request_id: w.ctx.request_id.clone(), // clone: the record keeps its own
        user,
        kind: w.proof.principal().account_kind(),
        credential_id: w.credential_id,
        source_ip: peer,
        user_agent: agent,
    };
    let line = PushApplied::new(&pusher, target.project, &record);
    if let Err(err) = line.log_as("final", "agent_builder_compile") {
        log.failure(err);
    }
    let commits = PushedCommits {
        commits: vec![commit.clone()], // clone: the record task owns it
        complete: true,
    };
    let recorder = Arc::clone(&push.recorder);
    let task_log = log.clone(); // clone: the task owns its copy
    let guard = smart.push_tasks.start();
    let recorded = tokio::spawn(async move {
        let _guard = guard;
        record_applied(&*recorder, &pusher, &record, &commits, &task_log).await;
    });
    if let Err(e) = recorded.await {
        log.failure(
            ForgeError::new(ErrorCode::PushRecordFailed)
                .with_context("reason", "record_task_failed")
                .with_context("detail", e.to_string()),
        );
    }
    Ok((branch, commit))
}

/// Open the build's merge request for the caller, decided on
/// `MergeRequestCreate`.
async fn open_merge_request(
    review: &Review,
    w: &WriteParts<ProjectCtx, BuildRequest>,
    record: &forge_core::ProjectRecord,
    branch: &RefName,
    target: &RefName,
    title: &str,
) -> Result<(MergeRequestBody, forge_core::MergeRequestId), ForgeError> {
    let user = caller(w.proof.principal())?;
    let proof = prove(
        w.proof.principal(),
        Action::MergeRequestCreate,
        MergeRequestCtx::new(
            projects::loaded(record),
            LoadedMergeRequest {
                iid: None,
                author: user,
                state: MergeRequestState::Open,
                draft: false,
                source_branch: branch.clone(), // clone: the context owns it
                target_branch: target.clone(), // clone: as above
            },
        ),
    )?;
    let opened = review
        .open(
            &ReviewRequest {
                request_id: w.ctx.request_id.clone(), // clone: the request keeps its id
                credential_id: w.credential_id,
            },
            &proof,
            record.repository,
            &NewFields {
                title: title.to_owned(),
                description: "Built by the GitStation agent builder from signed marketplace parts. `forge/agent-lock` verifies the configuration; a Maintainer approves it before it merges.".to_owned(),
            },
        )
        .await?;
    let id = opened.id;
    Ok((body_of(&opened), id))
}

#[cfg(test)]
#[path = "agent_builder_tests.rs"]
mod tests;
