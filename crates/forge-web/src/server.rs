//! Binding and serving (`forge serve`).
//!
//! The connection loop is built on hyper directly (not `axum::serve`) so
//! that every connection is bounded (security review, slowloris; M1 phase 9
//! MR0, connection fairness):
//! - a header-read timeout (also bounds idle keep-alive connections),
//! - a maximum number of concurrent connections, in total and per socket
//!   peer address; a connection over either cap is answered `FG-WEB-0008`
//!   (503, `Retry-After`) and closed, never queued,
//! - a keep-alive lifetime and request count: the connection is closed after
//!   the request it is serving (a running request, e.g. a long git stream, is
//!   never cut by them),
//! - a response-write idle timeout: a write that makes no progress (the
//!   client does not read) closes the connection. Only a stalled socket
//!   counts, so a git stream that keeps sending is not idle,
//! - a graceful-shutdown deadline.
//!
//! The peer is the socket peer address. There is no trusted-proxy setting:
//! behind a reverse proxy every client shares the proxy's address, so
//! `max_connections_per_peer` then caps the proxy as a whole.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::http::header::{HeaderValue, CONNECTION, CONTENT_LANGUAGE, CONTENT_TYPE, RETRY_AFTER};
use axum::http::Request;
use axum::Router;
use forge_core::config::ServerLimitsConfig;
use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError, Lang};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::time::Sleep;
use tower::ServiceExt;

/// Limits of the HTTP server.
#[derive(Debug, Clone, Copy)]
pub struct ServerLimits {
    /// Time a client has to send the request headers (and the idle time
    /// allowed between keep-alive requests).
    pub header_read_timeout: Duration,
    /// Concurrent connections in total; further clients get `FG-WEB-0008`.
    pub max_connections: usize,
    /// Concurrent connections from one socket peer address.
    pub max_connections_per_peer: usize,
    /// A connection older than this is closed after its current request.
    pub keep_alive_max: Duration,
    /// Requests served on one connection; the last answer says `close`.
    pub keep_alive_max_requests: u32,
    /// A response write without progress for this long closes the connection.
    pub write_idle_timeout: Duration,
    /// How long in-flight requests may run after shutdown is requested.
    pub shutdown_grace: Duration,
}

impl ServerLimits {
    /// The limits from the validated configuration (`server.*`).
    pub fn from_config(cfg: &ServerLimitsConfig) -> Self {
        Self {
            header_read_timeout: cfg.header_read_timeout,
            // u32 → usize: lossless on the supported (32/64-bit) targets.
            max_connections: usize::try_from(cfg.max_connections).unwrap_or(usize::MAX),
            max_connections_per_peer: usize::try_from(cfg.max_connections_per_peer)
                .unwrap_or(usize::MAX),
            keep_alive_max: cfg.keep_alive_max,
            keep_alive_max_requests: cfg.keep_alive_max_requests,
            write_idle_timeout: cfg.write_idle_timeout,
            shutdown_grace: Duration::from_secs(30),
        }
    }
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self::from_config(&ServerLimitsConfig::DEFAULT)
    }
}

/// `Retry-After` of a refused connection, in seconds.
const BUSY_RETRY_AFTER_SECS: u64 = 5;
/// Refusals answered at the same time; beyond it the connection is just
/// closed (a flood must not cost a task per connection).
const MAX_CONCURRENT_REFUSALS: usize = 64;
/// How long a refused client gets to receive its 503.
const REFUSAL_DEADLINE: Duration = Duration::from_secs(2);
/// A refused client's request bytes read (and dropped) before closing, so
/// the close does not reset the connection before the 503 is read.
const REFUSAL_DRAIN_BYTES: usize = 16 * 1024;
/// At most one refusal log line per this interval (`suppressed` counts the
/// rest), so a flood cannot flood the log.
const REFUSAL_LOG_INTERVAL: Duration = Duration::from_secs(1);

/// Bind the listen address; failure is `FG-SYS-0004` with the OS error.
pub async fn bind(addr: &str) -> Result<TcpListener, ForgeError> {
    let listener = TcpListener::bind(addr).await.map_err(|e| {
        ForgeError::new(ErrorCode::ServerBindFailed)
            .with_context("bind", addr)
            .with_source(e)
    })?;
    if let Ok(local) = listener.local_addr() {
        if !local.ip().is_loopback() {
            // Not a failure: a reminder that TLS is the reverse proxy's job.
            tracing::warn!(
                target: "forge_web::server",
                bind = %local,
                "listening on a non-loopback address: put the TLS reverse proxy in front"
            );
        }
    }
    Ok(listener)
}

fn serve_failed(operation: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ServeFailed).with_context("operation", operation)
}

/// Open connections per peer address.
struct PeerTable {
    counts: Mutex<HashMap<IpAddr, usize>>,
    limit: usize,
}

/// One connection of a peer; released on drop.
struct PeerGuard {
    table: Arc<PeerTable>,
    ip: IpAddr,
}

impl PeerTable {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            counts: Mutex::new(HashMap::new()),
            limit,
        })
    }

    /// `Err(open)` when the peer already holds its share.
    fn enter(self: &Arc<Self>, ip: IpAddr) -> Result<PeerGuard, usize> {
        // The critical sections are a map update; a poisoned lock holds
        // consistent data, so it is used as is.
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let open = counts.entry(ip).or_insert(0);
        if *open >= self.limit {
            return Err(*open);
        }
        *open += 1;
        Ok(PeerGuard {
            table: Arc::clone(self),
            ip,
        })
    }
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        let mut counts = self
            .table
            .counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(open) = counts.get_mut(&self.ip) {
            *open = open.saturating_sub(1);
            if *open == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

/// Why a connection was refused.
struct Refusal {
    reason: &'static str,
    limit: usize,
    open: usize,
}

/// Rate limit of the refusal log line.
struct RefusalLog {
    last: Option<Instant>,
    suppressed: u64,
}

impl RefusalLog {
    /// `Some(suppressed since the last line)` when a line may be written now.
    fn due(&mut self) -> Option<u64> {
        let now = Instant::now();
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < REFUSAL_LOG_INTERVAL)
        {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.suppressed))
    }
}

/// Serve until `shutdown` resolves; then stop accepting, let in-flight
/// requests finish within `limits.shutdown_grace`, and return.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    limits: ServerLimits,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ForgeError> {
    let total = u32::try_from(limits.max_connections.clamp(1, Semaphore::MAX_PERMITS))
        .unwrap_or(u32::MAX)
        .max(1);
    let slots = Arc::new(Semaphore::new(
        usize::try_from(total).unwrap_or(Semaphore::MAX_PERMITS),
    ));
    let peers = PeerTable::new(limits.max_connections_per_peer.max(1));
    let refusals = Arc::new(Semaphore::new(MAX_CONCURRENT_REFUSALS));
    let mut refusal_log = RefusalLog {
        last: None,
        suppressed: 0,
    };
    let (stop, _) = watch::channel(false);
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                // Usually out of file descriptors: log with a code, pause, retry.
                let err = ForgeError::new(ErrorCode::AcceptFailed)
                    .with_context("operation", "accept")
                    .with_source(e);
                telemetry::log_failure(&err, &RequestId::generate());
                tokio::select! {
                    () = &mut shutdown => break,
                    () = tokio::time::sleep(Duration::from_millis(100)) => continue,
                }
            }
        };
        let ip = peer.ip().to_canonical();
        match admit(&peers, &slots, ip, limits.max_connections) {
            Ok((permit, guard)) => {
                tokio::spawn(run_connection(
                    stream,
                    ip,
                    router.clone(),
                    limits,
                    stop.subscribe(),
                    (permit, guard),
                ));
            }
            Err(refusal) => {
                let request_id = RequestId::generate();
                if let Some(suppressed) = refusal_log.due() {
                    let err = ForgeError::new(ErrorCode::ServerBusy)
                        .with_context("reason", refusal.reason)
                        .with_context("peer", ip)
                        .with_context("limit", refusal.limit)
                        .with_context("open", refusal.open)
                        .with_context("suppressed", suppressed);
                    telemetry::log_failure(&err, &request_id);
                }
                match Arc::clone(&refusals).try_acquire_owned() {
                    Ok(slot) => {
                        tokio::spawn(async move {
                            send_busy(stream, &request_id).await;
                            drop(slot);
                        });
                    }
                    // Too many refusals at once: just close.
                    Err(_) => drop(stream),
                }
            }
        }
    }

    drop(listener);
    stop.send_replace(true);
    tokio::select! {
        // Every connection task holds one permit until it is done.
        all = slots.acquire_many(total) => match all {
            Ok(_) => Ok(()),
            Err(e) => Err(serve_failed("connection_limit").with_source(e)),
        },
        () = tokio::time::sleep(limits.shutdown_grace) => Err(serve_failed("shutdown_deadline")
            .with_context("grace_ms", limits.shutdown_grace.as_millis())),
    }
}

/// Take a connection slot: the peer's share first, then the global one.
fn admit(
    peers: &Arc<PeerTable>,
    slots: &Arc<Semaphore>,
    ip: IpAddr,
    max_connections: usize,
) -> Result<(OwnedSemaphorePermit, PeerGuard), Refusal> {
    let guard = peers.enter(ip).map_err(|open| Refusal {
        reason: "peer_limit",
        limit: peers.limit,
        open,
    })?;
    // clone: try_acquire_owned consumes an Arc handle
    let permit = Arc::clone(slots).try_acquire_owned().map_err(|_| Refusal {
        reason: "connection_limit",
        limit: max_connections,
        open: max_connections,
    })?;
    Ok((permit, guard))
}

/// Answer a refused connection with `FG-WEB-0008` and close it. The client
/// is not trusted to be quick: everything is bounded by [`REFUSAL_DEADLINE`].
async fn send_busy(mut stream: TcpStream, request_id: &RequestId) {
    let code = ErrorCode::ServerBusy;
    let lang = Lang::Ja;
    let body = serde_json::json!({
        "error_code": code.id(),
        "user_message": code.user_message(lang),
        "request_id": request_id.as_str(),
    })
    .to_string();
    let head = format!(
        "HTTP/1.1 503 Service Unavailable\r\n{}: application/json\r\n{}: {}\r\n{}: {BUSY_RETRY_AFTER_SECS}\r\n{}: close\r\nContent-Length: {}\r\n\r\n",
        CONTENT_TYPE,
        CONTENT_LANGUAGE,
        lang.as_str(),
        RETRY_AFTER,
        CONNECTION,
        body.len()
    );
    let _ = tokio::time::timeout(REFUSAL_DEADLINE, async {
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body.as_bytes()).await?;
        stream.shutdown().await?;
        // Read (and drop) what the client already sent: closing with unread
        // data would reset the connection and could lose the 503.
        let mut seen = 0;
        let mut buf = [0u8; 2048];
        while seen < REFUSAL_DRAIN_BYTES {
            match stream.read(&mut buf).await? {
                0 => break,
                n => seen += n,
            }
        }
        Ok::<(), io::Error>(())
    })
    .await; // ignore: the refused client may be gone or slow; nothing more is owed to it
}

/// One accepted connection until it ends. Holds its global slot and its
/// peer slot for as long as it lives.
async fn run_connection(
    stream: TcpStream,
    peer: IpAddr,
    router: Router,
    limits: ServerLimits,
    mut stop: watch::Receiver<bool>,
    _slots: (OwnedSemaphorePermit, PeerGuard),
) {
    let write_stalled = Arc::new(AtomicBool::new(false));
    let io = TokioIo::new(WriteIdle::new(
        stream,
        limits.write_idle_timeout,
        Arc::clone(&write_stalled),
    ));
    let served = Arc::new(AtomicU32::new(0));
    let max_requests = limits.keep_alive_max_requests;
    let service = service_fn(move |req: Request<Incoming>| {
        let router = router.clone();
        let nth = served.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        async move {
            let mut res = match router.oneshot(req).await {
                Ok(res) => res,
                Err(never) => match never {},
            };
            if nth >= max_requests {
                res.headers_mut()
                    .insert(CONNECTION, HeaderValue::from_static("close"));
            }
            Ok::<_, Infallible>(res)
        }
    });
    let conn = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout)
        // No HTTP upgrades: git Smart HTTP does not use them.
        .serve_connection(io, service);
    let mut conn = std::pin::pin!(conn);
    let mut lifetime = std::pin::pin!(tokio::time::sleep(limits.keep_alive_max));
    let mut draining = false;
    let result = loop {
        tokio::select! {
            r = conn.as_mut() => break r,
            // Graceful: the request being served finishes, then the
            // connection closes (an idle one closes at once).
            () = &mut lifetime, if !draining => {
                draining = true;
                conn.as_mut().graceful_shutdown();
            }
            _ = stop.changed(), if !draining => {
                draining = true;
                conn.as_mut().graceful_shutdown();
            }
        }
    };
    if let Err(e) = result {
        if write_stalled.load(Ordering::Relaxed) {
            let err = ForgeError::new(ErrorCode::RequestTimeout)
                .with_context("operation", "response_write")
                .with_context("kind", "idle")
                .with_context("peer", peer)
                .with_context("idle_ms", limits.write_idle_timeout.as_millis());
            telemetry::log_failure(&err, &RequestId::generate());
        } else {
            // Other client-side errors (reset, timeout while reading
            // headers) are not server failures; visible at debug level.
            tracing::debug!(target: "forge_web::server", error = %e, "http.connection_closed");
        }
    }
}

/// A socket whose writes must make progress: a write, flush or shutdown
/// that stays pending for `idle` (the peer does not read) fails with
/// `TimedOut`, and `stalled` records why. Progress of any size restarts the
/// clock, so a slow but reading client and a streaming git response are
/// never cut by it.
struct WriteIdle {
    inner: TcpStream,
    idle: Duration,
    timer: Option<Pin<Box<Sleep>>>,
    stalled: Arc<AtomicBool>,
}

impl WriteIdle {
    fn new(inner: TcpStream, idle: Duration, stalled: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            idle,
            timer: None,
            stalled,
        }
    }

    /// The inner write is pending: arm the clock (once) and fail when it ran out.
    fn pending<T>(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<T>> {
        let idle = self.idle;
        let timer = self
            .timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(idle)));
        if timer.as_mut().poll(cx).is_ready() {
            self.stalled.store(true, Ordering::Relaxed);
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "response write idle timeout",
            )));
        }
        Poll::Pending
    }

    fn settle<T>(
        &mut self,
        polled: Poll<io::Result<T>>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<T>> {
        match polled {
            Poll::Pending => self.pending(cx),
            ready => {
                self.timer = None;
                ready
            }
        }
    }
}

impl AsyncRead for WriteIdle {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for WriteIdle {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_write(cx, buf);
        this.settle(polled, cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        this.settle(polled, cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_flush(cx);
        this.settle(polled, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_shutdown(cx);
        this.settle(polled, cx)
    }
}

/// Resolves on Ctrl-C (SIGINT) or, on Unix, SIGTERM (`systemctl stop`).
/// If no handler can be installed the failure is logged with
/// `FG-SYS-0006` and the server keeps running (it never stops on its own).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            report_signal_failure("SIGINT", e);
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                report_signal_failure("SIGTERM", e);
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

/// The error logged when a signal handler cannot be installed.
pub fn signal_failure(signal: &'static str, e: std::io::Error) -> ForgeError {
    ForgeError::new(ErrorCode::SignalHandlerUnavailable)
        .with_context("signal", signal)
        .with_source(e)
}

fn report_signal_failure(signal: &'static str, e: std::io::Error) {
    telemetry::log_failure(&signal_failure(signal, e), &RequestId::generate());
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
