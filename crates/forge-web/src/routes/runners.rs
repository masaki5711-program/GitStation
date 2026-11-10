//! CI runners (M4 C3, plan 14 §4.2, ADR-0029 §1).
//!
//! Administration (an administrator's web session, people only):
//! - `GET /api/v1/admin/runners` (`RunnerManage`): every runner (never a
//!   token).
//! - `POST /api/v1/admin/runners` (`RunnerRegister`): `{"name", "tags"?,
//!   "description"?}` → `201` the runner and its token `fgr_…`, shown once.
//! - `PATCH /api/v1/admin/runners/{runner_id}` (`RunnerManage`):
//!   `{"enabled"?, "tags"?, "description"?}` → `200`; `404 FG-REPO-0005`
//!   without such a runner. A disabled runner's token is refused at once.
//!
//! The runner API (a runner's token `fgr_…` only; `authorize()` refuses
//! everyone else, a job's token included):
//! - `POST /api/v1/runner/jobs/request` (`RunnerJobRequest`): `{"wait_s"?}`
//!   (0..=20, default 20) → `200 {"job": …}` — a job the runner's tags
//!   cover, claimed with `FOR UPDATE SKIP LOCKED`, with its job token
//!   `fgj_…` (shown once) — or `{"job": null}` when none came within the
//!   wait (ask again).
//! - `POST /api/v1/runner/jobs/{job_id}/heartbeat` (`RunnerJobHeartbeat`):
//!   `{"attempt"}` → `{"state": "running", "lease_until"}` or `{"state":
//!   "canceled"}` (stop the job, then complete it).
//! - `POST …/{job_id}/log` (`RunnerLogAppend`): `{"attempt", "seq",
//!   "text"}` (append only; a resent `seq` is a no-op) → `{"stored"}`;
//!   `413 FG-CI-0006` past the job's limit.
//! - `POST …/{job_id}/complete` (`RunnerJobComplete`): `{"attempt",
//!   "result": "succeeded"|"failed", "exit_code"?}` → `{"status",
//!   "pipeline_status"}`. A completion that ends the pipeline deletes its
//!   ref `refs/pipelines/<id>/head` (M4 C4 review; a failure is logged as
//!   `forge.ci.pipeline_ref_delete_failed` with its code and left to the
//!   maintenance sweep — the completion itself stands).
//!
//! The job routes are decided on the job's lease (`RunnerJobCtx`): another
//! runner's job or an ended one is `403 FG-AUTH-0003`; a lease that ran out
//! (or an attempt that is no longer the job's) is `409 FG-CI-0004` — the
//! runner stops the job.

use std::time::Duration;

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{InstanceCtx, RunnerJobCtx};
use forge_core::{parse_positive_id, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_pipelines::runners::{
    self as runners, ClaimedJob, JobHeartbeat, LogAppended, Runner, RunnerChangeRequest,
    RunnerJobResult, RunnerRegistration,
};
use forge_pipelines::{ChangeRequest, Pipelines};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, WriteParts};
use crate::routes::protected_refs::found;
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` the runners, `POST` register one.
pub const ADMIN: &str = "/api/v1/admin/runners";
/// `PATCH` one runner.
pub const ADMIN_ONE: &str = "/api/v1/admin/runners/{runner_id}";
/// `POST` claim a job (long-poll).
pub const REQUEST: &str = "/api/v1/runner/jobs/request";
/// `POST` extend the lease.
pub const HEARTBEAT: &str = "/api/v1/runner/jobs/{job_id}/heartbeat";
/// `POST` append a log chunk.
pub const LOG: &str = "/api/v1/runner/jobs/{job_id}/log";
/// `POST` report the result.
pub const COMPLETE: &str = "/api/v1/runner/jobs/{job_id}/complete";
/// The body cap of the administration and the small runner calls.
pub const MAX_BODY: usize = 4096;
/// The body cap of a log chunk (the registry's ceiling: send text of at
/// most 32 KiB, JSON escaping may double it).
pub const MAX_LOG_BODY: usize = 64 * 1024;

const ADMIN_PREFIX: &str = "/api/v1/admin/runners/";
const JOBS_PREFIX: &str = "/api/v1/runner/jobs/";

/// `POST /api/v1/admin/runners`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewRunnerBody {
    /// `[a-z0-9][a-z0-9._-]{0,62}`, unique.
    pub name: String,
    /// At most 1024 bytes.
    #[serde(default)]
    pub description: Option<String>,
    /// At most 16, each like a name. A job runs on a runner whose tags
    /// cover all of the job's.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

/// `PATCH /api/v1/admin/runners/{runner_id}`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerPatchBody {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub description: Option<String>,
}

/// A runner (never its token).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub enabled: bool,
    #[schema(minimum = 1)]
    pub created_by: i64,
    pub created_at: String,
    pub updated_at: String,
    #[schema(required = true)]
    pub last_contact_at: Option<String>,
}

/// `GET /api/v1/admin/runners`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerList {
    pub runners: Vec<RunnerBody>,
}

/// `201` of a registration: the runner and its token (shown once).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisteredRunnerBody {
    pub runner: RunnerBody,
    /// `fgr_<16 hex>_<64 hex>`: put it in the runner host's token file
    /// (ADR-0013); it is never shown again.
    pub token: String,
}

/// `POST /api/v1/runner/jobs/request`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JobRequestBody {
    /// How long to wait for a job, in seconds (0..=20, default 20).
    #[serde(default)]
    pub wait_s: Option<u64>,
}

/// A claimed job: what the runner needs to run it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimedJobBody {
    #[schema(minimum = 1)]
    pub job_id: i64,
    /// Send it back with every call on this job.
    pub attempt: i32,
    pub name: String,
    pub stage: String,
    /// Pinned by digest.
    pub image: String,
    pub timeout_s: i32,
    pub tags: Vec<String>,
    #[schema(minimum = 1)]
    pub pipeline_id: i64,
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// `refs/heads/…`.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// The pinned commit (the job reads only this).
    pub sha: String,
    /// sha256 of the `.forge-ci.yml` bytes the pipeline was planned from:
    /// the runner reads the job's script from that file at `sha` and
    /// refuses other bytes.
    #[schema(required = true)]
    pub definition_hash: Option<String>,
    /// Only a push to a branch protected when the pipeline was created
    /// (ADR-0029): the only jobs that may ever be given secrets.
    pub secrets_allowed: bool,
    /// The secret names the job asks for (empty unless `secrets_allowed`;
    /// no value before C6).
    pub secret_names: Vec<String>,
    /// The lease ends then unless a heartbeat extends it.
    pub lease_until: String,
    /// The job's token `fgj_…`: one project, this commit, this attempt;
    /// revoked when the job ends. Shown once.
    pub job_token: String,
    pub job_token_expires_at: String,
}

/// The answer of a claim.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JobRequestAnswer {
    /// `null`: nothing came within the wait.
    #[schema(required = true)]
    pub job: Option<ClaimedJobBody>,
}

/// `POST …/heartbeat` and the attempt of every job call.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatBody {
    #[schema(minimum = 1)]
    pub attempt: i32,
}

/// The answer of a heartbeat.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatAnswer {
    /// `running` or `canceled` (stop the job and complete it).
    pub state: String,
    #[schema(required = true)]
    pub lease_until: Option<String>,
}

/// `POST …/log`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LogChunkBody {
    #[schema(minimum = 1)]
    pub attempt: i32,
    /// 0-based, in order; a resent chunk keeps its number.
    #[schema(minimum = 0, maximum = 4095)]
    pub seq: i64,
    /// UTF-8 text, not empty.
    pub text: String,
}

/// The answer of a log append.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LogAnswer {
    /// `false`: the chunk was stored already (a resend).
    pub stored: bool,
}

/// `POST …/complete`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompleteBody {
    #[schema(minimum = 1)]
    pub attempt: i32,
    /// `succeeded` or `failed`.
    pub result: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
}

/// The answer of a completion.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompleteAnswer {
    /// The job's final status (`canceled` for an acknowledged cancel).
    pub status: String,
    pub pipeline_status: String,
}

fn pipelines(state: &AppState) -> Result<&Pipelines, ForgeError> {
    state.pipelines.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "pipelines_unconfigured")
    })
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

fn change_request<R: forge_auth::ResourceCtx, B>(w: &WriteParts<R, B>) -> ChangeRequest {
    ChangeRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
    }
}

fn runner_body(r: Runner) -> RunnerBody {
    RunnerBody {
        id: r.id,
        name: r.name,
        description: r.description,
        tags: r.tags,
        enabled: r.enabled,
        created_by: r.created_by,
        created_at: r.created_at,
        updated_at: r.updated_at,
        last_contact_at: r.last_contact_at,
    }
}

fn tags_of(raw: Option<&Vec<String>>) -> Result<Option<Vec<String>>, ForgeError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.len() > runners::MAX_TAGS {
        return Err(param_invalid("tags", "too_many"));
    }
    let mut tags: Vec<String> = Vec::with_capacity(raw.len());
    for t in raw {
        if !runners::valid_name(t) {
            return Err(param_invalid("tags", "invalid_tag"));
        }
        if !tags.contains(t) {
            tags.push(t.clone()); // clone: the body stays with the request
        }
    }
    tags.sort();
    Ok(Some(tags))
}

fn description_of(raw: Option<&String>) -> Result<Option<String>, ForgeError> {
    match raw {
        Some(d) if d.len() > runners::MAX_DESCRIPTION || d.chars().any(char::is_control) => {
            Err(param_invalid("description", "invalid"))
        }
        other => Ok(other.cloned()), // clone: the body stays with the request
    }
}

/// The instance: every administration route and the claim.
pub struct OnInstance;

impl Loader<AppState> for OnInstance {
    type Ctx = InstanceCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, InstanceCtx> {
        Box::pin(async { Ok(found(InstanceCtx)) })
    }
}

impl<B: Send + Sync + 'static> BodyLoader<AppState, B> for OnInstance {
    type Ctx = InstanceCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: &'a B,
        _: Option<UserId>,
    ) -> LoadFuture<'a, InstanceCtx> {
        Box::pin(async { Ok(found(InstanceCtx)) })
    }
}

/// `…/jobs/{job_id}/{tail}`: the job's lease.
pub struct JobLease {
    tail: &'static str,
}

/// `…/heartbeat`.
pub const LEASE_HEARTBEAT: JobLease = JobLease { tail: "heartbeat" };
/// `…/log`.
pub const LEASE_LOG: JobLease = JobLease { tail: "log" };
/// `…/complete`.
pub const LEASE_COMPLETE: JobLease = JobLease { tail: "complete" };

fn job_id(raw_path: &str, tail: &str) -> Result<i64, ForgeError> {
    raw_path
        .strip_prefix(JOBS_PREFIX)
        .and_then(|rest| rest.strip_suffix(tail))
        .and_then(|rest| rest.strip_suffix('/'))
        .filter(|id| !id.contains('/'))
        .and_then(parse_positive_id)
        .ok_or_else(|| param_invalid("job_id", "not_positive_decimal"))
}

impl<B: Send + Sync + 'static> BodyLoader<AppState, B> for JobLease {
    type Ctx = RunnerJobCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: &'a B,
        _: Option<UserId>,
    ) -> LoadFuture<'a, RunnerJobCtx> {
        Box::pin(async move {
            let job = job_id(parts.uri.path(), self.tail)?;
            let lease = bounded(state, "runner_job_lease", pipelines(state)?.lease_of(job)).await?;
            Ok(match lease {
                Some((holder, finished)) => found(RunnerJobCtx::new(job, holder, finished)),
                None => absent(),
            })
        })
    }
}

/// `GET /api/v1/admin/runners` → `200`.
pub async fn list(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let rows = bounded(&state, "runner_list", pipelines(&state)?.runners(&w.proof)).await?;
        json(RunnerList {
            runners: rows.into_iter().map(runner_body).collect(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST /api/v1/admin/runners` → `201`.
pub async fn register(
    state: AppState,
    request: Dispatched<InstanceCtx, NewRunnerBody>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let input = &w.input;
        if !runners::valid_name(&input.name) {
            return Err(param_invalid("name", "invalid_name"));
        }
        let new = RunnerRegistration {
            name: input.name.clone(), // clone: the body stays with the request
            description: description_of(input.description.as_ref())?.unwrap_or_default(),
            tags: tags_of(input.tags.as_ref())?.unwrap_or_default(),
        };
        let registered = pipelines(&state)?
            .register_runner(&change_request(&w), &w.proof, &new)
            .await?;
        json(RegisteredRunnerBody {
            token: registered.token.reveal(),
            runner: runner_body(registered.runner),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::CREATED, built)
}

/// `PATCH /api/v1/admin/runners/{runner_id}` → `200`.
pub async fn update(
    state: AppState,
    request: Dispatched<InstanceCtx, RunnerPatchBody>,
) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let id = path
            .strip_prefix(ADMIN_PREFIX)
            .filter(|id| !id.contains('/'))
            .and_then(parse_positive_id)
            .ok_or_else(|| param_invalid("runner_id", "not_positive_decimal"))?;
        let change = RunnerChangeRequest {
            enabled: w.input.enabled,
            tags: tags_of(w.input.tags.as_ref())?,
            description: description_of(w.input.description.as_ref())?,
        };
        let updated = pipelines(&state)?
            .update_runner(&change_request(&w), &w.proof, id, &change)
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::NotFound)
                    .with_context("resource", "runner")
                    .with_context("runner_id", id)
            })?;
        json(runner_body(updated))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

fn claimed_body(c: ClaimedJob) -> ClaimedJobBody {
    ClaimedJobBody {
        job_id: c.job_id,
        attempt: c.attempt,
        name: c.name,
        stage: c.stage,
        image: c.image,
        timeout_s: c.timeout_s,
        tags: c.tags,
        pipeline_id: c.pipeline.id,
        project_id: c.pipeline.project.get(),
        git_ref: c.pipeline.git_ref,
        sha: c.pipeline.sha,
        definition_hash: c.pipeline.definition_hash,
        secrets_allowed: c.secrets_allowed,
        secret_names: c.secret_names,
        lease_until: c.lease_until,
        job_token: c.token.reveal(),
        job_token_expires_at: c.token_expires_at,
    }
}

/// `POST /api/v1/runner/jobs/request` → `200`.
pub async fn request_job(
    state: AppState,
    request: Dispatched<InstanceCtx, JobRequestBody>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let max = runners::LONG_POLL_MAX.as_secs();
        let wait = match w.input.wait_s {
            None => max,
            Some(s) if s <= max => s,
            Some(_) => return Err(param_invalid("wait_s", "out_of_range")),
        };
        let claimed = pipelines(&state)?
            .request_job(&w.ctx.request_id, &w.proof, Duration::from_secs(wait))
            .await?;
        json(JobRequestAnswer {
            job: claimed.map(claimed_body),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/jobs/{job_id}/heartbeat` → `200`.
pub async fn heartbeat(
    state: AppState,
    request: Dispatched<RunnerJobCtx, HeartbeatBody>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let answer = match pipelines(&state)?
            .heartbeat(&w.proof, w.input.attempt)
            .await?
        {
            JobHeartbeat::Running { lease_until } => HeartbeatAnswer {
                state: "running".to_owned(),
                lease_until: Some(lease_until),
            },
            JobHeartbeat::Canceled => HeartbeatAnswer {
                state: "canceled".to_owned(),
                lease_until: None,
            },
        };
        json(answer)
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/jobs/{job_id}/log` → `200`.
pub async fn append_log(
    state: AppState,
    request: Dispatched<RunnerJobCtx, LogChunkBody>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let input = &w.input;
        if input.text.is_empty() {
            return Err(param_invalid("text", "empty"));
        }
        if input.text.contains('\0') {
            return Err(param_invalid("text", "nul"));
        }
        if !(0..runners::MAX_LOG_CHUNKS).contains(&input.seq) {
            return Err(param_invalid("seq", "out_of_range"));
        }
        let appended = pipelines(&state)?
            .append_log(&w.proof, input.attempt, input.seq, &input.text)
            .await?;
        json(LogAnswer {
            stored: appended == LogAppended::Stored,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST …/jobs/{job_id}/complete` → `200`.
pub async fn complete(
    state: AppState,
    request: Dispatched<RunnerJobCtx, CompleteBody>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let result = match w.input.result.as_str() {
            "succeeded" => RunnerJobResult::Succeeded,
            "failed" => RunnerJobResult::Failed,
            _ => return Err(param_invalid("result", "unknown")),
        };
        let done = pipelines(&state)?
            .complete(
                &w.ctx.request_id,
                &w.proof,
                w.input.attempt,
                result,
                w.input.exit_code,
            )
            .await?;
        if matches!(
            done.pipeline_status.as_str(),
            "succeeded" | "failed" | "canceled"
        ) {
            drop_pipeline_ref(&state, &w.ctx.request_id, done.job_id, done.pipeline_id).await;
        }
        json(CompleteAnswer {
            status: done.status.to_owned(),
            pipeline_status: done.pipeline_status,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// Delete the ended pipeline's ref (M4 C4 review). Never fails the
/// completion: a failure is logged once here with its code, and the
/// maintenance sweep deletes the ref later. Without Smart HTTP no job
/// fetch ran, so there is no ref.
async fn drop_pipeline_ref(
    state: &AppState,
    request_id: &forge_core::RequestId,
    job: i64,
    pipeline: i64,
) {
    let Some(smart) = state.smart_http.as_deref() else {
        return;
    };
    let deleted = async {
        let copy = pipelines(state)?.job_copy(job).await?.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "completed_job_missing")
        })?;
        smart
            .git
            .delete_pipeline_ref(
                &smart.storage,
                copy.repository.id,
                pipeline,
                smart.limits.want_check_timeout,
            )
            .await
    }
    .await;
    if let Err(e) = deleted {
        tracing::warn!(
            target: "forge_web",
            event = "forge.ci.pipeline_ref_delete_failed",
            error_code = e.code().id(),
            request_id = %request_id,
            job_id = job,
            pipeline_id = pipeline,
            error = %e,
            "the ended pipeline's ref was not deleted; the maintenance sweep deletes it"
        );
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_job_paths_name_the_job() {
        assert_eq!(
            job_id("/api/v1/runner/jobs/7/heartbeat", "heartbeat").expect("id"),
            7
        );
        for bad in [
            "/api/v1/runner/jobs/0/heartbeat",
            "/api/v1/runner/jobs/x/heartbeat",
            "/api/v1/runner/jobs/7/log",
            "/api/v1/runner/jobs/7/8/heartbeat",
        ] {
            assert!(job_id(bad, "heartbeat").is_err(), "{bad}");
        }
    }

    #[test]
    fn tags_are_checked_deduplicated_and_sorted() {
        let tags = tags_of(Some(&vec![
            "podman".to_owned(),
            "linux".to_owned(),
            "linux".to_owned(),
        ]))
        .expect("tags")
        .expect("some");
        assert_eq!(tags, vec!["linux".to_owned(), "podman".to_owned()]);
        assert!(tags_of(Some(&vec!["Linux".to_owned()])).is_err());
        assert!(tags_of(Some(&vec!["t".to_owned(); 17])).is_err());
        assert!(description_of(Some(&"a\u{7}".to_owned())).is_err());
    }

    /// The long-poll must answer before `forge serve`'s request timeout
    /// (never below `AppConfig::default()`, 30 s).
    #[test]
    fn the_long_poll_ends_before_the_request_timeout() {
        let limit = crate::app::AppConfig::default().request_timeout;
        assert!(runners::LONG_POLL_MAX + runners::POLL_INTERVAL * 5 < limit);
    }
}
