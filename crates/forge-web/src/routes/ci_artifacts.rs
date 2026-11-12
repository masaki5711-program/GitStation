//! CI artifacts (M4 C6, plan 14 §4.2; ADR-0029 decision 5).
//!
//! A job's own token (`fgj_…`, `JobArtifactUpload` / `JobArtifactDownload`
//! on its own job: another job's id is `404 FG-REPO-0005`):
//! - `PUT /api/v1/runner/jobs/{job_id}/artifacts?path=<relative path>`: the
//!   raw bytes of one file (any content type) → `201 {"id", "path",
//!   "size_bytes", "sha256"}`; while the job runs only (`409 FG-CI-0004`);
//!   `413 FG-CI-0019` past 100 MiB per file or 512 MiB / 1000 files per
//!   attempt; `400 FG-WEB-0006` for a bad path or one stored already.
//! - `GET /api/v1/runner/jobs/{job_id}/needs/artifacts`: the artifacts of
//!   the jobs it `needs` (their current, succeeded attempt) in its own
//!   pipeline; `GET …/needs/artifacts/{artifact_id}`: one file's bytes.
//!
//! People and agents (`CiArtifactRead`, like the job's log: an AI agent
//! audience only when both marks are open, `403 FG-AUTH-0022
//! route=ci_artifact`):
//! - `GET /api/v1/projects/{id}/pipelines/{pipeline_id}/jobs/{job_id}/artifacts`
//! - `GET …/artifacts/{artifact_id}`: the bytes, `application/octet-stream`,
//!   `Content-Disposition: attachment`, `X-Content-Type-Options: nosniff`,
//!   `X-Forge-Sha256`.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use forge_auth::JobSelfCtx;
use forge_core::{parse_positive_id, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_pipelines::artifacts::{Artifact, StoredUpload};
use forge_pipelines::Pipelines;
use futures_util::StreamExt as _;
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, QueryParam};
use crate::routes::write::{absent, bounded, json, no_store};

/// `PUT` one file of a job.
pub const UPLOAD: &str = "/api/v1/runner/jobs/{job_id}/artifacts";
/// `GET` the artifacts of the jobs a job needs.
pub const NEEDS: &str = "/api/v1/runner/jobs/{job_id}/needs/artifacts";
/// `GET` one of them.
pub const NEED_ONE: &str = "/api/v1/runner/jobs/{job_id}/needs/artifacts/{artifact_id}";

const JOBS_PREFIX: &str = "/api/v1/runner/jobs/";
/// How long an upload may wait for its next bytes.
const IDLE: std::time::Duration = std::time::Duration::from_secs(60);
/// How long a whole upload may take.
pub const UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3600);
/// The size of one read of a stored file.
const READ_CHUNK: usize = 64 * 1024;

/// The query of an upload (`openapi.json`).
pub const UPLOAD_QUERY: &[QueryParam] = &[QueryParam {
    name: "path",
    description: "The file's path relative to the job's work tree (no leading /, no . / .. / .git component, at most 1024 bytes)",
    integer_range: None,
}];

/// `201` of an upload.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UploadedArtifactBody {
    #[schema(minimum = 1)]
    pub id: i64,
    pub path: String,
    pub size_bytes: u64,
    /// Lower-case hex.
    pub sha256: String,
}

/// One artifact's metadata.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactBody {
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(minimum = 1)]
    pub pipeline_id: i64,
    #[schema(minimum = 1)]
    pub job_id: i64,
    /// The job that uploaded it.
    pub job: String,
    pub attempt: i32,
    pub path: String,
    pub size_bytes: i64,
    pub sha256: String,
    /// Kept past the retention (a deployed version).
    pub keep: bool,
    pub created_at: String,
    pub expires_at: String,
}

/// A list of artifacts.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactList {
    pub artifacts: Vec<ArtifactBody>,
}

pub(crate) fn artifact_body(a: Artifact) -> ArtifactBody {
    ArtifactBody {
        id: a.id,
        pipeline_id: a.pipeline_id,
        job_id: a.job_id,
        job: a.job_name,
        attempt: a.attempt,
        path: a.path,
        size_bytes: a.size_bytes,
        sha256: a.sha256,
        keep: a.keep,
        created_at: a.created_at,
        expires_at: a.expires_at,
    }
}

fn pipelines(state: &AppState) -> Result<&Pipelines, ForgeError> {
    state.pipelines.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "pipelines_unconfigured")
    })
}

/// What a job route names after `…/jobs/{job_id}/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobTail {
    Upload,
    Needs,
    NeedOne(i64),
}

fn job_path(raw: &str) -> Result<(i64, JobTail), ForgeError> {
    let bad = || param_invalid("job_id", "not_positive_decimal");
    let rest = raw.strip_prefix(JOBS_PREFIX).ok_or_else(bad)?;
    let (job, tail) = rest.split_once('/').ok_or_else(bad)?;
    let job = parse_positive_id(job).ok_or_else(bad)?;
    let tail = match tail {
        "artifacts" => JobTail::Upload,
        "needs/artifacts" => JobTail::Needs,
        other => {
            let id = other
                .strip_prefix("needs/artifacts/")
                .filter(|id| !id.contains('/'))
                .and_then(parse_positive_id)
                .ok_or_else(|| param_invalid("artifact_id", "not_positive_decimal"))?;
            JobTail::NeedOne(id)
        }
    };
    Ok((job, tail))
}

/// Is this request a job's artifact upload (its body is read before the
/// answer: it gets [`UPLOAD_TIMEOUT`], not the request timeout)?
pub(crate) fn is_upload(method: &axum::http::Method, path: &str) -> bool {
    method == axum::http::Method::PUT && matches!(job_path(path), Ok((_, JobTail::Upload)))
}

/// `…/jobs/{job_id}/…`: the job and its project (the decision compares
/// them with the job's token).
pub struct JobOf {
    upload: bool,
}

/// `PUT …/artifacts`.
pub const OF_UPLOAD: JobOf = JobOf { upload: true };
/// `GET …/needs/artifacts[/{artifact_id}]`.
pub const OF_NEEDS: JobOf = JobOf { upload: false };

impl Loader<AppState> for JobOf {
    type Ctx = JobSelfCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, JobSelfCtx> {
        Box::pin(async move {
            let (job, tail) = job_path(parts.uri.path())?;
            if (tail == JobTail::Upload) != self.upload {
                return Err(param_invalid("job_id", "not_positive_decimal"));
            }
            let copy = bounded(
                state,
                "job_artifact_target",
                pipelines(state)?.job_copy(job),
            )
            .await?;
            Ok(match copy {
                Some(c) => crate::routes::protected_refs::found(JobSelfCtx::new(job, c.project)),
                None => absent(),
            })
        })
    }
}

fn query_path(raw_query: Option<&str>) -> Result<String, ForgeError> {
    let raw = raw_query.unwrap_or("");
    let mut found = None;
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "path" {
            return Err(param_invalid("query", "unknown_parameter"));
        }
        if found.is_some() {
            return Err(param_invalid("path", "repeated"));
        }
        found = Some(percent_decode(value).ok_or_else(|| param_invalid("path", "not_utf8"))?);
    }
    found.ok_or_else(|| param_invalid("path", "missing"))
}

/// `%XX` decoded (`+` is a plus: the runner encodes every reserved byte);
/// `None` for a bad escape or bytes that are not UTF-8.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `PUT …/jobs/{job_id}/artifacts?path=` → `201`.
pub async fn upload(state: AppState, request: Dispatched<JobSelfCtx>) -> Response {
    let query = request.raw_query().map(str::to_owned);
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    let job = request.proof().ctx().job();
    let built = async {
        let path = query_path(query.as_deref())?;
        let pipelines = pipelines(&state)?;
        let mut upload = pipelines.begin_artifact(request.proof(), &path).await?;
        let mut stream = request.into_body().into_data_stream();
        let deadline = tokio::time::Instant::now() + UPLOAD_TIMEOUT;
        loop {
            let wait = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(IDLE);
            let next = match tokio::time::timeout(wait, stream.next()).await {
                Ok(next) => next,
                Err(_) => {
                    upload.abort().await;
                    return Err(ForgeError::new(ErrorCode::RequestTimeout)
                        .with_context("operation", "artifact_upload")
                        .with_context("timeout_ms", wait.as_millis()));
                }
            };
            match next {
                None => break,
                Some(Ok(bytes)) => {
                    if let Err(e) = upload.write(&bytes).await {
                        upload.abort().await;
                        return Err(e);
                    }
                }
                Some(Err(_)) => {
                    upload.abort().await;
                    return Err(ForgeError::new(ErrorCode::RequestInvalid)
                        .with_context("reason", "body_aborted"));
                }
            }
        }
        let stored: StoredUpload = upload.finish(pipelines).await?;
        tracing::info!(
            target: "forge_web",
            event = "forge.ci.artifact_stored",
            request_id = %ctx.request_id,
            job_id = stored.job_id,
            attempt = stored.attempt,
            artifact_id = stored.id,
            size_bytes = stored.size_bytes,
            "CI artifact stored"
        );
        json(UploadedArtifactBody {
            id: stored.id,
            path: stored.path,
            size_bytes: stored.size_bytes,
            sha256: stored.sha256,
        })
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::CREATED, value),
        Err(e) => ctx.fail(e.with_context("job_id", job)).into_response(),
    }
}

/// `GET …/jobs/{job_id}/needs/artifacts` → `200`.
pub async fn needs(state: AppState, request: Dispatched<JobSelfCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let rows = bounded(
            &state,
            "job_needed_artifacts",
            pipelines(&state)?.needed_artifacts(&w.proof),
        )
        .await?;
        json(ArtifactList {
            artifacts: rows.into_iter().map(artifact_body).collect(),
        })
    }
    .await;
    match built {
        Ok(value) => no_store(StatusCode::OK, value),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `GET …/jobs/{job_id}/needs/artifacts/{artifact_id}` → `200` the bytes.
pub async fn need_one(state: AppState, request: Dispatched<JobSelfCtx>) -> Response {
    let path = request.raw_path().to_owned();
    let w = request.into_write();
    let built = async {
        let Ok((_, JobTail::NeedOne(id))) = job_path(&path) else {
            return Err(param_invalid("artifact_id", "not_positive_decimal"));
        };
        let found = bounded(
            &state,
            "job_needed_artifact",
            pipelines(&state)?.needed_artifact_file(&w.proof, id),
        )
        .await?
        .ok_or_else(not_found)?;
        file_response(found).await
    }
    .await;
    match built {
        Ok(res) => res,
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

pub(crate) fn not_found() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound).with_context("resource", "artifact")
}

/// The stored file as the answer (streamed in [`READ_CHUNK`] reads).
pub(crate) async fn file_response(
    (row, file): (Artifact, PathBuf),
) -> Result<Response, ForgeError> {
    let opened = tokio::fs::File::open(&file).await.map_err(|e| {
        let code = if e.kind() == std::io::ErrorKind::NotFound {
            // The row is there but not the file: expired meanwhile, or lost.
            ErrorCode::NotFound
        } else {
            ErrorCode::StorageIoFailed
        };
        ForgeError::new(code)
            .with_context("resource", "artifact")
            .with_context("operation", "artifact_open")
            .with_context("artifact_id", row.id)
            .with_source(e)
    })?;
    let stream = futures_util::stream::unfold(Some(opened), |state| async move {
        use tokio::io::AsyncReadExt as _;
        let mut f = state?;
        let mut buf = vec![0u8; READ_CHUNK];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((
                    Ok::<_, std::io::Error>(axum::body::Bytes::from(buf)),
                    Some(f),
                ))
            }
            Err(e) => Some((Err(e), None)),
        }
    });
    let name = row
        .path
        .rsplit('/')
        .next()
        .unwrap_or("artifact")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let mut res = Body::from_stream(stream).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Ok(v) = HeaderValue::from_str(&row.size_bytes.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(&row.sha256) {
        h.insert("x-forge-sha256", v);
    }
    Ok(res)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_job_paths_name_the_job_and_the_artifact() {
        assert_eq!(
            job_path("/api/v1/runner/jobs/7/artifacts").expect("upload"),
            (7, JobTail::Upload)
        );
        assert_eq!(
            job_path("/api/v1/runner/jobs/7/needs/artifacts").expect("needs"),
            (7, JobTail::Needs)
        );
        assert_eq!(
            job_path("/api/v1/runner/jobs/7/needs/artifacts/9").expect("one"),
            (7, JobTail::NeedOne(9))
        );
        for bad in [
            "/api/v1/runner/jobs/0/artifacts",
            "/api/v1/runner/jobs/7/needs/artifacts/x",
            "/api/v1/runner/jobs/7/needs/artifacts/9/x",
        ] {
            assert!(job_path(bad).is_err(), "{bad}");
        }
        assert!(is_upload(
            &axum::http::Method::PUT,
            "/api/v1/runner/jobs/7/artifacts"
        ));
        assert!(!is_upload(
            &axum::http::Method::GET,
            "/api/v1/runner/jobs/7/needs/artifacts"
        ));
    }

    #[test]
    fn the_upload_query_names_one_path() {
        assert_eq!(
            query_path(Some("path=target%2Fapp.tar")).expect("one"),
            "target/app.tar"
        );
        for bad in [None, Some(""), Some("path=a&path=b"), Some("other=1")] {
            assert!(query_path(bad).is_err(), "{bad:?}");
        }
    }
}
