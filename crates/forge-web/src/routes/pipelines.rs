//! A project's CI pipelines (M4 C2, plan 14 §4.2, `forge-pipelines`).
//!
//! - `GET /api/v1/projects/{id}/pipelines?ref=&before=&limit=`
//!   (`PipelineRead`): newest first, at most `limit` (default 20, 1..=100)
//!   with an id below `before`, optionally of one branch (`main` or
//!   `refs/heads/main`); `next_before` names the next page.
//! - `GET …/pipelines/{pipeline_id}` (`PipelineRead`): one pipeline with
//!   its jobs; `404 FG-REPO-0005` when the project has no such pipeline.
//! - `POST …/pipelines` (`PipelineCreate`): `{"ref": "main"}` → `201` the
//!   pipeline planned on the branch's tip (`pending`, `skipped` or
//!   `invalid`); `404 FG-REPO-0023` for no such branch (`ref_not_found`) or
//!   no `.forge-ci.yml` at its tip (`ci_file_not_found`); `503 FG-CI-0002`
//!   when the definition could not be read (no row).
//! - `POST …/pipelines/{pipeline_id}/cancel` (`PipelineCancel`) and
//!   `…/retry` (`PipelineRetry`): `200` the pipeline after the change;
//!   `409 FG-CI-0003` for a state that cannot change.
//!
//! Metadata only (an agent audience reads it too): no logs or artifacts
//! exist before C5, which decides CI outputs on their marks (ADR-0029
//! decision 5). The secret names a job uses are listed, never a value.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::ProjectCtx;
use forge_core::{parse_positive_id, ProjectId, RefName, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_pipelines::{ManualRun, PipelineDetail, Pipelines, StoredJob, StoredPipeline};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` the list, `POST` a manual run.
pub const LIST: &str = "/api/v1/projects/{id}/pipelines";
/// `GET` one pipeline.
pub const ONE: &str = "/api/v1/projects/{id}/pipelines/{pipeline_id}";
/// `POST` cancel.
pub const CANCEL: &str = "/api/v1/projects/{id}/pipelines/{pipeline_id}/cancel";
/// `POST` retry.
pub const RETRY: &str = "/api/v1/projects/{id}/pipelines/{pipeline_id}/retry";
/// The body cap of a manual run (one branch name).
pub const MAX_BODY: usize = 1024;
/// The page size without `limit`.
const DEFAULT_LIMIT: i64 = 20;

/// The list's query parameters.
pub const LIST_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "ref",
        description: "Only the pipelines of this branch (`main` or `refs/heads/main`)",
        integer_range: None,
    },
    QueryParam {
        name: "before",
        description: "Only pipelines with an id below this one (the previous page's `next_before`)",
        integer_range: Some((1, i64::MAX)),
    },
    QueryParam {
        name: "limit",
        description: "At most this many pipelines (default 20)",
        integer_range: Some((1, forge_pipelines::MAX_PAGE)),
    },
];

/// `POST …/pipelines`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewPipelineRun {
    /// The branch: `main` or `refs/heads/main`.
    #[serde(rename = "ref")]
    pub git_ref: String,
}

/// One problem of an invalid definition (FG-CI-0001).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PipelineProblemBody {
    /// 1-based; 0 for the whole file.
    pub line: i64,
    /// The stable reason (crates/forge-ci/README.md).
    pub reason: String,
    #[schema(required = true)]
    pub subject: Option<String>,
}

/// One job of a pipeline.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PipelineJobBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub name: String,
    pub stage: String,
    /// Jobs of one wave may run at once.
    pub wave: i32,
    /// The jobs it waits for.
    pub needs: Vec<String>,
    pub image: String,
    pub timeout_s: i32,
    pub tags: Vec<String>,
    /// The secret names (never a value).
    pub secrets: Vec<String>,
    /// `created`, `pending`, `running`, `succeeded`, `failed`, `canceled`.
    pub status: String,
    pub attempt: i32,
    #[schema(required = true)]
    pub error_code: Option<String>,
    pub created_at: String,
    #[schema(required = true)]
    pub started_at: Option<String>,
    #[schema(required = true)]
    pub finished_at: Option<String>,
}

/// A pipeline (with its jobs on the one-pipeline routes).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PipelineBody {
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// `refs/heads/…`.
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
    /// `push`, `merge_request`, `schedule`, `manual`.
    pub source: String,
    #[schema(required = true, minimum = 1)]
    pub merge_request_id: Option<i64>,
    #[schema(required = true, minimum = 1)]
    pub triggered_by: Option<i64>,
    /// `pending`, `running`, `succeeded`, `failed`, `canceled`, `skipped`,
    /// `invalid` (FG-CI-0001), `error` (FG-CI-0002).
    pub status: String,
    #[schema(required = true)]
    pub error_code: Option<String>,
    /// Why `invalid` (at most 20).
    pub problems: Vec<PipelineProblemBody>,
    /// The confidential marks at `sha` when it was created: `open`,
    /// `confidential`, `undecided` (ADR-0029 decision 5).
    pub copy_mark: String,
    #[schema(required = true)]
    pub definition_hash: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[schema(required = true)]
    pub finished_at: Option<String>,
    /// The jobs (only on `GET …/{pipeline_id}` and the changes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jobs: Option<Vec<PipelineJobBody>>,
}

/// `GET …/pipelines`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PipelinePage {
    pub pipelines: Vec<PipelineBody>,
    /// Pass as `before` for the next page; `null` on the last.
    #[schema(required = true, minimum = 1)]
    pub next_before: Option<i64>,
}

fn job_body(j: StoredJob) -> PipelineJobBody {
    PipelineJobBody {
        id: j.id,
        name: j.name,
        stage: j.stage,
        wave: j.wave,
        needs: j.needs,
        image: j.image,
        timeout_s: j.timeout_s,
        tags: j.tags,
        secrets: j.secrets,
        status: j.status.as_str().to_owned(),
        attempt: j.attempt,
        error_code: j.error_code,
        created_at: j.created_at,
        started_at: j.started_at,
        finished_at: j.finished_at,
    }
}

fn body(p: StoredPipeline, jobs: Option<Vec<StoredJob>>) -> PipelineBody {
    PipelineBody {
        id: p.id,
        project_id: p.project.get(),
        git_ref: p.git_ref,
        sha: p.sha,
        source: p.source.as_str().to_owned(),
        merge_request_id: p.merge_request.map(|m| m.get()),
        triggered_by: p.triggered_by.map(UserId::get),
        status: p.status.as_str().to_owned(),
        error_code: p.error_code,
        problems: p
            .problems
            .into_iter()
            .map(|x| PipelineProblemBody {
                line: x.line,
                reason: x.reason,
                subject: x.subject,
            })
            .collect(),
        copy_mark: p.copy_mark.as_str().to_owned(),
        definition_hash: p.definition_hash,
        created_at: p.created_at,
        updated_at: p.updated_at,
        finished_at: p.finished_at,
        jobs: jobs.map(|jobs| jobs.into_iter().map(job_body).collect()),
    }
}

fn detail_body(d: PipelineDetail) -> PipelineBody {
    body(d.pipeline, Some(d.jobs))
}

fn pipelines(state: &AppState) -> Result<&Pipelines, ForgeError> {
    state.pipelines.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "pipelines_unconfigured")
    })
}

fn not_found() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound).with_context("resource", "pipeline")
}

/// What follows the project in the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tail {
    List,
    One(i64),
    Cancel(i64),
    Retry(i64),
}

impl Tail {
    fn kind(self) -> u8 {
        match self {
            Self::List => 0,
            Self::One(_) => 1,
            Self::Cancel(_) => 2,
            Self::Retry(_) => 3,
        }
    }

    fn pipeline(self) -> Option<i64> {
        match self {
            Self::List => None,
            Self::One(id) | Self::Cancel(id) | Self::Retry(id) => Some(id),
        }
    }
}

/// The path's project id and tail.
fn ids(raw_path: &str) -> Result<(ProjectId, Tail), ForgeError> {
    let bad = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(bad)?;
    let (project, tail) = rest.split_once("/pipelines").ok_or_else(bad)?;
    let project = parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)?;
    if tail.is_empty() {
        return Ok((project, Tail::List));
    }
    let bad_pipeline = || param_invalid("pipeline_id", "not_positive_decimal");
    let tail = tail.strip_prefix('/').ok_or_else(bad)?;
    let (id, action) = tail.split_once('/').unwrap_or((tail, ""));
    let id = parse_positive_id(id).ok_or_else(bad_pipeline)?;
    let tail = match action {
        "" => Tail::One(id),
        "cancel" => Tail::Cancel(id),
        "retry" => Tail::Retry(id),
        _ => return Err(bad()),
    };
    Ok((project, tail))
}

/// `…/pipelines[/{pipeline_id}[/cancel|/retry]]`: the project.
pub struct PipelinesOf {
    tail: u8,
}

/// `GET|POST …/pipelines`.
pub const OF_LIST: PipelinesOf = PipelinesOf { tail: 0 };
/// `GET …/pipelines/{pipeline_id}`.
pub const OF_ONE: PipelinesOf = PipelinesOf { tail: 1 };
/// `POST …/cancel`.
pub const OF_CANCEL: PipelinesOf = PipelinesOf { tail: 2 };
/// `POST …/retry`.
pub const OF_RETRY: PipelinesOf = PipelinesOf { tail: 3 };

impl PipelinesOf {
    async fn lookup(
        &self,
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<ProjectCtx>, ForgeError> {
        let (id, tail) = ids(parts.uri.path())?;
        if tail.kind() != self.tail {
            return Err(param_invalid("id", "not_positive_decimal"));
        }
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

impl Loader<AppState> for PipelinesOf {
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

impl BodyLoader<AppState, NewPipelineRun> for PipelinesOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a NewPipelineRun,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

/// A branch as the API takes it: `main` or `refs/heads/main`.
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

/// The list's query: `(ref, before, limit)`.
fn list_query(query: Option<&str>) -> Result<(Option<RefName>, Option<i64>, i64), ForgeError> {
    let (mut git_ref, mut before, mut limit) = (None, None, None);
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
        let value = crate::routes::namespaces::percent_decode(raw)
            .ok_or_else(|| param_invalid("query", "malformed"))?;
        let repeated = match key {
            "ref" => git_ref.replace(branch(&value, "ref")?).is_some(),
            "before" => before
                .replace(
                    parse_positive_id(&value)
                        .ok_or_else(|| param_invalid("before", "not_positive_decimal"))?,
                )
                .is_some(),
            "limit" => {
                let n = parse_positive_id(&value)
                    .filter(|n| *n <= forge_pipelines::MAX_PAGE)
                    .ok_or_else(|| param_invalid("limit", "out_of_range"))?;
                limit.replace(n).is_some()
            }
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if repeated {
            return Err(param_invalid(key_name(key), "repeated"));
        }
    }
    Ok((git_ref, before, limit.unwrap_or(DEFAULT_LIMIT)))
}

fn key_name(key: &str) -> &'static str {
    match key {
        "ref" => "ref",
        "before" => "before",
        _ => "limit",
    }
}

fn respond(
    ctx: &crate::Ctx,
    status: StatusCode,
    built: Result<serde_json::Value, ForgeError>,
) -> Response {
    match built {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET …/pipelines` → `200`.
pub async fn list(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let query = request.raw_query().map(str::to_owned);
    let w = request.into_write();
    let built = async {
        let (git_ref, before, limit) = list_query(query.as_deref())?;
        let rows = bounded(
            &state,
            "pipelines_list",
            pipelines(&state)?.list(&w.proof, git_ref.as_ref(), before, limit),
        )
        .await?;
        let full = i64::try_from(rows.len()).unwrap_or(i64::MAX) >= limit;
        let next_before = if full {
            rows.last().map(|p| p.id)
        } else {
            None
        };
        json(PipelinePage {
            pipelines: rows.into_iter().map(|p| body(p, None)).collect(),
            next_before,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// The path's pipeline id (the loader checked the shape).
fn pipeline_id(w_path: &str) -> Result<i64, ForgeError> {
    let (_, tail) = ids(w_path)?;
    tail.pipeline().ok_or_else(not_found)
}

/// `GET …/pipelines/{pipeline_id}` → `200`.
pub async fn one(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let id = pipeline_id(&path)?;
        let got = bounded(
            &state,
            "pipeline_read",
            pipelines(&state)?.get(&w.proof, id),
        )
        .await?;
        json(detail_body(got.ok_or_else(not_found)?))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/pipelines/{pipeline_id}/cancel` → `200`.
pub async fn cancel(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let id = pipeline_id(&path)?;
        let done = pipelines(&state)?
            .cancel(&w.proof, id, &w.ctx.request_id)
            .await?;
        json(detail_body(done.ok_or_else(not_found)?))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/pipelines/{pipeline_id}/retry` → `200`.
pub async fn retry(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let id = pipeline_id(&path)?;
        let done = pipelines(&state)?
            .retry(&w.proof, id, &w.ctx.request_id)
            .await?;
        json(detail_body(done.ok_or_else(not_found)?))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/pipelines` → `201`.
pub async fn run(state: AppState, request: Dispatched<ProjectCtx, NewPipelineRun>) -> Response {
    let w = request.into_write();
    let built = async {
        let git_ref = branch(&w.input.git_ref, "ref")?;
        match pipelines(&state)?
            .run_manual(&w.proof, &git_ref, &w.ctx.request_id)
            .await?
        {
            ManualRun::Recorded(detail) => json(detail_body(*detail)),
            ManualRun::NoDefinition => Err(ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                .with_context("reason", "ci_file_not_found")),
        }
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_paths_name_the_project_and_the_pipeline() {
        assert_eq!(
            ids("/api/v1/projects/7/pipelines").expect("list"),
            (ProjectId::new(7), Tail::List)
        );
        assert_eq!(
            ids("/api/v1/projects/7/pipelines/3").expect("one"),
            (ProjectId::new(7), Tail::One(3))
        );
        assert_eq!(
            ids("/api/v1/projects/7/pipelines/3/cancel").expect("cancel"),
            (ProjectId::new(7), Tail::Cancel(3))
        );
        assert_eq!(
            ids("/api/v1/projects/7/pipelines/3/retry").expect("retry"),
            (ProjectId::new(7), Tail::Retry(3))
        );
        for bad in [
            "/api/v1/projects/0/pipelines",
            "/api/v1/projects/7/pipelinesx",
            "/api/v1/projects/7/pipelines/0",
            "/api/v1/projects/7/pipelines/3/other",
            "/api/v1/projects/7/pipelines/x/cancel",
        ] {
            assert!(ids(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_list_query_takes_a_branch_a_cursor_and_a_limit() {
        let (r, b, l) = list_query(Some("ref=main&before=9&limit=5")).expect("query");
        assert_eq!(r.expect("ref").as_str(), "refs/heads/main");
        assert_eq!((b, l), (Some(9), 5));
        assert_eq!(list_query(None).expect("none").2, DEFAULT_LIMIT);
        for bad in [
            "ref=refs/tags/v1",
            "limit=0",
            "limit=101",
            "before=x",
            "other=1",
            "limit=1&limit=2",
        ] {
            assert!(list_query(Some(bad)).is_err(), "{bad}");
        }
    }
}
