//! Streaming git subprocesses (Smart HTTP, ADR-0007).
//!
//! [`GitCommand::spawn_stream`] starts git with piped stdin / stdout and
//! hands both to the caller ([`GitStdin`] is an `AsyncWrite` sink,
//! [`GitStdout`] an `AsyncRead`). A supervisor task owns the child and
//!
//! - captures stderr (≤ 64 KiB kept, the rest drained and dropped) for the log;
//! - kills the process tree (process group / Job Object) when no byte moved
//!   in either direction for the idle timeout (FG-GIT-0002 `kind=idle`), at
//!   the total timeout (`kind=total`), or when the stream is dropped or
//!   aborted (FG-GIT-0015);
//! - holds the stream slot and the per-principal claim until git is reaped.
//!
//! After stdout reached EOF (or stdin failed), [`GitStream::finish`] says how
//! git ended. A killed stream also ends the caller's I/O: the pipes close.

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::{oneshot, watch, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tracing::Instrument;

use crate::command::{GitCommand, MAX_STDERR_BYTES};
use crate::config::GitConfig;
use crate::process_tree::{self, ProcessTree};
use crate::redact::stderr_head;

/// The streaming pool and the per-principal counts (part of [`crate::Git`]).
#[derive(Debug, Clone)]
pub(crate) struct StreamLimits {
    slots: Arc<Semaphore>,
    per_principal: Arc<Mutex<HashMap<String, usize>>>,
}

impl StreamLimits {
    pub(crate) fn new(config: &GitConfig) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(config.max_streams.max(1))),
            per_principal: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Claim one stream for `principal`; `None` when it already holds `max`.
    fn claim(&self, principal: &str, max: usize) -> Option<PrincipalClaim> {
        let mut map = self
            .per_principal
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let count = map.entry(principal.to_owned()).or_insert(0);
        if *count >= max.max(1) {
            return None;
        }
        *count += 1;
        Some(PrincipalClaim {
            map: Arc::clone(&self.per_principal),
            key: principal.to_owned(),
        })
    }
}

/// Released (count decremented, empty entries removed) on drop.
#[derive(Debug)]
struct PrincipalClaim {
    map: Arc<Mutex<HashMap<String, usize>>>,
    key: String,
}

impl Drop for PrincipalClaim {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = map.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(&self.key);
            }
        }
    }
}

/// Byte counters and the time of the last byte in either direction.
#[derive(Debug)]
struct Activity {
    start: Instant,
    last_ms: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
}

impl Activity {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            last_ms: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
        }
    }

    fn touch(&self, counter: &AtomicU64, n: usize) {
        counter.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
        let ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.fetch_max(ms, Ordering::Relaxed);
    }

    fn last(&self) -> Instant {
        self.start + Duration::from_millis(self.last_ms.load(Ordering::Relaxed))
    }

    fn stats(&self) -> StreamStats {
        StreamStats {
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_out: self.bytes_out.load(Ordering::Relaxed),
            duration: self.start.elapsed(),
        }
    }
}

/// What a finished stream moved. Logged by the caller (`forge.git.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStats {
    /// Bytes written to git's stdin.
    pub bytes_in: u64,
    /// Bytes read from git's stdout.
    pub bytes_out: u64,
    pub duration: Duration,
}

/// git's stdin. Every written byte counts as activity.
#[derive(Debug)]
pub struct GitStdin {
    inner: ChildStdin,
    activity: Arc<Activity>,
}

impl AsyncWrite for GitStdin {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let r = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = r {
            if n > 0 {
                this.activity.touch(&this.activity.bytes_in, n);
            }
        }
        r
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// git's stdout. Every read byte counts as activity.
#[derive(Debug)]
pub struct GitStdout {
    inner: ChildStdout,
    activity: Arc<Activity>,
}

impl AsyncRead for GitStdout {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let before = buf.filled().len();
        let r = Pin::new(&mut this.inner).poll_read(cx, buf);
        let n = buf.filled().len().saturating_sub(before);
        if n > 0 {
            this.activity.touch(&this.activity.bytes_out, n);
        }
        r
    }
}

/// Resolves once git has ended (exited, or killed and reaped). For callers
/// that block on something else (e.g. reading the client's body) and must
/// stop when the stream was killed.
#[derive(Debug, Clone)]
pub struct StreamEnded(watch::Receiver<bool>);

impl StreamEnded {
    pub async fn wait(&mut self) {
        // Err: the supervisor is gone, which also means git has ended.
        let _ = self.0.wait_for(|ended| *ended).await; // ignore: both outcomes mean "ended"
    }
}

/// A running streaming git subprocess. Dropping it kills the process tree
/// (logged as FG-GIT-0015, the usual cause is a client that went away).
#[derive(Debug)]
pub struct GitStream {
    subcommand: &'static str,
    stdin: Option<GitStdin>,
    stdout: Option<GitStdout>,
    ended: watch::Receiver<bool>,
    cancel: Option<oneshot::Sender<()>>,
    result: Option<oneshot::Receiver<Ending>>,
    activity: Arc<Activity>,
    idle: Duration,
    total: Duration,
}

/// How the supervisor saw the stream end.
#[derive(Debug)]
enum End {
    Exited(std::process::ExitStatus),
    WaitFailed(io::Error),
    Idle,
    Total,
    Cancelled,
}

#[derive(Debug)]
struct Ending {
    end: End,
    stderr: Vec<u8>,
    stderr_truncated: bool,
    /// A failure to confirm git was reaped after a kill (attached to the error).
    reaped: Result<(), crate::process::ReapFailure>,
}

impl GitStream {
    /// git's stdin (once). Close it (`shutdown`) when the request is complete.
    pub fn take_stdin(&mut self) -> Option<GitStdin> {
        self.stdin.take()
    }

    /// git's stdout (once). Read to EOF, then call [`GitStream::finish`].
    pub fn take_stdout(&mut self) -> Option<GitStdout> {
        self.stdout.take()
    }

    pub fn ended(&self) -> StreamEnded {
        StreamEnded(self.ended.clone()) // clone: an independent watch receiver
    }

    /// Bytes moved so far.
    pub fn stats(&self) -> StreamStats {
        self.activity.stats()
    }

    fn fail(&self, code: ErrorCode) -> ForgeError {
        let stats = self.activity.stats();
        ForgeError::new(code)
            .with_context("subcommand", self.subcommand)
            .with_context("bytes_in", stats.bytes_in)
            .with_context("bytes_out", stats.bytes_out)
            .with_context("duration_ms", stats.duration.as_millis())
    }

    /// Wait for git to end and say how. Closes stdin / stdout if the caller
    /// still holds them here (git then sees EOF / a broken pipe).
    ///
    /// Exit 0 → [`StreamStats`]; non-zero → FG-GIT-0003 (`exit_code`,
    /// `stderr`); idle / total timeout → FG-GIT-0002 (`kind`).
    pub async fn finish(mut self) -> Result<StreamStats, ForgeError> {
        drop(self.stdin.take());
        drop(self.stdout.take());
        let Ending {
            end,
            stderr,
            stderr_truncated,
            reaped,
        } = self.ending(false).await?;
        let r = match end {
            End::Exited(status) if status.success() => Ok(self.activity.stats()),
            End::Exited(status) => Err(self
                .fail(ErrorCode::GitProcessFailed)
                .with_context(
                    "exit_code",
                    status
                        .code()
                        .map_or_else(|| "signal".to_string(), |c| c.to_string()),
                )
                .with_context("stderr", stderr_head(&stderr))
                .with_context("stderr_truncated", stderr_truncated)),
            End::WaitFailed(e) => Err(self.fail(ErrorCode::GitProcessFailed).with_source(e)),
            End::Idle => Err(self
                .fail(ErrorCode::GitTimeout)
                .with_context("kind", "idle")
                .with_context("idle_timeout_ms", self.idle.as_millis())),
            End::Total => Err(self
                .fail(ErrorCode::GitTimeout)
                .with_context("kind", "total")
                .with_context("timeout_ms", self.total.as_millis())),
            // Only `abort` cancels while the result is awaited.
            End::Cancelled => Err(self.fail(ErrorCode::GitClientDisconnected)),
        };
        r.map_err(|e| crate::process::with_reap(e, reaped))
    }

    /// The client went away: kill the process tree, wait for it to be
    /// reaped and return FG-GIT-0015 for the caller to log.
    pub async fn abort_client_disconnected(mut self) -> ForgeError {
        drop(self.stdin.take());
        drop(self.stdout.take());
        match self.ending(true).await {
            Ok(ending) => crate::process::with_reap(
                self.fail(ErrorCode::GitClientDisconnected)
                    .with_context("reason", "client_disconnected"),
                ending.reaped,
            ),
            Err(e) => e,
        }
    }

    async fn ending(&mut self, cancel: bool) -> Result<Ending, ForgeError> {
        let cancel_tx = self.cancel.take();
        if cancel {
            if let Some(tx) = cancel_tx {
                let _ = tx.send(()); // ignore: Err = the supervisor already finished
            }
        }
        let Some(rx) = self.result.take() else {
            return Err(self
                .fail(ErrorCode::Internal)
                .with_context("reason", "stream_result_taken"));
        };
        let r = rx.await;
        // `cancel_tx` (when not sent) lives until here: dropping it earlier
        // would kill git.
        match r {
            Ok(ending) => Ok(ending),
            Err(_) => Err(self
                .fail(ErrorCode::Internal)
                .with_context("reason", "stream_supervisor_lost")),
        }
    }
}

impl GitCommand {
    /// Start git as a stream (Smart HTTP). `principal` is the caller's key
    /// for the per-principal cap (e.g. `user:<id>`, or the client address for
    /// anonymous access); it is not logged.
    ///
    /// Refusals: over the per-principal cap → FG-GIT-0004
    /// `reason=principal_streams` (at once); no stream slot within
    /// `acquire_timeout` → FG-GIT-0004 `reason=streams`; spawn failure →
    /// FG-GIT-0001.
    pub async fn spawn_stream(self, principal: &str) -> Result<GitStream, ForgeError> {
        if self.option_like_value {
            return Err(self
                .fail(ErrorCode::Internal)
                .with_context("reason", "option_like_generated_value"));
        }
        if self.stdin.is_some() {
            return Err(self
                .fail(ErrorCode::Internal)
                .with_context("reason", "stdin_bytes_on_a_stream"));
        }
        let git = self.git.clone(); // clone: Arc handle
        let config = git.config();
        let Some(claim) = git
            .streams
            .claim(principal, config.max_streams_per_principal)
        else {
            return Err(self
                .fail(ErrorCode::GitBusy)
                .with_context("reason", "principal_streams")
                .with_context("max_per_principal", config.max_streams_per_principal));
        };
        let permit = match tokio::time::timeout(
            config.acquire_timeout,
            Arc::clone(&git.streams.slots).acquire_owned(),
        )
        .await
        {
            Err(_) => {
                return Err(self
                    .fail(ErrorCode::GitBusy)
                    .with_context("reason", "streams")
                    .with_context("max_streams", config.max_streams))
            }
            Ok(Err(e)) => return Err(self.fail(ErrorCode::Internal).with_source(e)),
            Ok(Ok(permit)) => permit,
        };

        let total = self.timeout.unwrap_or(config.stream_timeout_total);
        let idle = self.idle_timeout.unwrap_or(config.stream_idle_timeout);
        let mut cmd = self.build(true);
        process_tree::configure(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| {
            self.fail(ErrorCode::GitUnavailable)
                .with_context("reason", "spawn_failed")
                .with_context("git_bin", config.bin.display())
                .with_source(e)
        })?;
        let tree = match ProcessTree::attach(&child) {
            Ok(tree) => tree,
            Err(e) => {
                let reaped = crate::process::reap(&mut child).await;
                return Err(crate::process::with_reap(
                    self.fail(ErrorCode::GitUnavailable)
                        .with_context("reason", "process_tree_failed")
                        .with_source(e),
                    reaped,
                ));
            }
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let reaped = crate::process::reap(&mut child).await;
            return Err(crate::process::with_reap(
                self.fail(ErrorCode::Internal)
                    .with_context("reason", "stream_pipes_missing"),
                reaped,
            ));
        };
        let stderr = child.stderr.take();

        let activity = Arc::new(Activity::new());
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (result_tx, result_rx) = oneshot::channel();
        let (ended_tx, ended_rx) = watch::channel(false);
        let supervisor = Supervisor {
            subcommand: self.subcommand,
            child,
            tree,
            stderr,
            activity: Arc::clone(&activity),
            idle,
            deadline: activity.start + total,
            _permit: permit,
            _claim: claim,
        };
        tokio::spawn(
            supervisor
                .run(cancel_rx, result_tx, ended_tx)
                .instrument(tracing::Span::current()),
        );
        Ok(GitStream {
            subcommand: self.subcommand,
            stdin: Some(GitStdin {
                inner: stdin,
                activity: Arc::clone(&activity),
            }),
            stdout: Some(GitStdout {
                inner: stdout,
                activity: Arc::clone(&activity),
            }),
            ended: ended_rx,
            cancel: Some(cancel_tx),
            result: Some(result_rx),
            activity,
            idle,
            total,
        })
    }
}

/// Owns the child until it is reaped.
struct Supervisor {
    subcommand: &'static str,
    child: Child,
    tree: ProcessTree,
    stderr: Option<ChildStderr>,
    activity: Arc<Activity>,
    idle: Duration,
    deadline: Instant,
    _permit: OwnedSemaphorePermit,
    _claim: PrincipalClaim,
}

/// How long to keep draining stderr after git ended (a helper that outlived
/// git may hold it open).
const STDERR_DRAIN_AFTER_EXIT: Duration = Duration::from_secs(1);

impl Supervisor {
    async fn run(
        mut self,
        mut cancel: oneshot::Receiver<()>,
        result: oneshot::Sender<Ending>,
        ended: watch::Sender<bool>,
    ) {
        let mut stderr = StderrTail::default();
        let mut leader = LeaderWatch::new();
        let stop = loop {
            let wake = (self.activity.last() + self.idle).min(self.deadline);
            tokio::select! {
                exit = leader.exited(&mut self.child, &self.tree) => break Stop::Leader(exit),
                () = stderr.read(&mut self.stderr), if self.stderr.is_some() => {}
                () = tokio::time::sleep_until(wake) => {
                    let now = Instant::now();
                    if now >= self.deadline {
                        break Stop::Kill(End::Total);
                    }
                    if now.saturating_duration_since(self.activity.last()) >= self.idle {
                        break Stop::Kill(End::Idle);
                    }
                }
                _ = &mut cancel => break Stop::Kill(End::Cancelled),
            }
        };
        let (end, reaped) = match stop {
            Stop::Leader(LeaderExit::Status(status)) => {
                // Already reaped by `wait` (Windows, or the Unix fallback).
                self.tree.release();
                (end_of(status), Ok(()))
            }
            Stop::Leader(LeaderExit::Zombie { success }) => {
                if success {
                    self.tree.release();
                } else {
                    // The leader is a zombie: its id is still taken, so the
                    // group kill cannot hit another process. Helpers it left
                    // behind would otherwise keep running and holding locks.
                    self.tree.kill();
                }
                match tokio::time::timeout(LEADER_REAP_TIMEOUT, self.child.wait()).await {
                    Ok(status) => (end_of(status), Ok(())),
                    Err(_) => {
                        let reaped = crate::process::reap(&mut self.child).await;
                        (
                            End::WaitFailed(io::Error::other("zombie leader not reaped")),
                            reaped,
                        )
                    }
                }
            }
            Stop::Kill(end) => {
                // Group first, while the leader is unreaped (its id cannot be reused).
                self.tree.kill();
                (end, crate::process::reap(&mut self.child).await)
            }
        };
        let _ = tokio::time::timeout(STDERR_DRAIN_AFTER_EXIT, async {
            while self.stderr.is_some() {
                stderr.read(&mut self.stderr).await;
            }
        })
        .await; // ignore: a helper holding stderr open only shortens the log tail
        let _ = ended.send(true); // ignore: no StreamEnded receiver left is fine
        let cancelled = matches!(end, End::Cancelled);
        let ending = Ending {
            end,
            stderr: stderr.buf,
            stderr_truncated: stderr.truncated,
            reaped,
        };
        if let Err(unsent) = result.send(ending) {
            if !cancelled {
                return; // the stream ended by itself; the caller just stopped waiting
            }
            let send_failed_reap = unsent.reaped;
            // Nobody awaits the result: the stream was dropped (the request
            // future was cancelled — usually the client went away). This is
            // the only place that knows, so it logs.
            let stats = self.activity.stats();
            let err = crate::process::with_reap(
                ForgeError::new(ErrorCode::GitClientDisconnected)
                    .with_context("subcommand", self.subcommand)
                    .with_context("reason", "dropped")
                    .with_context("bytes_in", stats.bytes_in)
                    .with_context("bytes_out", stats.bytes_out)
                    .with_context("duration_ms", stats.duration.as_millis()),
                send_failed_reap,
            );
            tracing::info!(
                target: "forge_git::stream",
                error_code = err.code().id(),
                error = %err,
                "git.stream.aborted"
            );
        }
    }
}

/// Why the supervisor loop stopped.
enum Stop {
    Leader(LeaderExit),
    Kill(End),
}

/// How the leader was seen to exit.
enum LeaderExit {
    /// Exited, not yet reaped (Unix, `waitid(WNOWAIT)`).
    #[cfg_attr(windows, expect(dead_code, reason = "Windows waits with a plain wait"))]
    Zombie { success: bool },
    /// Exited and reaped by `wait`.
    Status(io::Result<std::process::ExitStatus>),
}

fn end_of(status: io::Result<std::process::ExitStatus>) -> End {
    match status {
        Ok(status) => End::Exited(status),
        Err(e) => End::WaitFailed(e),
    }
}

/// Bounds the wait for a leader already seen as a zombie.
const LEADER_REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Notices the leader's exit without reaping it (Unix: SIGCHLD or a 100 ms
/// tick, then `waitid(WNOWAIT)`); Windows waits normally.
struct LeaderWatch {
    #[cfg(unix)]
    sigchld: Option<tokio::signal::unix::Signal>,
}

impl LeaderWatch {
    fn new() -> Self {
        Self {
            // Without the signal stream the 100 ms tick still notices the exit.
            #[cfg(unix)]
            sigchld: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).ok(),
        }
    }

    /// Cancel-safe: holds no state across awaits.
    async fn exited(&mut self, child: &mut Child, tree: &ProcessTree) -> LeaderExit {
        #[cfg(unix)]
        loop {
            match tree.peek_leader() {
                Ok(Some(success)) => return LeaderExit::Zombie { success },
                Ok(None) => {}
                // No pid to watch or waitid refused: fall back to a plain
                // wait (the failure-path group kill is then skipped).
                Err(_) => return LeaderExit::Status(child.wait().await),
            }
            let tick = tokio::time::sleep(Duration::from_millis(100));
            match self.sigchld.as_mut() {
                Some(sig) => {
                    tokio::select! {
                        _ = sig.recv() => {}
                        () = tick => {}
                    }
                }
                None => tick.await,
            }
        }
        #[cfg(windows)]
        {
            let _ = tree;
            LeaderExit::Status(child.wait().await)
        }
    }
}

/// The first [`MAX_STDERR_BYTES`] of stderr; the rest is read and dropped so
/// git never blocks on a full stderr pipe.
#[derive(Default)]
struct StderrTail {
    buf: Vec<u8>,
    truncated: bool,
}

impl StderrTail {
    /// One read; sets `pipe` to `None` at EOF or on a read error.
    async fn read(&mut self, pipe: &mut Option<ChildStderr>) {
        let Some(p) = pipe.as_mut() else {
            return;
        };
        let mut chunk = [0u8; 8192];
        match p.read(&mut chunk).await {
            Ok(0) | Err(_) => *pipe = None, // EOF; a read error only ends the log tail
            Ok(n) => {
                let room = MAX_STDERR_BYTES.saturating_sub(self.buf.len());
                let keep = n.min(room);
                self.buf
                    .extend_from_slice(chunk.get(..keep).unwrap_or_default());
                if keep < n {
                    self.truncated = true;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
