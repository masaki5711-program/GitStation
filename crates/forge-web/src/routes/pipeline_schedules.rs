//! A project's pipeline schedules (M4 C12, plan 14 §5 C12; ADR-0029
//! Amendment 2026-11-13).
//!
//! - `GET /api/v1/projects/{id}/pipeline-schedules` (`PipelineRead`): every
//!   schedule (at most 20), with its owner, next time and last run.
//! - `POST …/pipeline-schedules` (`PipelineScheduleManage`: Maintainer,
//!   people only): `{"description", "ref", "cron", "utc_offset_minutes",
//!   "active"?}` → `201`; the caller owns it. `400 FG-WEB-0006` with
//!   `param` and `reason` for a bad value; `409 FG-CI-0027` past 20.
//! - `PUT …/pipeline-schedules/{schedule_id}`: the same body → `200`; the
//!   caller becomes its owner. `404 FG-REPO-0005` without it.
//! - `DELETE …/pipeline-schedules/{schedule_id}` → `204`.
//!
//! A schedule runs with its owner's rights as they are at each run, and
//! never with secrets (its pipelines are never protected).

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::ProjectCtx;
use forge_core::{parse_positive_id, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_pipelines::schedules::{ScheduleDraft, StoredSchedule};
use forge_pipelines::{ChangeRequest, Pipelines};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` the list, `POST` a new one.
pub const LIST: &str = "/api/v1/projects/{id}/pipeline-schedules";
/// `PUT` / `DELETE` one.
pub const ONE: &str = "/api/v1/projects/{id}/pipeline-schedules/{schedule_id}";
/// The body cap.
pub const MAX_BODY: usize = 4 * 1024;

const TAIL: &str = "pipeline-schedules";

/// `POST …/pipeline-schedules`, `PUT …/{schedule_id}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PipelineScheduleInput {
    /// What it is for (1..=255 bytes, one line).
    pub description: String,
    /// The branch: `main` or `refs/heads/main`.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Five-field cron (`minute hour day-of-month month day-of-week`), at
    /// most once every 5 minutes.
    pub cron: String,
    /// The fixed UTC offset the rule is read at, in minutes (540 = JST,
    /// 420 = WIB); a quarter hour within −720..=840.
    pub utc_offset_minutes: i32,
    /// Default `true`; `false` never runs.
    #[serde(default)]
    pub active: Option<bool>,
}

/// A schedule.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PipelineScheduleBody {
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(minimum = 1)]
    pub project_id: i64,
    pub description: String,
    /// `refs/heads/…`.
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub cron: String,
    pub utc_offset_minutes: i32,
    pub active: bool,
    /// The person whose rights it runs with (whoever last saved it).
    #[schema(minimum = 1)]
    pub owner_id: i64,
    /// When it runs next (RFC 3339, UTC); `null` when inactive.
    #[schema(required = true)]
    pub next_run_at: Option<String>,
    /// The due time of its last run.
    #[schema(required = true)]
    pub last_run_at: Option<String>,
    /// What its last run did: `created` (`last_pipeline_id`),
    /// `no_definition`, `ref_missing`, `refused` (FG-CI-0026: the owner may
    /// not run pipelines now), `failed` (`last_error_code`).
    #[schema(required = true)]
    pub last_outcome: Option<String>,
    #[schema(required = true, minimum = 1)]
    pub last_pipeline_id: Option<i64>,
    #[schema(required = true)]
    pub last_error_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// `GET …/pipeline-schedules`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PipelineScheduleList {
    pub schedules: Vec<PipelineScheduleBody>,
}

fn schedule_body(s: StoredSchedule) -> PipelineScheduleBody {
    PipelineScheduleBody {
        id: s.id,
        project_id: s.project.get(),
        description: s.description,
        git_ref: s.git_ref,
        cron: s.cron,
        utc_offset_minutes: s.utc_offset_minutes,
        active: s.active,
        owner_id: s.owner.get(),
        next_run_at: s.next_run_at,
        last_run_at: s.last_run_at,
        last_outcome: s.last_outcome.map(|o| o.as_str().to_owned()),
        last_pipeline_id: s.last_pipeline_id,
        last_error_code: s.last_error_code,
        created_at: s.created_at,
        updated_at: s.updated_at,
    }
}

fn draft_of(input: &PipelineScheduleInput) -> ScheduleDraft {
    ScheduleDraft {
        description: input.description.clone(), // clone: the draft owns its text
        branch: input.git_ref.clone(),
        cron: input.cron.clone(),
        utc_offset_minutes: input.utc_offset_minutes,
        active: input.active.unwrap_or(true),
    }
}

fn pipelines(state: &AppState) -> Result<&Pipelines, ForgeError> {
    state.pipelines.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "pipelines_unconfigured")
    })
}

/// The path's project and, for one schedule, its id.
fn path_of(raw_path: &str) -> Result<(ProjectId, Option<i64>), ForgeError> {
    let bad = || param_invalid("id", "not_positive_decimal");
    let rest = raw_path.strip_prefix(PREFIX).ok_or_else(bad)?;
    let (project, tail) = rest.split_once('/').ok_or_else(bad)?;
    let project = parse_positive_id(project)
        .map(ProjectId::new)
        .ok_or_else(bad)?;
    if tail == TAIL {
        return Ok((project, None));
    }
    let id = tail
        .strip_prefix(TAIL)
        .and_then(|t| t.strip_prefix('/'))
        .and_then(parse_positive_id)
        .ok_or_else(|| param_invalid("schedule_id", "not_positive_decimal"))?;
    Ok((project, Some(id)))
}

/// `…/pipeline-schedules[/{schedule_id}]`: the project.
pub struct SchedulesOf {
    one: bool,
}

/// `GET|POST …/pipeline-schedules`.
pub const OF_LIST: SchedulesOf = SchedulesOf { one: false };
/// `PUT|DELETE …/pipeline-schedules/{schedule_id}`.
pub const OF_ONE: SchedulesOf = SchedulesOf { one: true };

impl SchedulesOf {
    async fn lookup(
        &self,
        state: &AppState,
        parts: &Parts,
        viewer: Option<UserId>,
    ) -> Result<Lookup<ProjectCtx>, ForgeError> {
        let (id, schedule) = path_of(parts.uri.path())?;
        if schedule.is_some() != self.one {
            return Err(param_invalid("schedule_id", "not_positive_decimal"));
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

impl Loader<AppState> for SchedulesOf {
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

impl BodyLoader<AppState, PipelineScheduleInput> for SchedulesOf {
    type Ctx = ProjectCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a PipelineScheduleInput,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ProjectCtx> {
        Box::pin(self.lookup(state, parts, viewer))
    }
}

fn request_of<B>(w: &WriteParts<ProjectCtx, B>) -> ChangeRequest {
    ChangeRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

fn schedule_id(raw_path: &str) -> Result<i64, ForgeError> {
    path_of(raw_path)?
        .1
        .ok_or_else(|| param_invalid("schedule_id", "not_positive_decimal"))
}

/// `GET …/pipeline-schedules` → `200`.
pub async fn list(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let rows = bounded(
            &state,
            "pipeline_schedule_list",
            pipelines(&state)?.schedules(&w.proof),
        )
        .await?;
        json(PipelineScheduleList {
            schedules: rows.into_iter().map(schedule_body).collect(),
        })
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `POST …/pipeline-schedules` → `201`.
pub async fn create(
    state: AppState,
    request: Dispatched<ProjectCtx, PipelineScheduleInput>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let stored = pipelines(&state)?
            .create_schedule(&request_of(&w), &w.proof, &draft_of(&w.input))
            .await?;
        json(schedule_body(stored))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::CREATED, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `PUT …/pipeline-schedules/{schedule_id}` → `200`.
pub async fn update(
    state: AppState,
    request: Dispatched<ProjectCtx, PipelineScheduleInput>,
) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let id = schedule_id(&path)?;
        let stored = pipelines(&state)?
            .update_schedule(&request_of(&w), &w.proof, id, &draft_of(&w.input))
            .await?;
        json(schedule_body(stored))
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `DELETE …/pipeline-schedules/{schedule_id}` → `204`.
pub async fn delete(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let done = async {
        let id = schedule_id(&path)?;
        pipelines(&state)?
            .delete_schedule(&request_of(&w), &w.proof, id)
            .await
    }
    .await;
    match done {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_path_names_the_project_and_the_schedule() {
        assert_eq!(
            path_of("/api/v1/projects/7/pipeline-schedules").expect("list"),
            (ProjectId::new(7), None)
        );
        assert_eq!(
            path_of("/api/v1/projects/7/pipeline-schedules/3").expect("one"),
            (ProjectId::new(7), Some(3))
        );
        for bad in [
            "/api/v1/projects/0/pipeline-schedules",
            "/api/v1/projects/7/pipeline-schedulesX",
            "/api/v1/projects/7/pipeline-schedules/",
            "/api/v1/projects/7/pipeline-schedules/0",
            "/api/v1/projects/7/pipeline-schedules/3/x",
        ] {
            assert!(path_of(bad).is_err(), "{bad}");
        }
    }
}
