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
//! - `GET …/pipelines/{pipeline_id}/jobs/{job_id}/log?attempt=&from=`
//!   (`CiLogRead`, M4 C5): one page of a job attempt's log (whole chunks
//!   from `from`, at most 256 chunks / 1 MiB), masked; `next_from` names
//!   the next page, `complete` says nothing more will come.
//! - `GET …/pipelines/{pipeline_id}/diagnosis` (`PipelineDiagnosisRead`,
//!   M4 C5, plan 04 P-9 deterministic part): the failed jobs with their
//!   fingerprints, error codes, masked excerpts and known fixes.
//!
//! The pipeline routes answer metadata (an agent audience reads it too;
//! the secret names a job uses are listed, never a value). The log and the
//! diagnosis are CI outputs (ADR-0029 decision 5): after the project read,
//! an AI agent audience is measured — the default branch's tip, as a clone
//! is (`repo_copy::measure`, cached per tip) — and `authorize_ci_output`
//! refuses it with `FG-AUTH-0022` unless the pipeline's own mark and the
//! tip are both open. A person is never measured nor refused by the marks.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{CopyMark, ProjectCtx};
use forge_core::{parse_positive_id, ProjectId, RefName, RepoId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_pipelines::outputs::{Failure as StoredFailure, OutputView};
use forge_pipelines::{
    ChangeRequest, ManualRun, PipelineDetail, Pipelines, StoredJob, StoredPipeline,
};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, QueryParam, WriteParts};
use crate::routes::projects::{self, PREFIX};
use crate::routes::repo_copy::{measure, refusal_context, Measured};
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` the list, `POST` a manual run.
pub const LIST: &str = "/api/v1/projects/{id}/pipelines";
/// `GET` one pipeline.
pub const ONE: &str = "/api/v1/projects/{id}/pipelines/{pipeline_id}";
/// `POST` cancel.
pub const CANCEL: &str = "/api/v1/projects/{id}/pipelines/{pipeline_id}/cancel";
/// `POST` retry.
pub const RETRY: &str = "/api/v1/projects/{id}/pipelines/{pipeline_id}/retry";
/// `GET` a job's log (M4 C5).
pub const LOG: &str = "/api/v1/projects/{id}/pipelines/{pipeline_id}/jobs/{job_id}/log";
/// `GET` a pipeline's diagnosis (M4 C5).
pub const DIAGNOSIS: &str = "/api/v1/projects/{id}/pipelines/{pipeline_id}/diagnosis";
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

/// The log's query parameters.
pub const LOG_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "attempt",
        description: "The job's attempt (default its current one)",
        integer_range: Some((1, 100)),
    },
    QueryParam {
        name: "from",
        description: "The first chunk (0-based; the previous page's `next_from`; default 0)",
        integer_range: Some((0, 4095)),
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
    /// `true` only for a push to a branch that was protected when the
    /// pipeline was created: the only pipelines whose jobs may be given
    /// secrets (ADR-0029).
    pub protected: bool,
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

/// `GET …/jobs/{job_id}/log`: one page of a job attempt's log.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JobLogPage {
    #[schema(minimum = 1)]
    pub pipeline_id: i64,
    #[schema(minimum = 1)]
    pub job_id: i64,
    /// The job's name.
    pub job: String,
    /// `created`, `pending`, `running`, `succeeded`, `failed`, `canceled`.
    pub job_status: String,
    /// The attempt this page is of.
    #[schema(minimum = 1)]
    pub attempt: i32,
    /// The job's current attempt.
    #[schema(minimum = 1)]
    pub current_attempt: i32,
    /// The first chunk in this page (`null`: none stored from `from` on
    /// yet).
    #[schema(required = true, minimum = 0)]
    pub from: Option<i32>,
    /// The last chunk in this page.
    #[schema(required = true, minimum = 0)]
    pub to: Option<i32>,
    /// The page's text, secrets masked as `[MASKED]`.
    pub text: String,
    /// Pass as `from` for the next page; `null`: nothing stored after this
    /// page yet.
    #[schema(required = true, minimum = 0)]
    pub next_from: Option<i32>,
    /// The attempt is over and nothing is left after this page.
    pub complete: bool,
}

/// A known fix of a failure (plan 04 P-7): the merge request whose
/// pipeline failed with the same fingerprint and later succeeded.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct KnownFixBody {
    #[schema(minimum = 1)]
    pub merge_request_id: i64,
    #[schema(minimum = 1)]
    pub iid: i64,
    pub title: String,
    pub fixed_at: String,
}

/// Where a failure's excerpt is in the job's log.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FailureSourceBody {
    /// The log route of the attempt (`GET`, CiLogRead).
    pub log_path: String,
    /// The excerpt's first and last line in the attempt's log.
    #[schema(required = true)]
    pub lines: Option<[i32; 2]>,
}

/// One failed job of a pipeline (plan 04 P-9, deterministic).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FailureBody {
    /// The job's name.
    pub job: String,
    #[schema(minimum = 1)]
    pub job_id: i64,
    pub stage: String,
    #[schema(minimum = 1)]
    pub attempt: i32,
    /// `true` while the worker has not fingerprinted it yet (seconds after
    /// the job failed): the fields below are empty then.
    pub pending: bool,
    /// `fp_` + 16 hex digits: the same failure on any commit has the same
    /// one.
    #[schema(required = true)]
    pub fingerprint: Option<String>,
    /// The `FG-…` codes: the job's own first, then those its log names.
    pub error_codes: Vec<String>,
    /// The failure lines, secrets masked.
    pub excerpt: String,
    pub source: FailureSourceBody,
    /// How often this fingerprint failed in the project.
    #[schema(minimum = 0)]
    pub occurrences: i32,
    #[schema(required = true)]
    pub first_seen_at: Option<String>,
    /// Deterministic fixes (P-7), never a guess.
    pub known_fixes: Vec<KnownFixBody>,
}

/// `GET …/pipelines/{pipeline_id}/diagnosis`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DiagnosisBody {
    #[schema(minimum = 1)]
    pub pipeline_id: i64,
    pub status: String,
    #[schema(required = true)]
    pub error_code: Option<String>,
    pub failures: Vec<FailureBody>,
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
        protected: p.protected,
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
    Log(i64, i64),
    Diagnosis(i64),
}

impl Tail {
    fn kind(self) -> u8 {
        match self {
            Self::List => 0,
            Self::One(_) => 1,
            Self::Cancel(_) => 2,
            Self::Retry(_) => 3,
            Self::Log(..) => 4,
            Self::Diagnosis(_) => 5,
        }
    }

    fn pipeline(self) -> Option<i64> {
        match self {
            Self::List => None,
            Self::One(id)
            | Self::Cancel(id)
            | Self::Retry(id)
            | Self::Log(id, _)
            | Self::Diagnosis(id) => Some(id),
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
        "diagnosis" => Tail::Diagnosis(id),
        other => {
            let job = other
                .strip_prefix("jobs/")
                .and_then(|rest| rest.strip_suffix("/log"))
                .ok_or_else(bad)?;
            let job = parse_positive_id(job)
                .ok_or_else(|| param_invalid("job_id", "not_positive_decimal"))?;
            Tail::Log(id, job)
        }
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
/// `GET …/jobs/{job_id}/log`.
pub const OF_LOG: PipelinesOf = PipelinesOf { tail: 4 };
/// `GET …/diagnosis`.
pub const OF_DIAGNOSIS: PipelinesOf = PipelinesOf { tail: 5 };

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

/// The request a change belongs to (the audit row's request and token).
fn change_request<B>(w: &WriteParts<ProjectCtx, B>) -> ChangeRequest {
    ChangeRequest {
        request_id: w.ctx.request_id.clone(), // clone: the context stays for the error path
        credential_id: w.credential_id,
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

/// The default branch's tip mark for a CI output (module docs): an AI
/// agent audience is measured as a clone is; a person is not. A tip that
/// cannot be measured — no Smart HTTP state, a repository not ready, a
/// failed measurement — is `Undecided`: refused to an agent (fail closed),
/// with the cause in the refusal's log line.
async fn tip_mark(state: &AppState, target: TipTarget) -> (CopyMark, Option<(RepoId, Measured)>) {
    match target {
        TipTarget::Person => (CopyMark::NotMeasured, None),
        // Answered before (the header's own error); never measured.
        TipTarget::Refused(_) => (CopyMark::Undecided, None),
        TipTarget::Unmeasurable(repo, e) => (
            CopyMark::Undecided,
            repo.map(|r| (r, Measured::undecided(None, e))),
        ),
        TipTarget::Measure(repo, key) => match state.smart_http.as_ref() {
            Some(smart) => {
                let measured = measure(smart, repo, &key, &smart.copy_limits).await;
                (measured.mark, Some((repo, measured)))
            }
            None => (CopyMark::Undecided, None),
        },
    }
}

/// What [`tip_mark`] measures, read from the request before any await
/// (the request's body is not `Sync`).
enum TipTarget {
    Person,
    /// The audience header was malformed (`FG-WEB-0006`).
    Refused(ForgeError),
    Unmeasurable(Option<RepoId>, ForgeError),
    Measure(RepoId, String),
}

fn tip_target(state: &AppState, request: &Dispatched<ProjectCtx>) -> TipTarget {
    // The principal's audience, else `Forge-Context-Audience: agent`
    // (`gstn mcp` always sends it): an agent is measured.
    match crate::routes::context::audience(request) {
        Ok(a) if a.is_agent() => {}
        Ok(_) => return TipTarget::Person,
        Err(e) => return TipTarget::Refused(e),
    }
    // A fixed route carries no dispatcher repository: the loader's project
    // row names it (as for browse).
    match crate::routes::browse::Target::of(state, request) {
        Ok(t) => TipTarget::Measure(t.repository.id, t.principal),
        Err(e) => TipTarget::Unmeasurable(request.project().map(|r| r.repository.id), e),
    }
}

/// Whether the request declared an AI agent audience although its
/// principal is a person (`Forge-Context-Audience: agent`): `authorize()`
/// then decides it as an agent's (`audience=header`).
fn declared_agent(request: &Dispatched<ProjectCtx>) -> bool {
    !request.principal().is_agent_audience()
        && matches!(
            crate::routes::context::audience(request),
            Ok(crate::routes::context::Audience::Agent(_))
        )
}

/// A CI output's refusal with what the measurement found (`tip`,
/// `paths`, `check_error`), for the one log line at the boundary.
fn ci_output_refusal(err: ForgeError, measured: Option<&(RepoId, Measured)>) -> ForgeError {
    match measured {
        Some((repo, m)) if err.code() == ErrorCode::RepositoryCopyWithheld => {
            refusal_context(err, *repo, m)
        }
        _ => err,
    }
}

/// The log's query: `(attempt, from)`.
fn log_query(query: Option<&str>) -> Result<(Option<i32>, i32), ForgeError> {
    let (mut attempt, mut from) = (None, None);
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
        let number = |name: &'static str, min: i64, max: i64| {
            Some(raw)
                .filter(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|r| r.parse::<i64>().ok())
                .filter(|n| (min..=max).contains(n))
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| param_invalid(name, "out_of_range"))
        };
        let repeated = match key {
            "attempt" => attempt.replace(number("attempt", 1, 100)?).is_some(),
            "from" => from.replace(number("from", 0, 4095)?).is_some(),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if repeated {
            let name = if key == "attempt" { "attempt" } else { "from" };
            return Err(param_invalid(name, "repeated"));
        }
    }
    Ok((attempt, from.unwrap_or(0)))
}

/// `GET …/pipelines/{pipeline_id}/jobs/{job_id}/log` → `200`.
pub async fn log(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let query = request.raw_query().map(str::to_owned);
    let target = tip_target(&state, &request);
    let declared_agent = declared_agent(&request);
    let w = request.into_write();
    let (target, refused) = match target {
        TipTarget::Refused(e) => (TipTarget::Person, Some(e)),
        t @ (TipTarget::Person | TipTarget::Unmeasurable(..) | TipTarget::Measure(..)) => (t, None),
    };
    let (tip_mark, measured) = tip_mark(&state, target).await;
    let view = OutputView {
        tip_mark,
        declared_agent,
    };
    let built = async {
        if let Some(e) = refused {
            return Err(e);
        }
        let (attempt, from) = log_query(query.as_deref())?;
        let (_, tail) = ids(&path)?;
        let Tail::Log(pipeline, job) = tail else {
            return Err(not_found());
        };
        let page = bounded(
            &state,
            "pipeline_job_log",
            pipelines(&state)?.job_log(&w.proof, view, (pipeline, job), attempt, from),
        )
        .await
        .map_err(|e| ci_output_refusal(e, measured.as_ref()))?
        .ok_or_else(|| ForgeError::new(ErrorCode::NotFound).with_context("resource", "job"))?;
        json(JobLogPage {
            pipeline_id: page.pipeline_id,
            job_id: page.job_id,
            job: page.job_name,
            job_status: page.job_status.as_str().to_owned(),
            attempt: page.attempt,
            current_attempt: page.current_attempt,
            from: page.chunks.map(|c| c.0),
            to: page.chunks.map(|c| c.1),
            text: page.text,
            next_from: page.next_seq,
            complete: page.complete,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

fn failure_body(project: ProjectId, pipeline: i64, f: StoredFailure) -> FailureBody {
    let log_path = format!(
        "/api/v1/projects/{}/pipelines/{pipeline}/jobs/{}/log?attempt={}",
        project.get(),
        f.job_id,
        f.attempt
    );
    let pending = f.found.is_none();
    let mut error_codes: Vec<String> = f.job_error_code.into_iter().collect();
    let Some(found) = f.found else {
        return FailureBody {
            job: f.job_name,
            job_id: f.job_id,
            stage: f.stage,
            attempt: f.attempt,
            pending,
            fingerprint: None,
            error_codes,
            excerpt: String::new(),
            source: FailureSourceBody {
                log_path,
                lines: None,
            },
            occurrences: 0,
            first_seen_at: None,
            known_fixes: Vec::new(),
        };
    };
    for code in found.error_codes {
        if !error_codes.contains(&code) {
            error_codes.push(code);
        }
    }
    FailureBody {
        job: f.job_name,
        job_id: f.job_id,
        stage: f.stage,
        attempt: f.attempt,
        pending,
        fingerprint: Some(found.fingerprint),
        error_codes,
        excerpt: found.excerpt,
        source: FailureSourceBody {
            log_path,
            lines: found.lines.map(|(a, b)| [a, b]),
        },
        occurrences: found.occurrences,
        first_seen_at: Some(found.first_seen_at),
        known_fixes: found
            .known_fix
            .map(|k| KnownFixBody {
                merge_request_id: k.merge_request_id,
                iid: k.iid,
                title: k.title,
                fixed_at: k.fixed_at,
            })
            .into_iter()
            .collect(),
    }
}

/// `GET …/pipelines/{pipeline_id}/diagnosis` → `200`.
pub async fn diagnosis(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let target = tip_target(&state, &request);
    let declared_agent = declared_agent(&request);
    let w = request.into_write();
    let (target, refused) = match target {
        TipTarget::Refused(e) => (TipTarget::Person, Some(e)),
        t @ (TipTarget::Person | TipTarget::Unmeasurable(..) | TipTarget::Measure(..)) => (t, None),
    };
    let (tip_mark, measured) = tip_mark(&state, target).await;
    let view = OutputView {
        tip_mark,
        declared_agent,
    };
    let built = async {
        if let Some(e) = refused {
            return Err(e);
        }
        let id = pipeline_id(&path)?;
        let project = *w.proof.id();
        let got = bounded(
            &state,
            "pipeline_diagnosis",
            pipelines(&state)?.diagnosis(&w.proof, view, id),
        )
        .await
        .map_err(|e| ci_output_refusal(e, measured.as_ref()))?
        .ok_or_else(not_found)?;
        json(DiagnosisBody {
            pipeline_id: got.pipeline_id,
            status: got.status.as_str().to_owned(),
            error_code: got.error_code,
            failures: got
                .failures
                .into_iter()
                .map(|f| failure_body(project, id, f))
                .collect(),
        })
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
            .cancel(&change_request(&w), &w.proof, id)
            .await?;
        json(detail_body(done))
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
            .retry(&change_request(&w), &w.proof, id)
            .await?;
        json(detail_body(done))
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
            .run_manual(&change_request(&w), &w.proof, &git_ref)
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
        assert_eq!(
            ids("/api/v1/projects/7/pipelines/3/jobs/9/log").expect("log"),
            (ProjectId::new(7), Tail::Log(3, 9))
        );
        assert_eq!(
            ids("/api/v1/projects/7/pipelines/3/diagnosis").expect("diagnosis"),
            (ProjectId::new(7), Tail::Diagnosis(3))
        );
        for bad in [
            "/api/v1/projects/0/pipelines",
            "/api/v1/projects/7/pipelinesx",
            "/api/v1/projects/7/pipelines/0",
            "/api/v1/projects/7/pipelines/3/other",
            "/api/v1/projects/7/pipelines/x/cancel",
            "/api/v1/projects/7/pipelines/3/jobs/0/log",
            "/api/v1/projects/7/pipelines/3/jobs/9",
            "/api/v1/projects/7/pipelines/3/jobs/9/log/x",
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

    #[test]
    fn the_log_query_takes_an_attempt_and_a_first_chunk() {
        assert_eq!(log_query(None).expect("none"), (None, 0));
        assert_eq!(
            log_query(Some("attempt=2&from=17")).expect("both"),
            (Some(2), 17)
        );
        for bad in [
            "attempt=0",
            "attempt=101",
            "from=-1",
            "from=4096",
            "from=+3",
            "from=",
            "x=1",
            "from=1&from=2",
        ] {
            assert!(log_query(Some(bad)).is_err(), "{bad}");
        }
    }
}
