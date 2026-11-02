//! Smart HTTP fetch (M1 phase 9 MR3, ADR-0007, ADR-0015 "git"): clone and
//! fetch over `{path}.git`. The dispatcher has authenticated, resolved and
//! proved `RepoRead` before a handler here runs; these handlers run git
//! **only through forge-git's Smart HTTP API** (`Git::upload_pack`,
//! `Git::check_wants`), never the generic command builder (the test
//! `forge_web_runs_git_only_through_smart_http_api`).
//!
//! - `GET|HEAD {path}.git/info/refs?service=git-upload-pack`: the
//!   advertisement (`# service=` prefix for v0 / v1, none for v2). `HEAD`
//!   answers the headers only and starts no git. The receive-pack
//!   advertisement (`service=git-receive-pack`, `RepoPush`, M1 phase 9
//!   MR4) is the same handler with the other service (always the prefix:
//!   receive-pack has no v2); the push itself is `routes::git_push`.
//! - `POST {path}.git/git-upload-pack`: the request body (Content-Type
//!   checked; gzip or identity; at most `fetch_request_max_bytes` raw and
//!   decompressed; idle timeout and `body_timeout`; one body-read slot,
//!   global and per user) is read completely, parsed
//!   (`want`s, v2 command), and **every want is checked to be reachable from
//!   a ref** (ADR-0007 option (a)) before upload-pack starts. An
//!   unreachable want is refused in-band: HTTP 200 with one `ERR` packet
//!   (`FG-GIT-0014 reason=want_unreachable`, logged once), so git prints
//!   `remote error:`. Exempt from the request timeout (`app.rs`): the
//!   route's own bounds are `git.http.*`.
//!
//! Both: a repository still `provisioning` is `FG-REPO-0016` (after
//! `authorize()`); responses carry `Cache-Control: private, no-store` and
//! `Vary: Authorization`; git's output is streamed (`GitResponse`), and a
//! client that goes away kills git (`FG-GIT-0015`, logged with the request
//! id). The per-principal stream cap is keyed by the user id.

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use forge_auth::ProjectCtx;
use forge_core::config::GitHttpConfig;
use forge_core::{telemetry, ProjectRepository, ProtectionLoader, PushRecorder, RepositoryState};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::smart_http::{advertisement_prefix, Phase, ProtocolVersion, Service};
use forge_git::upload_request::{err_packet, parse_upload_pack_request};
use forge_git::{Git, StorageRoot, WantLimits};
use tokio::io::AsyncWriteExt as _;

use crate::app::AppState;
use crate::git_stream::{read_request_body, BodySlots, GitResponse, StreamLog};
use crate::registry::Dispatched;

const ADVERTISEMENT: &str = "application/x-git-upload-pack-advertisement";
const REQUEST: &str = "application/x-git-upload-pack-request";
const RESULT: &str = "application/x-git-upload-pack-result";
const RECEIVE_ADVERTISEMENT: &str = "application/x-git-receive-pack-advertisement";
const RECEIVE_REQUEST: &str = "application/x-git-receive-pack-request";
pub(crate) const RECEIVE_RESULT: &str = "application/x-git-receive-pack-result";

/// The Smart HTTP limits (`git.http.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitHttpLimits {
    /// One upload-pack request body, raw and decompressed.
    pub fetch_request_max_bytes: usize,
    /// No byte in either direction for this long ends the request.
    pub idle_timeout: Duration,
    /// One git POST, first byte to the end of the response; git's stream.
    pub timeout: Duration,
    /// Reading one request body completely (a slow client holds a
    /// body-read slot at most this long).
    pub body_timeout: Duration,
    /// The want reachability check.
    pub want_check_timeout: Duration,
    /// Distinct non-commit wants per upload-pack request.
    pub max_object_wants: usize,
    /// One push's pack (`receive.maxInputSize`); the body forge forwards
    /// may be this plus the command list (M1 phase 9 MR4).
    pub push_max_bytes: NonZeroU64,
}

impl GitHttpLimits {
    pub fn from_config(cfg: &GitHttpConfig) -> Self {
        Self {
            // u32 → usize: lossless on the supported (32/64-bit) targets.
            fetch_request_max_bytes: usize::try_from(cfg.fetch_request_max_bytes)
                .unwrap_or(usize::MAX),
            idle_timeout: cfg.idle_timeout,
            timeout: cfg.timeout,
            body_timeout: cfg.body_timeout,
            want_check_timeout: cfg.want_check_timeout,
            max_object_wants: usize::try_from(cfg.max_object_wants).unwrap_or(usize::MAX),
            // `git.http.push_max_bytes` is validated positive at load time;
            // a zero would mean "unlimited" to git, so it falls back to 1.
            push_max_bytes: NonZeroU64::new(u64::from(cfg.push_max_bytes))
                .unwrap_or(NonZeroU64::MIN),
        }
    }
}

impl Default for GitHttpLimits {
    fn default() -> Self {
        Self::from_config(&GitHttpConfig::DEFAULT)
    }
}

/// What the Smart HTTP routes run git with: the git handle (stream pools),
/// the storage root (paths from `RepoId` only, ADR-0009), the limits, and
/// the request-body read slots (sized like git's stream pools).
#[derive(Clone)]
pub struct SmartHttp {
    pub git: Git,
    pub storage: StorageRoot,
    pub limits: GitHttpLimits,
    pub(crate) body_slots: BodySlots,
    /// What a push needs besides git (`forge serve`: the database). Without
    /// it receive-pack answers `FG-SYS-0001 reason=push_unconfigured`.
    pub(crate) push: Option<PushServices>,
    /// The detached receive-pack tasks still running (graceful shutdown
    /// waits for them: [`SmartHttp::push_tasks`]).
    pub(crate) push_tasks: PushTasks,
    /// The browse API and the raw / archive routes (M2 MR7): the same git
    /// and storage, the browse limits, the per-repository cap and the ref
    /// cache every push drops.
    pub browse: Browser,
}

/// Counts running push tasks (a receive-pack the request no longer owns,
/// and its record). `forge serve` waits for them after the server stopped
/// (`wait_idle`), so a shutdown does not kill git mid ref-update or drop a
/// record.
#[derive(Debug, Clone, Default)]
pub struct PushTasks(Arc<(AtomicUsize, Notify)>);

/// One running push task; done on drop.
#[derive(Debug)]
pub(crate) struct PushTask(Arc<(AtomicUsize, Notify)>);

impl Drop for PushTask {
    fn drop(&mut self) {
        self.0 .0.fetch_sub(1, Ordering::SeqCst);
        self.0 .1.notify_waiters();
    }
}

impl PushTasks {
    pub(crate) fn start(&self) -> PushTask {
        self.0 .0.fetch_add(1, Ordering::SeqCst);
        PushTask(Arc::clone(&self.0))
    }

    /// Running tasks now.
    pub fn running(&self) -> usize {
        self.0 .0.load(Ordering::SeqCst)
    }

    /// Wait until no task runs, at most `timeout`; `false` = some still run.
    pub async fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.0 .1.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.running() == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.running() == 0;
            }
        }
    }
}

/// The push's protection snapshot and its recorder (forge-core traits;
/// forge-db implements both on `Db`).
#[derive(Clone)]
pub(crate) struct PushServices {
    pub(crate) protection: Arc<dyn ProtectionLoader>,
    pub(crate) recorder: Arc<dyn PushRecorder>,
}

impl std::fmt::Debug for SmartHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmartHttp")
            .field("git", &self.git)
            .field("storage", &self.storage)
            .field("limits", &self.limits)
            .field("push", &self.push.is_some())
            .finish_non_exhaustive()
    }
}

impl SmartHttp {
    /// The body-read slots take git's stream limits: `max_streams` in
    /// total, `max_streams_per_principal` per user, waiting at most
    /// `acquire_timeout` for a free one.
    pub fn new(git: Git, storage: StorageRoot, limits: GitHttpLimits) -> Self {
        let cfg = git.config();
        let body_slots = BodySlots::new(
            cfg.max_streams,
            cfg.max_streams_per_principal,
            cfg.acquire_timeout,
        );
        let browse = Browser::new(
            git.clone(),     // clone: a shared handle (pools, config)
            storage.clone(), // clone: a path
            BrowseLimits::default(),
        );
        Self {
            git,
            storage,
            limits,
            body_slots,
            push: None,
            push_tasks: PushTasks::default(),
            browse,
        }
    }

    /// Other browse limits (tests; `forge serve` keeps the defaults).
    #[must_use]
    pub fn with_browse_limits(mut self, limits: BrowseLimits) -> Self {
        self.browse = Browser::new(
            self.git.clone(),     // clone: a shared handle
            self.storage.clone(), // clone: a path
            limits,
        );
        self
    }

    /// The running push tasks (`forge serve` waits for them on shutdown).
    pub fn push_tasks(&self) -> PushTasks {
        self.push_tasks.clone() // clone: a shared counter
    }

    /// Enable pushes: the protection snapshot of each push is loaded from
    /// `protection`, applied pushes are recorded with `recorder`.
    #[must_use]
    pub fn with_push(
        mut self,
        protection: Arc<dyn ProtectionLoader>,
        recorder: Arc<dyn PushRecorder>,
    ) -> Self {
        self.push = Some(PushServices {
            protection,
            recorder,
        });
        self
    }
}

/// `GET|HEAD {path}.git/info/refs?service=git-upload-pack`.
pub async fn advertise(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    advertise_service(state, request, Service::UploadPack).await
}

/// `GET|HEAD {path}.git/info/refs?service=git-receive-pack` (`RepoPush`,
/// M1 phase 9 MR4).
pub async fn advertise_receive_pack(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    advertise_service(state, request, Service::ReceivePack).await
}

async fn advertise_service(
    state: AppState,
    request: Dispatched<ProjectCtx>,
    service: Service,
) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    match advertise_inner(&state, request, service).await {
        Ok(res) => res,
        Err(err) => ctx
            .fail(
                err.with_context("route", "info_refs")
                    .with_context("service", service.name()),
            )
            .into_response(),
    }
}

async fn advertise_inner(
    state: &AppState,
    request: Dispatched<ProjectCtx>,
    service: Service,
) -> Result<Response, ForgeError> {
    let (smart, repository) = ready(state, &request)?;
    let content_type = match service {
        Service::UploadPack => ADVERTISEMENT,
        Service::ReceivePack => RECEIVE_ADVERTISEMENT,
    };
    let mut res = git_response(content_type, Body::empty());
    if *request.method() == Method::HEAD {
        return Ok(res);
    }
    let protocol = git_protocol(request.headers());
    let version = ProtocolVersion::from_git_protocol(protocol.as_deref());
    let principal = principal_key(&request)?;
    let log = StreamLog::new(&request, repository, "info_refs");
    drop(request);
    let cmd = match service {
        Service::UploadPack => smart.git.upload_pack(
            &smart.storage,
            repository.id,
            Phase::Advertise,
            protocol.as_deref(),
        )?,
        Service::ReceivePack => smart.git.receive_pack(
            &smart.storage,
            repository.id,
            Phase::Advertise,
            protocol.as_deref(),
            smart.limits.push_max_bytes,
        )?,
    };
    let mut stream = cmd
        .idle_timeout(smart.limits.idle_timeout)
        .timeout(smart.limits.timeout)
        .spawn_stream(&principal)
        .await?;
    // --advertise-refs reads nothing.
    drop(stream.take_stdin());
    let prefix = advertisement_prefix(service, version);
    *res.body_mut() = GitResponse::new(prefix, stream, log)?.into_body();
    Ok(res)
}

/// `POST {path}.git/git-upload-pack`.
pub async fn upload_pack(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    match upload_pack_inner(&state, request).await {
        Ok(res) => res,
        Err(err) => ctx
            .fail(err.with_context("route", "upload_pack"))
            .into_response(),
    }
}

async fn upload_pack_inner(
    state: &AppState,
    request: Dispatched<ProjectCtx>,
) -> Result<Response, ForgeError> {
    let (smart, repository) = ready(state, &request)?;
    let smart = Arc::clone(smart);
    check_content_type(request.headers(), Service::UploadPack)?;
    let gzip = gzip_encoded(request.headers(), Service::UploadPack)?;
    let protocol = git_protocol(request.headers());
    let version = ProtocolVersion::from_git_protocol(protocol.as_deref());
    let principal = principal_key(&request)?;
    let log = StreamLog::new(&request, repository, "upload_pack");
    let ctx = request.ctx().clone(); // clone: the request body is consumed below
    let slot = smart.body_slots.acquire(&principal).await?;
    let body = read_request_body(request.into_body(), gzip, &smart.limits).await?;
    drop(slot);
    let parsed = parse_upload_pack_request(&body, version)?;
    let check = smart
        .git
        .check_wants(
            &smart.storage,
            repository.id,
            &parsed.wants,
            &principal,
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
            let refused = log.context(refused.with_context("command", parsed.command.as_str()));
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
        .upload_pack(
            &smart.storage,
            repository.id,
            Phase::Rpc,
            protocol.as_deref(),
        )?
        .idle_timeout(smart.limits.idle_timeout)
        .timeout(smart.limits.timeout)
        .spawn_stream(&principal)
        .await?;
    let Some(mut stdin) = stream.take_stdin() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "stream_stdin_taken")
        );
    };
    let mut ended = stream.ended();
    // Write the request while the response streams (git may answer before
    // it has read everything); stop when git ended. A write error means git
    // stopped reading: its exit status (logged by `GitResponse`) says why.
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

/// The Smart HTTP state and the repository, once it is ready
/// (`FG-REPO-0016` while provisioning; decided after `authorize()`).
pub(crate) fn ready<'a>(
    state: &'a AppState,
    request: &Dispatched<ProjectCtx>,
) -> Result<(&'a Arc<SmartHttp>, ProjectRepository), ForgeError> {
    let Some(smart) = state.smart_http.as_ref() else {
        // `forge serve` always configures it; a test state may not.
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "smart_http_unconfigured")
        );
    };
    let Some(repository) = request.repository() else {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "git_route_without_repository"));
    };
    if repository.state != RepositoryState::Ready {
        return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
            .with_context("repo_id", repository.id.get())
            .with_context("project_id", request.proof().id().get())
            .with_context("state", repository.state.as_str()));
    }
    Ok((smart, repository))
}

/// The per-principal stream key: the user id (Smart HTTP needs an
/// authenticated user; the dispatcher refused anonymous callers).
pub(crate) fn principal_key(request: &Dispatched<ProjectCtx>) -> Result<String, ForgeError> {
    request
        .principal()
        .user()
        .map(|user| format!("user:{}", user.get()))
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "git_route_without_user")
        })
}

/// The raw `Git-Protocol` header (forge-git keeps only a safe value).
pub(crate) fn git_protocol(headers: &HeaderMap) -> Option<String> {
    headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// `Content-Type: application/x-git-<service>-request` (parameters and
/// case ignored), else `FG-GIT-0014 reason=content_type`.
pub(crate) fn check_content_type(headers: &HeaderMap, service: Service) -> Result<(), ForgeError> {
    let expected = match service {
        Service::UploadPack => REQUEST,
        Service::ReceivePack => RECEIVE_REQUEST,
    };
    let media = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::trim);
    if media.is_some_and(|m| m.eq_ignore_ascii_case(expected)) {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::GitProtocolInvalid)
        .with_context("reason", "content_type")
        .with_context("service", service.name()))
}

/// `true` for `Content-Encoding: gzip` (git's only one), `false` without
/// one or for `identity`; anything else is `FG-GIT-0014
/// reason=content_encoding`.
pub(crate) fn gzip_encoded(headers: &HeaderMap, service: Service) -> Result<bool, ForgeError> {
    let mut values = headers.get_all(CONTENT_ENCODING).iter();
    let Some(first) = values.next() else {
        return Ok(false);
    };
    let value = first.to_str().ok().map(str::trim);
    match value {
        _ if values.next().is_some() => {}
        Some(v) if v.eq_ignore_ascii_case("gzip") || v.eq_ignore_ascii_case("x-gzip") => {
            return Ok(true)
        }
        Some(v) if v.eq_ignore_ascii_case("identity") => return Ok(false),
        _ => {}
    }
    Err(ForgeError::new(ErrorCode::GitProtocolInvalid)
        .with_context("reason", "content_encoding")
        .with_context("service", service.name()))
}

/// A Smart HTTP response: 200, the content type, and never cached by a
/// shared cache (ADR-0015 "応答の安全").
pub(crate) fn git_response(content_type: &'static str, body: Body) -> Response {
    let mut res = (StatusCode::OK, body).into_response();
    let headers = res.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(VARY, HeaderValue::from_static("Authorization"));
    res
}
