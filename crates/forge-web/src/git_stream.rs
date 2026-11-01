//! The HTTP side of a git stream (M1 phase 9 MR3): reading a Smart HTTP
//! request body under its own limits, and streaming git's stdout as the
//! response body.
//!
//! - [`read_request_body`]: the whole body, at most
//!   `fetch_request_max_bytes` raw **and** decompressed (gzip through
//!   forge-git's capped decoder), with an idle timeout between chunks and
//!   one total deadline. Over a limit `FG-GIT-0013` (`limit=request_body` /
//!   `gzip_decompressed`), too slow `FG-WEB-0002`
//!   (`operation=git_request_body`, `kind=idle|total`), a client that goes
//!   away `FG-GIT-0015` (`reason=client_disconnected`,
//!   `phase=request_body`; no git was started).
//! - [`GitResponse`]: an optional prefix, then git's stdout as it comes.
//!   At EOF it waits for git (`GitStream::finish`) and logs the outcome
//!   once: `forge.git.<route>` on success, the error code otherwise (the
//!   response is then cut, so the client sees a failed transfer, never a
//!   silently short one). Dropped before the end (the client went away):
//!   git is killed with `abort_client_disconnected()` and `FG-GIT-0015` is
//!   logged with the request id (status 499 in the code; no response is
//!   left to send).

use std::io;
use std::time::Duration;

use axum::body::{Body, Bytes};
use forge_auth::ProjectCtx;
use forge_core::{telemetry, ProjectRepository, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::gzip::GzipDecoder;
use forge_git::{GitStdout, GitStream};
use futures_util::StreamExt as _;
use tokio::io::AsyncReadExt as _;
use tokio::time::Instant;

use crate::registry::Dispatched;
use crate::routes::git::GitHttpLimits;

/// The bytes read from git's stdout per response chunk.
const CHUNK: usize = 64 * 1024;

/// Read a Smart HTTP request body (see the module docs). `gzip`: the body
/// is `Content-Encoding: gzip`; the result is decompressed.
pub(crate) async fn read_request_body(
    body: Body,
    gzip: bool,
    limits: &GitHttpLimits,
) -> Result<Vec<u8>, ForgeError> {
    let cap = limits.fetch_request_max_bytes;
    let deadline = Instant::now() + limits.timeout;
    let mut chunks = body.into_data_stream();
    let mut decoder = gzip.then(|| GzipDecoder::new(cap));
    let mut raw = 0usize;
    let mut out = Vec::new();
    loop {
        let now = Instant::now();
        let (wait, kind) = if deadline.saturating_duration_since(now) < limits.idle_timeout {
            (deadline.saturating_duration_since(now), "total")
        } else {
            (limits.idle_timeout, "idle")
        };
        let next = match tokio::time::timeout(wait, chunks.next()).await {
            Ok(next) => next,
            Err(_) => {
                return Err(ForgeError::new(ErrorCode::RequestTimeout)
                    .with_context("operation", "git_request_body")
                    .with_context("kind", kind)
                    .with_context("timeout_ms", wait.as_millis())
                    .with_context("bytes_in", raw));
            }
        };
        let chunk = match next {
            None => break,
            Some(Ok(chunk)) => chunk,
            Some(Err(e)) => {
                return Err(ForgeError::new(ErrorCode::GitClientDisconnected)
                    .with_context("reason", "client_disconnected")
                    .with_context("phase", "request_body")
                    .with_context("bytes_in", raw)
                    .with_source(e));
            }
        };
        raw = raw.saturating_add(chunk.len());
        if raw > cap {
            return Err(ForgeError::new(ErrorCode::GitRequestTooLarge)
                .with_context("limit", "request_body")
                .with_context("max_bytes", cap));
        }
        match decoder.as_mut() {
            Some(d) => out.extend(d.push(&chunk)?),
            None => out.extend_from_slice(&chunk),
        }
    }
    if let Some(d) = decoder {
        out.extend(d.finish()?);
    }
    Ok(out)
}

/// What a stream's outcome is logged with: the request and the target.
#[derive(Debug, Clone)]
pub(crate) struct StreamLog {
    request_id: RequestId,
    route: &'static str,
    project_id: i64,
    repo_id: i64,
    command: Option<&'static str>,
    wants: Option<usize>,
    check_ms: Option<u128>,
}

impl StreamLog {
    pub(crate) fn new(
        request: &Dispatched<ProjectCtx>,
        repository: ProjectRepository,
        route: &'static str,
    ) -> Self {
        Self {
            request_id: request.ctx().request_id.clone(), // clone: the log outlives the request
            route,
            project_id: request.proof().id().get(),
            repo_id: repository.id.get(),
            command: None,
            wants: None,
            check_ms: None,
        }
    }

    /// The upload-pack request's command and want check.
    pub(crate) fn with_check(
        mut self,
        command: &'static str,
        wants: usize,
        check: Duration,
    ) -> Self {
        self.command = Some(command);
        self.wants = Some(wants);
        self.check_ms = Some(check.as_millis());
        self
    }

    /// The target as error context.
    pub(crate) fn context(&self, err: ForgeError) -> ForgeError {
        let err = err
            .with_context("route", self.route)
            .with_context("project_id", self.project_id)
            .with_context("repo_id", self.repo_id);
        match self.command {
            Some(command) => err.with_context("command", command),
            None => err,
        }
    }

    fn failure(&self, err: ForgeError) {
        telemetry::log_failure(&self.context(err), &self.request_id);
    }

    fn success(&self, stats: forge_git::StreamStats) {
        tracing::info!(
            target: "forge_web::git",
            request_id = %self.request_id,
            route = self.route,
            project_id = self.project_id,
            repo_id = self.repo_id,
            command = self.command.unwrap_or(""),
            wants = self.wants.unwrap_or(0),
            check_ms = self.check_ms.unwrap_or(0),
            bytes_in = stats.bytes_in,
            bytes_out = stats.bytes_out,
            duration_ms = stats.duration.as_millis(),
            "forge.git.served"
        );
    }
}

/// git's stdout as a response body (see the module docs).
pub(crate) struct GitResponse {
    prefix: Option<Bytes>,
    stdout: Option<GitStdout>,
    stream: Option<GitStream>,
    log: StreamLog,
}

impl GitResponse {
    pub(crate) fn new(
        prefix: Vec<u8>,
        mut stream: GitStream,
        log: StreamLog,
    ) -> Result<Self, ForgeError> {
        let Some(stdout) = stream.take_stdout() else {
            return Err(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "stream_stdout_taken")
            );
        };
        Ok(Self {
            prefix: (!prefix.is_empty()).then(|| Bytes::from(prefix)),
            stdout: Some(stdout),
            stream: Some(stream),
            log,
        })
    }

    pub(crate) fn into_body(self) -> Body {
        Body::from_stream(futures_util::stream::unfold(self, Self::next))
    }

    async fn next(mut self) -> Option<(Result<Bytes, io::Error>, Self)> {
        if let Some(prefix) = self.prefix.take() {
            return Some((Ok(prefix), self));
        }
        let stdout = self.stdout.as_mut()?;
        let mut buf = vec![0; CHUNK];
        let read = stdout.read(&mut buf).await;
        match read {
            Ok(n) if n > 0 => {
                buf.truncate(n);
                Some((Ok(Bytes::from(buf)), self))
            }
            Ok(_) => {
                // EOF: git closed its stdout; how did it end?
                self.stdout = None;
                let stream = self.stream.take()?;
                match stream.finish().await {
                    Ok(stats) => {
                        self.log.success(stats);
                        None
                    }
                    Err(err) => {
                        self.log.failure(err);
                        Some((Err(io::Error::other("git failed")), self))
                    }
                }
            }
            Err(e) => {
                self.stdout = None;
                let stream = self.stream.take()?;
                let err = match stream.finish().await {
                    Err(err) => err,
                    Ok(_) => ForgeError::new(ErrorCode::GitProcessFailed)
                        .with_context("reason", "stdout_read_failed")
                        .with_source(e),
                };
                self.log.failure(err);
                Some((Err(io::Error::other("git failed")), self))
            }
        }
    }
}

impl Drop for GitResponse {
    /// The body was dropped before git ended: the client went away (or the
    /// server is shutting down). Kill git now and log `FG-GIT-0015` with the
    /// request id. Without a runtime (never in `forge serve`) the stream's
    /// own drop kills git and logs `reason=dropped`.
    fn drop(&mut self) {
        drop(self.stdout.take());
        let Some(stream) = self.stream.take() else {
            return;
        };
        let log = self.log.clone(); // clone: moves into the task
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let err = stream.abort_client_disconnected().await;
                log.failure(err);
            });
        }
    }
}
