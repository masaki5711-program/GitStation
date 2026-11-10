//! A CI job's fetch of its pipeline's pinned commit over Smart HTTP (M4 C4,
//! plan 14 §4.2, ADR-0029 §3). The runner's git clones
//! `/api/v1/runner/jobs/{job_id}/repo.git` with the job's token `fgj_…`
//! (Bearer, sent through git's `http.extraHeader` from the environment,
//! never argv):
//!
//! - `GET …/repo.git/info/refs?service=git-upload-pack` (`RepoRead`): the
//!   advertisement of exactly one ref, `refs/pipelines/<pipeline_id>/head`
//!   (created at the pipeline's SHA on first use; every other ref and
//!   `HEAD` hidden: `Git::upload_pack_pinned`).
//! - `POST …/repo.git/git-upload-pack` (`RepoRead`): the RPC; every want is
//!   checked reachable from the pinned commit (`Git::check_wants_pinned`)
//!   before upload-pack starts, an unreachable one is refused in-band
//!   (`FG-GIT-0014 reason=want_unreachable`, one `ERR` packet).
//!
//! The decision is `authorize()`'s on [`PinnedCopyCtx`] (the job's
//! pipeline, its project and SHA, the project's repository, read in one
//! statement): a live job of that project at that SHA only. Another
//! project's or another commit's job is the invisible project's
//! `FG-REPO-0005`; an ended job's token (revoked with the job) is
//! `FG-AUTH-0001`; a runner, a person or a service account is refused.
//! The confidential marks are not looked at (the outputs inherit them,
//! ADR-0029 §5). Only forge-git's Smart HTTP API runs git here.

use std::sync::Arc;

use axum::body::Body;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use forge_auth::PinnedCopyCtx;
use forge_core::{telemetry, ProjectRepository, RepositoryState, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::smart_http::{advertisement_prefix, Phase, ProtocolVersion, Service};
use forge_git::upload_request::{err_packet, parse_upload_pack_request};
use forge_git::WantLimits;
use tokio::io::AsyncWriteExt as _;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::git_stream::{read_request_body, GitResponse, StreamLog};
use crate::registry::{Dispatched, LoadFuture, Loader};
use crate::routes::git::{check_content_type, git_protocol, git_response, gzip_encoded, SmartHttp};
use crate::routes::protected_refs::found;
use crate::routes::write::{absent, bounded, id_in};

/// `GET` the advertisement (`?service=git-upload-pack`).
pub const INFO_REFS: &str = "/api/v1/runner/jobs/{job_id}/repo.git/info/refs";
/// `POST` the upload-pack RPC.
pub const UPLOAD_PACK: &str = "/api/v1/runner/jobs/{job_id}/repo.git/git-upload-pack";

const JOBS_PREFIX: &str = "/api/v1/runner/jobs/";
const INFO_REFS_TAIL: &str = "/repo.git/info/refs";
const UPLOAD_PACK_TAIL: &str = "/repo.git/git-upload-pack";
const ADVERTISEMENT: &str = "application/x-git-upload-pack-advertisement";
const RESULT: &str = "application/x-git-upload-pack-result";

/// `POST …/repo.git/git-upload-pack` by the raw path: exempt from the
/// request timeout like a namespace git RPC (`app::is_git_rpc`).
pub(crate) fn is_job_upload_pack(raw_path: &str) -> bool {
    id_in(raw_path, JOBS_PREFIX, UPLOAD_PACK_TAIL).is_some()
}

/// The job in the path → [`PinnedCopyCtx`].
pub struct JobCopy {
    tail: &'static str,
}

/// `…/repo.git/info/refs`.
pub const COPY_INFO_REFS: JobCopy = JobCopy {
    tail: INFO_REFS_TAIL,
};
/// `…/repo.git/git-upload-pack`.
pub const COPY_UPLOAD_PACK: JobCopy = JobCopy {
    tail: UPLOAD_PACK_TAIL,
};

impl Loader<AppState> for JobCopy {
    type Ctx = PinnedCopyCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, PinnedCopyCtx> {
        Box::pin(async move {
            let job = id_in(parts.uri.path(), JOBS_PREFIX, self.tail)
                .ok_or_else(|| param_invalid("job_id", "not_positive_decimal"))?;
            let pipelines = state.pipelines.as_deref().ok_or_else(|| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "pipelines_unconfigured")
            })?;
            let copy = bounded(state, "runner_job_copy", pipelines.job_copy(job)).await?;
            Ok(match copy {
                Some(c) => found(PinnedCopyCtx::new(
                    c.job_id,
                    c.pipeline_id,
                    c.project,
                    c.sha,
                    c.repository,
                )),
                None => absent(),
            })
        })
    }
}

/// The Smart HTTP state and the job's repository, once it is ready.
fn ready<'a>(
    state: &'a AppState,
    ctx: &PinnedCopyCtx,
) -> Result<(&'a Arc<SmartHttp>, ProjectRepository), ForgeError> {
    let Some(smart) = state.smart_http.as_ref() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "smart_http_unconfigured")
        );
    };
    let repository = ctx.repository();
    if repository.state != RepositoryState::Ready {
        return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
            .with_context("repo_id", repository.id.get())
            .with_context("project_id", ctx.project().get())
            .with_context("state", repository.state.as_str()));
    }
    Ok((smart, repository))
}

/// The per-principal stream key of a job (it has no user).
fn job_key(job: i64) -> String {
    format!("job:{job}")
}

fn job_context(err: ForgeError, ctx: &PinnedCopyCtx, job: i64) -> ForgeError {
    err.with_context("job_id", job)
        .with_context("pipeline_id", ctx.pipeline())
        .with_context("project_id", ctx.project().get())
}

/// `refs/pipelines/<id>/head` at the pinned SHA (created when missing).
async fn pipeline_ref(
    smart: &SmartHttp,
    repository: ProjectRepository,
    ctx: &PinnedCopyCtx,
) -> Result<forge_core::RefName, ForgeError> {
    smart
        .git
        .ensure_pipeline_ref(
            &smart.storage,
            repository.id,
            ctx.pipeline(),
            ctx.sha(),
            smart.limits.want_check_timeout,
        )
        .await
}

/// `GET /api/v1/runner/jobs/{job_id}/repo.git/info/refs`.
pub async fn advertise(state: AppState, request: Dispatched<PinnedCopyCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    let job = *request.proof().id();
    let copy = request.proof().ctx().clone(); // clone: the error context outlives the request
    match advertise_inner(&state, request).await {
        Ok(res) => res,
        Err(err) => ctx
            .fail(job_context(
                err.with_context("route", "job_info_refs"),
                &copy,
                job,
            ))
            .into_response(),
    }
}

async fn advertise_inner(
    state: &AppState,
    request: Dispatched<PinnedCopyCtx>,
) -> Result<Response, ForgeError> {
    match request.raw_query() {
        Some("service=git-upload-pack") => {}
        _ => {
            return Err(ForgeError::new(ErrorCode::SmartHttpServiceUnsupported)
                .with_context("reason", "service_not_upload_pack"))
        }
    }
    let copy = request.proof().ctx();
    let (smart, repository) = ready(state, copy)?;
    let pipeline_ref = pipeline_ref(smart, repository, copy).await?;
    let protocol = git_protocol(request.headers());
    let version = ProtocolVersion::from_git_protocol(protocol.as_deref());
    let key = job_key(*request.proof().id());
    let log = StreamLog::for_job(
        request.ctx().request_id.clone(), // clone: the log outlives the request
        copy.project().get(),
        repository.id.get(),
        "job_info_refs",
    );
    let cmd = smart.git.upload_pack_pinned(
        &smart.storage,
        repository.id,
        Phase::Advertise,
        protocol.as_deref(),
        &pipeline_ref,
    )?;
    drop(request);
    let mut stream = cmd
        .idle_timeout(smart.limits.idle_timeout)
        .timeout(smart.limits.timeout)
        .spawn_stream(&key)
        .await?;
    // --advertise-refs reads nothing.
    drop(stream.take_stdin());
    let prefix = advertisement_prefix(Service::UploadPack, version);
    Ok(git_response(
        ADVERTISEMENT,
        GitResponse::new(prefix, stream, log)?.into_body(),
    ))
}

/// `POST /api/v1/runner/jobs/{job_id}/repo.git/git-upload-pack`.
pub async fn upload_pack(state: AppState, request: Dispatched<PinnedCopyCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    let job = *request.proof().id();
    let copy = request.proof().ctx().clone(); // clone: the error context outlives the request
    match upload_pack_inner(&state, request).await {
        Ok(res) => res,
        Err(err) => ctx
            .fail(job_context(
                err.with_context("route", "job_upload_pack"),
                &copy,
                job,
            ))
            .into_response(),
    }
}

async fn upload_pack_inner(
    state: &AppState,
    request: Dispatched<PinnedCopyCtx>,
) -> Result<Response, ForgeError> {
    let copy = request.proof().ctx().clone(); // clone: the request body is consumed below
    let (smart, repository) = ready(state, &copy)?;
    let smart = Arc::clone(smart);
    check_content_type(request.headers(), Service::UploadPack)?;
    let gzip = gzip_encoded(request.headers(), Service::UploadPack)?;
    let protocol = git_protocol(request.headers());
    let version = ProtocolVersion::from_git_protocol(protocol.as_deref());
    let job = *request.proof().id();
    let key = job_key(job);
    let ctx = request.ctx().clone(); // clone: the request body is consumed below
    let log = StreamLog::for_job(
        ctx.request_id.clone(), // clone: the log outlives the request
        copy.project().get(),
        repository.id.get(),
        "job_upload_pack",
    );
    let pipeline_ref = pipeline_ref(&smart, repository, &copy).await?;
    let slot = smart.body_slots.acquire(&key).await?;
    let body = read_request_body(request.into_body(), gzip, &smart.limits).await?;
    drop(slot);
    let parsed = parse_upload_pack_request(&body, version)?;
    let check = smart
        .git
        .check_wants_pinned(
            &smart.storage,
            repository.id,
            &parsed.wants,
            copy.sha(),
            &key,
            WantLimits {
                timeout: smart.limits.want_check_timeout,
                max_object_wants: smart.limits.max_object_wants,
            },
        )
        .await?;
    let check = match check.into_result() {
        Ok(check) => check,
        Err(refused) => {
            // In-band: git shows `remote error: <message>` and stops.
            let refused = job_context(
                log.context(refused.with_context("command", parsed.command.as_str())),
                &copy,
                job,
            );
            telemetry::log_failure(&refused, &ctx.request_id);
            let message = format!(
                "{} (request_id: {})",
                refused.user_message(ctx.lang),
                ctx.request_id
            );
            return Ok(git_response(RESULT, Body::from(err_packet(&message))));
        }
    };
    let mut stream = smart
        .git
        .upload_pack_pinned(
            &smart.storage,
            repository.id,
            Phase::Rpc,
            protocol.as_deref(),
            &pipeline_ref,
        )?
        .idle_timeout(smart.limits.idle_timeout)
        .timeout(smart.limits.timeout)
        .spawn_stream(&key)
        .await?;
    let Some(mut stdin) = stream.take_stdin() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "stream_stdin_taken")
        );
    };
    let mut ended = stream.ended();
    // As `routes::git::upload_pack`: write the request while the response
    // streams; a write error means git stopped reading (its exit status,
    // logged by `GitResponse`, says why).
    tokio::spawn(async move {
        tokio::select! {
            _ = async {
                if stdin.write_all(&body).await.is_ok() {
                    let _ = stdin.shutdown().await; // ignore: as above
                }
            } => {}
            () = ended.wait() => {}
        }
    });
    let log = log.with_check(parsed.command.as_str(), check.wants, check.duration);
    Ok(git_response(
        RESULT,
        GitResponse::new(Vec::new(), stream, log)?.into_body(),
    ))
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn only_a_job_upload_pack_path_is_a_git_rpc() {
        assert!(is_job_upload_pack(
            "/api/v1/runner/jobs/7/repo.git/git-upload-pack"
        ));
        for other in [
            "/api/v1/runner/jobs/7/repo.git/info/refs",
            "/api/v1/runner/jobs/0/repo.git/git-upload-pack",
            "/api/v1/runner/jobs/x/repo.git/git-upload-pack",
            "/api/v1/runner/jobs/7/8/repo.git/git-upload-pack",
            "/api/v1/runner/jobs/7/heartbeat",
        ] {
            assert!(!is_job_upload_pack(other), "{other}");
        }
    }
}
