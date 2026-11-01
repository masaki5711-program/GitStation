//! The HTTP side of a git stream (M1 phase 9 MR3): reading a Smart HTTP
//! request body under its own limits, and streaming git's stdout as the
//! response body.
//!
//! - [`BodySlots`]: a request body is read only while holding a slot,
//!   global (`max_streams`, waiting up to `acquire_timeout`, then
//!   `FG-GIT-0004 reason=body_reads`) and per user
//!   (`max_streams_per_principal`, refused at once,
//!   `reason=principal_body_reads`), so slow uploads cannot pile up
//!   buffered bodies (MR3 review).
//! - [`read_request_body`]: the whole body, at most
//!   `fetch_request_max_bytes` raw **and** decompressed (gzip through
//!   forge-git's capped decoder), with an idle timeout between chunks and
//!   one deadline (`body_timeout`, 5 min). Over a limit `FG-GIT-0013` (`limit=request_body` /
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

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use forge_auth::ProjectCtx;
use forge_core::{telemetry, ProjectRepository, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::gzip::GzipDecoder;
use forge_git::{GitStdout, GitStream};
use futures_util::StreamExt as _;
use tokio::io::AsyncReadExt as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
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
    let total = limits.body_timeout.min(limits.timeout);
    let deadline = Instant::now() + total;
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
                    .with_context(
                        "timeout_ms",
                        if kind == "total" {
                            total
                        } else {
                            limits.idle_timeout
                        }
                        .as_millis(),
                    )
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

/// A push body read chunk by chunk (M1 phase 9 MR4): decoded (gzip through
/// forge-git's capped decoder), at most `cap` bytes raw **and** decoded
/// (`FG-GIT-0013 limit=push_body`), with an idle timeout between chunks
/// and one deadline (`FG-WEB-0002 operation=git_request_body`); a client
/// that goes away is `FG-GIT-0015 phase=request_body`.
pub(crate) struct PushBody {
    chunks: axum::body::BodyDataStream,
    decoder: Option<GzipDecoder>,
    cap: u64,
    raw: u64,
    decoded: u64,
    idle: Duration,
    deadline: Instant,
    total: Duration,
    ended: bool,
}

impl PushBody {
    pub(crate) fn new(body: Body, gzip: bool, cap: u64, idle: Duration, total: Duration) -> Self {
        let decoder_cap = usize::try_from(cap).unwrap_or(usize::MAX);
        Self {
            chunks: body.into_data_stream(),
            decoder: gzip.then(|| GzipDecoder::new(decoder_cap)),
            cap,
            raw: 0,
            decoded: 0,
            idle,
            deadline: Instant::now() + total,
            total,
            ended: false,
        }
    }

    /// Bytes received so far (raw).
    pub(crate) fn raw_bytes(&self) -> u64 {
        self.raw
    }

    /// A shorter deadline from now on (the command section has its own,
    /// `body_timeout`; the pack the request's).
    pub(crate) fn set_deadline(&mut self, total: Duration, from: Instant) {
        self.deadline = from + total;
        self.total = total;
    }

    /// The next decoded bytes (possibly empty while gzip buffers), `None`
    /// at the end of the body.
    pub(crate) async fn next(&mut self) -> Result<Option<Vec<u8>>, ForgeError> {
        if self.ended {
            return Ok(None);
        }
        let now = Instant::now();
        let left = self.deadline.saturating_duration_since(now);
        let (wait, kind) = if left < self.idle {
            (left, "total")
        } else {
            (self.idle, "idle")
        };
        let next = match tokio::time::timeout(wait, self.chunks.next()).await {
            Ok(next) => next,
            Err(_) => {
                return Err(ForgeError::new(ErrorCode::RequestTimeout)
                    .with_context("operation", "git_request_body")
                    .with_context("kind", kind)
                    .with_context(
                        "timeout_ms",
                        if kind == "total" {
                            self.total
                        } else {
                            self.idle
                        }
                        .as_millis(),
                    )
                    .with_context("bytes_in", self.raw));
            }
        };
        let chunk = match next {
            None => {
                self.ended = true;
                return match self.decoder.take() {
                    Some(d) => {
                        let rest = d.finish()?;
                        self.count_decoded(rest.len())?;
                        Ok(Some(rest))
                    }
                    None => Ok(None),
                };
            }
            Some(Ok(chunk)) => chunk,
            Some(Err(e)) => {
                return Err(ForgeError::new(ErrorCode::GitClientDisconnected)
                    .with_context("reason", "client_disconnected")
                    .with_context("phase", "request_body")
                    .with_context("bytes_in", self.raw)
                    .with_source(e));
            }
        };
        self.raw = self
            .raw
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        if self.raw > self.cap {
            return Err(self.too_large());
        }
        let out = match self.decoder.as_mut() {
            Some(d) => d.push(&chunk)?,
            None => chunk.to_vec(),
        };
        self.count_decoded(out.len())?;
        Ok(Some(out))
    }

    fn count_decoded(&mut self, n: usize) -> Result<(), ForgeError> {
        self.decoded = self
            .decoded
            .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        if self.decoded > self.cap {
            return Err(self.too_large());
        }
        Ok(())
    }

    fn too_large(&self) -> ForgeError {
        ForgeError::new(ErrorCode::GitRequestTooLarge)
            .with_context("limit", "push_body")
            .with_context("max_bytes", self.cap)
            .with_context("service", "git-receive-pack")
    }

    /// Read and drop the rest of the body, at most `max` more bytes within
    /// the current deadline (so a refused client still gets its answer).
    /// `Err` says why it stopped early (the caller logs it with the refusal).
    pub(crate) async fn drain(&mut self, max: u64) -> Result<u64, ForgeError> {
        let start = self.raw;
        while let Some(_chunk) = self.next().await? {
            if self.raw - start > max {
                return Err(ForgeError::new(ErrorCode::GitRequestTooLarge)
                    .with_context("limit", "refused_push_drain")
                    .with_context("max_bytes", max));
            }
        }
        Ok(self.raw - start)
    }
}

/// The request-body read slots (see the module docs).
#[derive(Debug, Clone)]
pub(crate) struct BodySlots {
    global: Arc<Semaphore>,
    per_principal: Arc<Mutex<HashMap<String, usize>>>,
    max_per_principal: usize,
    wait: Duration,
}

/// One held body-read slot; released on drop (the permit, then the
/// per-user claim).
#[derive(Debug)]
pub(crate) struct BodySlot {
    _permit: OwnedSemaphorePermit,
    _claim: Claim,
}

impl BodySlots {
    pub(crate) fn new(max: usize, max_per_principal: usize, wait: Duration) -> Self {
        Self {
            global: Arc::new(Semaphore::new(max.max(1))),
            per_principal: Arc::new(Mutex::new(HashMap::new())),
            max_per_principal: max_per_principal.max(1),
            wait,
        }
    }

    /// A slot for `principal`: refused at once over the per-user cap,
    /// after `wait` when every slot is taken (both `FG-GIT-0004`, 503).
    pub(crate) async fn acquire(&self, principal: &str) -> Result<BodySlot, ForgeError> {
        {
            let mut map = self
                .per_principal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let held = map.entry(principal.to_owned()).or_insert(0);
            if *held >= self.max_per_principal {
                return Err(ForgeError::new(ErrorCode::GitBusy)
                    .with_context("reason", "principal_body_reads")
                    .with_context("max", self.max_per_principal));
            }
            *held += 1;
        }
        // From here the per-user claim must be returned on every path.
        let claim = Claim {
            key: principal.to_owned(),
            per_principal: Arc::clone(&self.per_principal),
        };
        let permit =
            match tokio::time::timeout(self.wait, Arc::clone(&self.global).acquire_owned()).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(e)) => {
                    return Err(ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "body_slots_closed")
                        .with_source(e))
                }
                Err(_) => {
                    return Err(ForgeError::new(ErrorCode::GitBusy)
                        .with_context("reason", "body_reads")
                        .with_context("wait_ms", self.wait.as_millis()))
                }
            };
        Ok(BodySlot {
            _permit: permit,
            _claim: claim,
        })
    }

    #[cfg(test)]
    pub(crate) fn held(&self, principal: &str) -> usize {
        self.per_principal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(principal)
            .copied()
            .unwrap_or(0)
    }
}

/// A per-user claim, returned on drop.
#[derive(Debug)]
struct Claim {
    key: String,
    per_principal: Arc<Mutex<HashMap<String, usize>>>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut map = self
            .per_principal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(held) = map.get_mut(&self.key) {
            *held = held.saturating_sub(1);
            if *held == 0 {
                map.remove(&self.key);
            }
        }
    }
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

#[cfg(test)]
#[path = "git_stream_tests.rs"]
mod tests;
