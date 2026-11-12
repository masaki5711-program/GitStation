//! A target branch's merge queue (M4 C8, ADR-0030 §9, §11):
//!
//! - `GET /api/v1/projects/{id}/merge-queue?target=main` (`MergeQueueRead`:
//!   as the repository browse API, an agent audience too): whether the
//!   queue is paused, its next batch bound, the batch being tested, the
//!   live entries in order and the latest finished ones (at most 20), and
//!   `viewer` (a trial decision of pause / resume: a hint only).
//! - `POST …/merge-queue/pause` (`MergeQueuePause`: people only, Developer)
//!   and `…/resume` (`MergeQueueManage`: people only, Maintainer):
//!   `{"target": "main"}` → `200` the queue. Pausing stops new batches and
//!   every landing of the target (the emergency stop); entries may still be
//!   added.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{Action, ProjectCtx, ResourceCtx};
use forge_core::{parse_positive_id, ProjectId, RefName, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{QueueView, Review, ReviewRequest};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, QueryParam, WriteParts};
use crate::routes::merge_requests::{entry_body, QueueEntryBody};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` a target's queue.
pub const QUEUE: &str = "/api/v1/projects/{id}/merge-queue";
/// `POST` pause a target's queue.
pub const PAUSE: &str = "/api/v1/projects/{id}/merge-queue/pause";
/// `POST` resume it.
pub const RESUME: &str = "/api/v1/projects/{id}/merge-queue/resume";
/// The body cap (one branch name).
pub const MAX_BODY: usize = 1024;
/// The finished entries a read lists.
const RECENT: i64 = 20;

/// The read's query parameters.
pub const QUEUE_QUERY: &[QueryParam] = &[QueryParam {
    name: "target",
    description: "The target branch (`main` or `refs/heads/main`); required",
    integer_range: None,
}];

/// `POST …/merge-queue/pause` and `…/resume`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueTarget {
    /// The target branch: `main` or `refs/heads/main`.
    pub target: String,
}

/// The batch being tested (ADR-0030 §3, §5).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueBatchBody {
    #[schema(minimum = 1)]
    pub batch_id: i64,
    /// `testing` or `landing`.
    pub state: String,
    /// The target tip the train was built on.
    pub base_sha: String,
    /// The train's last commit (its pipeline's sha, source `merge_queue`).
    pub train_sha: String,
    #[schema(minimum = 1, maximum = 4)]
    pub size: i64,
    /// UTC, RFC 3339: tested past it, the batch is reaped (FG-CI-0019).
    pub deadline_at: String,
    pub request_id: String,
    pub created_at: String,
}

/// What the caller could do now (a hint; the routes decide again).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueViewerBody {
    pub can_pause: bool,
    pub can_resume: bool,
}

/// A target branch's merge queue.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MergeQueueBody {
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// The target branch without `refs/heads/`.
    pub target_branch: String,
    pub paused: bool,
    #[schema(required = true, minimum = 1)]
    pub paused_by: Option<i64>,
    #[schema(required = true)]
    pub paused_at: Option<String>,
    /// The bound of the next batch (4; halved after a red batch).
    #[schema(minimum = 1, maximum = 4)]
    pub batch_limit: i64,
    #[schema(required = true)]
    pub batch: Option<QueueBatchBody>,
    /// The live entries (`queued` / `testing`) in order.
    pub entries: Vec<QueueEntryBody>,
    /// The latest merged or removed entries, newest first (at most 20).
    pub recent: Vec<QueueEntryBody>,
    pub viewer: QueueViewerBody,
}

fn review(state: &AppState) -> Result<&Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

/// `main` or `refs/heads/main` as a branch ref (`param` names it).
fn branch(raw: &str, param: &'static str) -> Result<RefName, ForgeError> {
    let full = if raw.starts_with("refs/") {
        raw.to_owned()
    } else {
        format!("refs/heads/{raw}")
    };
    if !full.starts_with("refs/heads/") {
        return Err(param_invalid(param, "not_a_branch"));
    }
    RefName::parse(&full).map_err(|_| param_invalid(param, "invalid_ref"))
}

/// The read's `target` (required, once).
fn target_query(query: Option<&str>) -> Result<RefName, ForgeError> {
    let mut target = None;
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
        let value = crate::routes::namespaces::percent_decode(raw)
            .ok_or_else(|| param_invalid("query", "malformed"))?;
        match key {
            "target" => {
                if target.replace(branch(&value, "target")?).is_some() {
                    return Err(param_invalid("target", "repeated"));
                }
            }
            _ => return Err(param_invalid("query", "unknown_parameter")),
        }
    }
    target.ok_or_else(|| param_invalid("target", "missing"))
}

/// The path's project id (`…/merge-queue`, `…/pause`, `…/resume`).
fn project_id(raw_path: &str) -> Result<ProjectId, ForgeError> {
    let bad = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(bad)?;
    let (project, tail) = rest.split_once('/').ok_or_else(bad)?;
    if !matches!(
        tail,
        "merge-queue" | "merge-queue/pause" | "merge-queue/resume"
    ) {
        return Err(bad());
    }
    parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)
}

/// The project of a queue route.
pub struct QueueOf;

impl QueueOf {
    async fn lookup(
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<ProjectCtx>, ForgeError> {
        let id = project_id(parts.uri.path())?;
        let Some(record) = projects::load_bounded(state, id, viewer).await? else {
            return Ok(absent());
        };
        if record.id != id {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "loaded_another_project")
                .with_context("project_id", id.get())
                .with_context("loaded_project_id", record.id.get()));
        }
        Ok(Lookup::Found {
            ctx: ProjectCtx::new(projects::loaded(&record)),
            canonical: None,
            tombstone: None,
            project: Some(record),
            group: None,
        })
    }
}

impl Loader<AppState> for QueueOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(Self::lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, QueueTarget> for QueueOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a QueueTarget,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(Self::lookup(state, parts, viewer))
    }
}

fn request_of<B>(w: &WriteParts<ProjectCtx, B>) -> ReviewRequest {
    ReviewRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

fn viewer<B>(w: &WriteParts<ProjectCtx, B>) -> QueueViewerBody {
    let can = |action| {
        forge_auth::authorize(w.proof.principal(), action, &w.proof.ctx().resource()).is_ok()
    };
    QueueViewerBody {
        can_pause: can(Action::MergeQueuePause),
        can_resume: can(Action::MergeQueueManage),
    }
}

fn body(
    project: ProjectId,
    target: &RefName,
    view: QueueView,
    viewer: QueueViewerBody,
) -> MergeQueueBody {
    let short = target.branch().unwrap_or(target.as_str()).to_owned();
    let with_target = |mut e: QueueEntryBody| {
        e.target_branch.clone_from(&short);
        e
    };
    let (paused, paused_by, paused_at, batch_limit) = match &view.queue {
        Some(q) => (
            q.paused,
            q.paused_by.map(UserId::get),
            q.paused_at.clone(), // clone: the view is borrowed
            i64::try_from(q.batch_limit).unwrap_or(4),
        ),
        None => (false, None, None, 4),
    };
    MergeQueueBody {
        project_id: project.get(),
        target_branch: short.clone(), // clone: the entries carry it too
        paused,
        paused_by,
        paused_at,
        batch_limit,
        batch: view.batch.map(|b| QueueBatchBody {
            batch_id: b.id,
            state: b.state.as_str().to_owned(),
            base_sha: b.base_sha.as_str().to_owned(),
            train_sha: b.train_sha.as_str().to_owned(),
            size: i64::try_from(b.size).unwrap_or(0),
            deadline_at: b.deadline_at,
            request_id: b.request_id.as_str().to_owned(),
            created_at: b.created_at,
        }),
        entries: view
            .live
            .iter()
            .enumerate()
            .map(|(i, e)| with_target(entry_body(e, Some(i64::try_from(i).unwrap_or(0) + 1))))
            .collect(),
        recent: view
            .recent
            .iter()
            .map(|e| with_target(entry_body(e, None)))
            .collect(),
        viewer,
    }
}

fn respond(ctx: &crate::Ctx, built: Result<serde_json::Value, ForgeError>) -> Response {
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET …/merge-queue?target=` → `200`.
pub async fn read(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let query = request.raw_query().map(str::to_owned);
    let w = request.into_write();
    let built = async {
        let target = target_query(query.as_deref())?;
        let project = *w.proof.id();
        let view = bounded(
            &state,
            "merge_queue_read",
            review(&state)?.merge_queue(project, &target, RECENT),
        )
        .await?;
        json(body(project, &target, view, viewer(&w)))
    }
    .await;
    respond(&w.ctx, built)
}

async fn set_paused(
    state: AppState,
    request: Dispatched<ProjectCtx, QueueTarget>,
    paused: bool,
) -> Response {
    let w = request.into_write();
    let built = async {
        let target = branch(&w.input.target, "target")?;
        let review = review(&state)?;
        review
            .set_queue_paused(&request_of(&w), &w.proof, &target, paused)
            .await?;
        let project = *w.proof.id();
        let view = review.merge_queue(project, &target, RECENT).await?;
        json(body(project, &target, view, viewer(&w)))
    }
    .await;
    respond(&w.ctx, built)
}

/// `POST …/merge-queue/pause` → `200`.
pub async fn pause(state: AppState, request: Dispatched<ProjectCtx, QueueTarget>) -> Response {
    set_paused(state, request, true).await
}

/// `POST …/merge-queue/resume` → `200`.
pub async fn resume(state: AppState, request: Dispatched<ProjectCtx, QueueTarget>) -> Response {
    set_paused(state, request, false).await
}
