//! Connection fairness (M1 phase 9 MR0): per-peer and global caps, 503
//! `FG-WEB-0008`, keep-alive limits, response-write idle timeout. The
//! server under test runs a plain axum router: these limits live in the
//! connection loop, below the registry.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::header::CONTENT_LENGTH;
use axum::response::IntoResponse;
use axum::Router;
use forge_auth::{Action, InstanceCtx};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::app::{build, AppConfig, AppState};
use crate::registry::{Dispatched, Registry, Verb};
use crate::tests::{no_lookups, no_tokens};
use crate::ServerLimits;

const CHUNK: usize = 64 * 1024;

static DATA: [u8; CHUNK] = [b'x'; CHUNK];

type Req = Dispatched<InstanceCtx>;

async fn healthz(_: AppState, _: Req) -> &'static str {
    "ok"
}

/// Never ends: only a closed connection stops it.
async fn endless(_: AppState, _: Req) -> Body {
    Body::from_stream(futures_util::stream::repeat_with(|| {
        Ok::<_, io::Error>(Bytes::from_static(&DATA))
    }))
}

/// Reads the whole request body; the length, or `read_failed`.
async fn upload(_: AppState, req: Req) -> &'static str {
    match axum::body::to_bytes(req.into_body(), 1 << 20).await {
        Ok(_) => "read",
        Err(_) => "read_failed", // the failure is logged by the server, not here
    }
}

/// A handler busy elsewhere (no body read) for longer than the read idle time.
async fn slow(_: AppState, _: Req) -> &'static str {
    tokio::time::sleep(Duration::from_millis(900)).await;
    "slow"
}

/// 32 MiB with a Content-Length, like a git pack being streamed.
async fn stream32(_: AppState, _: Req) -> impl IntoResponse {
    let chunks = 512;
    let body = Body::from_stream(futures_util::stream::iter(
        (0..chunks).map(|_| Ok::<_, io::Error>(Bytes::from_static(&DATA))),
    ));
    ([(CONTENT_LENGTH, (chunks * CHUNK).to_string())], body)
}

fn router() -> Router {
    let registry = Registry::new()
        .route(Verb::Get, "/healthz", Action::Health, "healthz", healthz)
        .route(Verb::Get, "/endless", Action::Health, "endless", endless)
        .route(Verb::Get, "/stream", Action::Health, "stream", stream32)
        .route(Verb::Post, "/upload", Action::Health, "upload", upload)
        .route(Verb::Get, "/slow", Action::Health, "slow", slow);
    build(
        registry,
        AppState::new("test", Vec::new(), no_tokens(), no_lookups()),
        AppConfig {
            request_timeout: Duration::from_secs(3600),
            ..AppConfig::default()
        },
    )
    .expect("router")
}

struct Running {
    addr: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), forge_errors::ForgeError>>,
}

impl Running {
    async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(()); // ignore: the server may already be stopping
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), self.task).await; // ignore: cleanup
    }
}

async fn start(limits: ServerLimits) -> Running {
    let listener = crate::server::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(crate::server::serve(listener, router(), limits, async {
        let _ = rx.await; // ignore: a dropped sender also means "stop"
    }));
    Running {
        addr,
        stop: Some(tx),
        task,
    }
}

fn limits() -> ServerLimits {
    ServerLimits {
        header_read_timeout: Duration::from_secs(30),
        ..ServerLimits::default()
    }
}

/// Everything up to and including the blank line, plus what was read past it.
async fn read_head(s: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(pos + 4);
            return (String::from_utf8(buf).expect("ascii head"), rest);
        }
        let mut chunk = [0u8; 1024];
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut chunk))
            .await
            .expect("head in time")
            .expect("read");
        assert!(n > 0, "closed before the head ended: {buf:?}");
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn get_keep_alive(s: &mut TcpStream) -> String {
    s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .expect("write");
    let (head, mut rest) = read_head(s).await;
    // The body is "ok" (2 bytes).
    while rest.len() < 2 {
        let mut b = [0u8; 16];
        let n = s.read(&mut b).await.expect("body");
        assert!(n > 0, "closed in the body");
        rest.extend_from_slice(&b[..n]);
    }
    head
}

async fn closed_within(s: &mut TcpStream, within: Duration) -> bool {
    let mut b = [0u8; 1024];
    matches!(
        tokio::time::timeout(within, s.read(&mut b)).await,
        Ok(Ok(0) | Err(_))
    )
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Logs {
    /// Capture this thread's events (`#[tokio::test]` runs on one thread).
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        let sink = self.clone();
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_writer(move || sink.clone())
                .finish(),
        )
    }

    fn failures(&self) -> Vec<Value> {
        let text = String::from_utf8(self.0.lock().expect("log buffer").clone()).expect("utf-8");
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).expect("json line"))
            .filter(|e| e.get("error_code").is_some())
            .collect()
    }
}

#[tokio::test]
async fn connections_over_the_per_peer_cap_get_503() {
    let logs = Logs::default();
    let _capture = logs.install();
    let server = start(ServerLimits {
        max_connections_per_peer: 2,
        ..limits()
    })
    .await;
    let mut held = vec![
        TcpStream::connect(&server.addr).await.expect("first"),
        TcpStream::connect(&server.addr).await.expect("second"),
    ];
    // Let the accept loop count both before the third arrives.
    for s in &mut held {
        assert!(get_keep_alive(s).await.starts_with("HTTP/1.1 200"));
    }
    let mut third = TcpStream::connect(&server.addr).await.expect("third");
    third
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .expect("write");
    let (head, rest) = read_head(&mut third).await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(
        head.to_ascii_lowercase().contains("retry-after: 5"),
        "{head}"
    );
    assert!(
        head.to_ascii_lowercase().contains("connection: close"),
        "{head}"
    );
    let mut body = rest;
    third.read_to_end(&mut body).await.expect("body to the end");
    let body: Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(body["error_code"], "FG-WEB-0008");
    assert!(body["request_id"].as_str().is_some_and(|r| !r.is_empty()));
    assert!(
        body.get("peer").is_none(),
        "no internals in the body: {body}"
    );

    // Logged once, with the code, the request id and the peer.
    let failures = logs.failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0]["error_code"], "FG-WEB-0008");
    assert_eq!(failures[0]["request_id"], body["request_id"]);
    let ctx = failures[0]["context"].as_str().expect("context");
    assert!(ctx.contains("reason=peer_limit"), "{ctx}");
    assert!(ctx.contains("peer=127.0.0.1"), "{ctx}");

    // A slot frees when a held connection closes.
    drop(held.pop());
    let mut again = None;
    for _ in 0..50 {
        let mut s = TcpStream::connect(&server.addr).await.expect("again");
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .expect("write");
        let (head, _) = read_head(&mut s).await;
        if head.starts_with("HTTP/1.1 200") {
            again = Some(head);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(again.is_some(), "a freed slot must be usable again");
    server.stop().await;
}

#[tokio::test]
async fn connections_over_the_global_cap_get_503() {
    let server = start(ServerLimits {
        max_connections: 2,
        max_connections_per_peer: 2,
        ..limits()
    })
    .await;
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut s = TcpStream::connect(&server.addr).await.expect("held");
        assert!(get_keep_alive(&mut s).await.starts_with("HTTP/1.1 200"));
        held.push(s);
    }
    let mut over = TcpStream::connect(&server.addr).await.expect("over");
    over.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .expect("write");
    let (head, _) = read_head(&mut over).await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    drop(held);
    server.stop().await;
}

#[tokio::test]
async fn idle_keepalive_is_closed() {
    let server = start(ServerLimits {
        header_read_timeout: Duration::from_millis(200),
        ..limits()
    })
    .await;
    let mut s = TcpStream::connect(&server.addr).await.expect("connect");
    assert!(get_keep_alive(&mut s).await.starts_with("HTTP/1.1 200"));
    assert!(
        closed_within(&mut s, Duration::from_secs(5)).await,
        "an idle keep-alive connection must be closed"
    );
    server.stop().await;
}

#[tokio::test]
async fn keep_alive_requests_and_lifetime_are_bounded() {
    let server = start(ServerLimits {
        keep_alive_max_requests: 2,
        ..limits()
    })
    .await;
    let mut s = TcpStream::connect(&server.addr).await.expect("connect");
    let first = get_keep_alive(&mut s).await;
    assert!(
        !first.to_ascii_lowercase().contains("connection: close"),
        "{first}"
    );
    let second = get_keep_alive(&mut s).await;
    assert!(
        second.to_ascii_lowercase().contains("connection: close"),
        "{second}"
    );
    assert!(closed_within(&mut s, Duration::from_secs(5)).await);
    server.stop().await;

    // Lifetime: an idle connection past it is closed (header-read timeout
    // is long here, so only the lifetime can do it).
    let server = start(ServerLimits {
        keep_alive_max: Duration::from_millis(300),
        ..limits()
    })
    .await;
    let mut s = TcpStream::connect(&server.addr).await.expect("connect");
    assert!(get_keep_alive(&mut s).await.starts_with("HTTP/1.1 200"));
    assert!(closed_within(&mut s, Duration::from_secs(5)).await);
    server.stop().await;
}

#[tokio::test]
async fn a_slow_reader_of_a_response_is_cut_after_the_write_idle_timeout() {
    let logs = Logs::default();
    let _capture = logs.install();
    let server = start(ServerLimits {
        write_idle_timeout: Duration::from_millis(300),
        ..limits()
    })
    .await;
    let mut s = TcpStream::connect(&server.addr).await.expect("connect");
    s.write_all(b"GET /endless HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .expect("write");
    // The client does not read: the socket buffers fill, then the write stalls.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // The response never ends by itself, so reading to the end finishes only
    // if the server closed the connection.
    let mut sink = tokio::io::sink();
    let ended = tokio::time::timeout(Duration::from_secs(20), async {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.write_all(&buf[..n]).await.expect("sink"),
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "the stalled connection must be closed");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let failures = logs.failures();
    let hit = failures.iter().find(|f| f["error_code"] == "FG-WEB-0002");
    let ctx = hit.and_then(|f| f["context"].as_str()).unwrap_or_default();
    assert!(ctx.contains("operation=response_write"), "{failures:?}");
    assert!(ctx.contains("kind=idle"), "{ctx}");
    server.stop().await;
}

/// A long stream that keeps being read is not idle, and neither the
/// keep-alive lifetime nor the request limit cuts the request being served.
#[tokio::test]
async fn an_active_stream_is_not_cut() {
    let server = start(ServerLimits {
        write_idle_timeout: Duration::from_millis(1000),
        keep_alive_max: Duration::from_millis(200),
        keep_alive_max_requests: 1,
        ..limits()
    })
    .await;
    let mut s = TcpStream::connect(&server.addr).await.expect("connect");
    s.write_all(b"GET /stream HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .expect("write");
    let (head, rest) = read_head(&mut s).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let mut got = rest.len();
    let started = std::time::Instant::now();
    let mut buf = vec![0u8; CHUNK];
    while got < 512 * CHUNK {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf))
            .await
            .expect("data keeps coming")
            .expect("read");
        assert!(n > 0, "cut after {got} bytes");
        got += n;
        // Slower than the producer: writes stall for short spells, each
        // far below the idle timeout.
        tokio::time::sleep(Duration::from_millis(4)).await;
    }
    assert_eq!(got, 512 * CHUNK);
    assert!(
        started.elapsed() > Duration::from_millis(400),
        "the stream must outlive the lifetime and the idle timeout: {:?}",
        started.elapsed()
    );
    // The request limit / lifetime close the connection after the response.
    assert!(closed_within(&mut s, Duration::from_secs(5)).await);
    server.stop().await;
}

#[test]
fn server_limits_follow_the_configuration() {
    let cfg = forge_core::config::ServerLimitsConfig::DEFAULT;
    let l = ServerLimits::from_config(&cfg);
    assert_eq!(l.max_connections, 1024);
    assert_eq!(l.max_connections_per_peer, 64);
    assert_eq!(l.write_idle_timeout, cfg.write_idle_timeout);
}

#[tokio::test]
async fn a_stalled_request_body_is_cut_but_a_busy_handler_and_a_steady_body_are_not() {
    let logs = Logs::default();
    let _capture = logs.install();
    let server = start(ServerLimits {
        read_idle_timeout: Duration::from_millis(400),
        ..limits()
    })
    .await;

    // A body that stops arriving: the read fails after the idle time.
    let mut s = TcpStream::connect(&server.addr).await.expect("connect");
    s.write_all(b"POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nabc")
        .await
        .expect("write");
    let (head, rest) = read_head(&mut s).await;
    assert!(
        head.starts_with("HTTP/1.1 500") || head.starts_with("HTTP/1.1 200"),
        "{head}"
    );
    let mut body = rest;
    let _ = tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut body)).await; // ignore: only the text matters
    assert!(
        String::from_utf8_lossy(&body).contains("read_failed"),
        "{body:?}"
    );
    let failures = logs.failures();
    let ctx = failures
        .iter()
        .find(|f| f["error_code"] == "FG-WEB-0002")
        .and_then(|f| f["context"].as_str())
        .unwrap_or_default();
    assert!(ctx.contains("operation=request_read"), "{failures:?}");
    assert!(ctx.contains("kind=idle"), "{ctx}");

    // A body that keeps arriving (gaps below the idle time, total above it).
    let mut s = TcpStream::connect(&server.addr).await.expect("connect");
    s.write_all(
        b"POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: 6\r\nConnection: close\r\n\r\n",
    )
    .await
    .expect("write");
    for part in [b"ab", b"cd", b"ef"] {
        tokio::time::sleep(Duration::from_millis(250)).await;
        s.write_all(part).await.expect("part");
    }
    let mut out = String::new();
    s.read_to_string(&mut out).await.expect("response");
    assert!(
        out.starts_with("HTTP/1.1 200") && out.ends_with("read"),
        "{out}"
    );

    // A handler that is busy elsewhere (no body wanted) is not read-idle.
    let mut s = TcpStream::connect(&server.addr).await.expect("connect");
    s.write_all(b"GET /slow HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut out = String::new();
    s.read_to_string(&mut out).await.expect("response");
    assert!(
        out.starts_with("HTTP/1.1 200") && out.ends_with("slow"),
        "{out}"
    );
    server.stop().await;
}

#[tokio::test]
async fn suppressed_refusals_are_summarised_when_the_flood_stops() {
    let logs = Logs::default();
    let _capture = logs.install();
    let server = start(ServerLimits {
        max_connections_per_peer: 1,
        ..limits()
    })
    .await;
    let mut held = TcpStream::connect(&server.addr).await.expect("held");
    assert!(get_keep_alive(&mut held).await.starts_with("HTTP/1.1 200"));
    for _ in 0..4 {
        let mut s = TcpStream::connect(&server.addr).await.expect("over");
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        let (head, _) = read_head(&mut s).await;
        assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    }
    // One line at once (3 suppressed), then a summary after a quiet second.
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let contexts: Vec<String> = logs
        .failures()
        .iter()
        .filter(|f| f["error_code"] == "FG-WEB-0008")
        .map(|f| f["context"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(contexts.len(), 2, "{contexts:?}");
    assert!(contexts[0].contains("reason=peer_limit"), "{contexts:?}");
    assert!(contexts[1].contains("reason=summary"), "{contexts:?}");
    assert!(contexts[1].contains("suppressed=3"), "{contexts:?}");
    drop(held);
    server.stop().await;
}

#[test]
fn ipv6_peers_count_by_their_64_prefix_and_ipv4_by_address() {
    use std::net::IpAddr;
    let ip = |s: &str| -> IpAddr { s.parse().expect("ip") };
    use super::{peer_key, PeerTable};
    assert_eq!(peer_key(ip("2001:db8:1:2:aaaa::1")), ip("2001:db8:1:2::"));
    assert_eq!(peer_key(ip("2001:db8:1:2:bbbb::9")), ip("2001:db8:1:2::"));
    assert_ne!(peer_key(ip("2001:db8:1:3::1")), ip("2001:db8:1:2::"));
    assert_eq!(peer_key(ip("192.0.2.7")), ip("192.0.2.7"));
    assert_eq!(peer_key(ip("::ffff:192.0.2.7")), ip("192.0.2.7"));

    let table = PeerTable::new(1);
    let _first = table.enter(peer_key(ip("2001:db8:1:2::1"))).expect("first");
    assert!(table.enter(peer_key(ip("2001:db8:1:2:ffff::2"))).is_err());
    assert!(table.enter(peer_key(ip("2001:db8:1:9::1"))).is_ok());
    assert!(table.enter(peer_key(ip("192.0.2.7"))).is_ok());
}

#[test]
fn a_stale_wait_does_not_fire_but_a_real_stall_does() {
    use super::{IdleTimer, Wait};
    use std::time::Instant;
    let idle = Duration::from_secs(10);
    let t0 = Instant::now();
    let mut timer = IdleTimer::new(idle);
    assert_eq!(timer.pending_at(t0), Wait::Left(idle));
    assert_eq!(
        timer.pending_at(t0 + Duration::from_secs(4)),
        Wait::Left(Duration::from_secs(6))
    );
    assert_eq!(
        timer.pending_at(t0 + Duration::from_secs(10)),
        Wait::Expired
    );

    // Nobody polled for far longer than the idle time: not the peer's stall.
    let mut timer = IdleTimer::new(idle);
    assert_eq!(timer.pending_at(t0), Wait::Left(idle));
    let later = t0 + Duration::from_secs(600);
    assert_eq!(timer.pending_at(later), Wait::Left(idle));
    // Progress starts a new wait.
    timer.progress();
    assert_eq!(
        timer.pending_at(later + Duration::from_secs(9)),
        Wait::Left(idle)
    );
}
