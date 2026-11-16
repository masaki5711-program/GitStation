//! The SSH git transport's bridge onto the Smart HTTP routes (M4 C13,
//! ADR-0033 §3). An SSH git command is served by **the same routes** as
//! Smart HTTP, called in process through the product router: the
//! dispatcher resolves the path and decides `RepoRead` / `RepoPush` with
//! `authorize()`, the push handler applies the protected-ref checks, the
//! pre-receive push checks and the push record, the fetch handler the want
//! check and the confidential copy check — nothing of that is repeated
//! or forked here. The request carries the SSH key's principal (a request
//! extension: no network input can set one) and the client's address.
//!
//! - `git-upload-pack` (protocol v2 only, `FG-GIT-0034` otherwise):
//!   `GET {path}/info/refs?service=git-upload-pack` → the capability
//!   advertisement; then each v2 request (pkt-lines up to a flush) is one
//!   `POST {path}/git-upload-pack`, its answer written back, until the
//!   client sends a lone flush or closes.
//! - `git-receive-pack`: `GET …?service=git-receive-pack` → the ref
//!   advertisement without the HTTP `# service=` prefix; then the command
//!   list, and the pack up to the client's EOF (none when every command
//!   is a delete: git does not close its side then), is the body of one
//!   `POST {path}/git-receive-pack`, whose report goes back.
//!
//! Only protocol v2 is served for upload-pack (v0 / v1 is FG-GIT-0034; the
//! route gets `Git-Protocol: version=2`, nothing the client sent). A push
//! that asks for `push-options` is refused (FG-GIT-0033
//! `reason=push_options`: the server never advertises them). A client
//! idle for `git.http.idle_timeout_ms` while a request or a pack is read
//! ends the command; the route's own body and total timeouts
//! (`git.http.body_timeout_ms`, `git.http.timeout_ms`) apply to the
//! in-process body exactly as to an HTTP one.
//!
//! An HTTP error answer (401 / 403 / 404 / 503 …) becomes one `ERR` packet
//! `<FG code> <user message> (request_id: <the HTTP request's id>)`, which
//! git prints as `fatal: remote error: …` — on side-band 3 instead once
//! a push negotiated a side-band (git then reads the report through the
//! side-band); the route logged it already.
//! An answer that cannot be read is `FG-GIT-0036 reason=bridge` (logged
//! here). The command's exit status is 0 when the HTTP answer was 200.

use std::io;
use std::net::IpAddr;

use axum::body::{Body, Bytes};
use axum::http::header::{CONTENT_TYPE, USER_AGENT};
use axum::http::{HeaderValue, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use forge_auth::Principal;
use forge_core::{telemetry, RequestId, UserId};
use forge_errors::{ErrorCode, ForgeError, Lang};
use futures_util::StreamExt as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tower::ServiceExt as _;

use super::command::{GitService, SshCommand};
use crate::context::REQUEST_ID_HEADER;

/// The largest pkt-line (git's `LARGE_PACKET_MAX`).
const MAX_PKT: usize = 65520;
/// The most bytes of an error answer read.
const MAX_ERROR_BODY: usize = 64 * 1024;
/// Chunks of a request body queued for the route.
const BODY_QUEUE: usize = 16;
/// Bytes read from the client at once (after the command list).
const CHUNK: usize = 64 * 1024;
/// The `User-Agent` the routes see (the push record keeps it).
const AGENT: &str = "gitstation-ssh";

/// Who runs the command (the SSH session's authentication).
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    pub principal: Principal,
    pub peer: IpAddr,
    pub ssh_key_id: i64,
    pub user_id: UserId,
    /// The proven key's fingerprint (decided again before each command).
    pub fingerprint: String,
    /// No byte from the client for this long while reading ends the
    /// command (`git.http.idle_timeout_ms`).
    pub idle: std::time::Duration,
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// The SSH exit status.
    pub exit_status: u32,
    /// The FG code the client was answered with, if any.
    pub refused: Option<&'static str>,
    /// The HTTP requests' ids, in order.
    pub http_request_ids: Vec<String>,
}

/// The client's side of the channel, read by pkt-lines (with what was read
/// ahead kept for the next read).
pub(crate) struct PktInput<R> {
    reader: R,
    buf: Vec<u8>,
    idle: std::time::Duration,
}

/// One pkt-line as it came (the 4 length bytes included).
#[derive(Debug, PartialEq, Eq)]
enum Pkt {
    Flush,
    Other(Vec<u8>),
}

fn protocol_error(detail: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail)
}

impl<R: AsyncRead + Unpin> PktInput<R> {
    pub(crate) fn new(reader: R, idle: std::time::Duration) -> Self {
        Self {
            reader,
            buf: Vec::new(),
            idle,
        }
    }

    /// One read, bounded by the idle timeout.
    async fn read_some(&mut self, chunk: &mut [u8]) -> io::Result<usize> {
        match tokio::time::timeout(self.idle, self.reader.read(chunk)).await {
            Ok(read) => read,
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "client_idle")),
        }
    }

    /// Read until `buf` holds `n` bytes; `false` at EOF before that.
    async fn fill(&mut self, n: usize) -> io::Result<bool> {
        let mut chunk = [0u8; 8192];
        while self.buf.len() < n {
            let read = self.read_some(&mut chunk).await?;
            if read == 0 {
                return Ok(false);
            }
            self.buf.extend_from_slice(&chunk[..read]);
        }
        Ok(true)
    }

    /// The next pkt-line; `None` at a clean EOF (no byte of a new line).
    async fn next(&mut self) -> io::Result<Option<Pkt>> {
        if !self.fill(4).await? {
            return if self.buf.is_empty() {
                Ok(None)
            } else {
                Err(protocol_error("truncated_length"))
            };
        }
        let len = std::str::from_utf8(&self.buf[..4])
            .ok()
            .and_then(|hex| usize::from_str_radix(hex, 16).ok())
            .ok_or_else(|| protocol_error("bad_length"))?;
        let len = match len {
            0 => {
                self.buf.drain(..4);
                return Ok(Some(Pkt::Flush));
            }
            // delim-pkt, response-end-pkt: four bytes.
            1 | 2 => 4,
            3 => return Err(protocol_error("bad_length")),
            n if n > MAX_PKT => return Err(protocol_error("too_long")),
            n => n,
        };
        if !self.fill(len).await? {
            return Err(protocol_error("truncated_packet"));
        }
        Ok(Some(Pkt::Other(self.buf.drain(..len).collect())))
    }

    /// Whatever is read ahead, then the rest up to EOF, in chunks.
    async fn next_raw(&mut self) -> io::Result<Option<Vec<u8>>> {
        if !self.buf.is_empty() {
            return Ok(Some(std::mem::take(&mut self.buf)));
        }
        let mut chunk = vec![0u8; CHUNK];
        let read = self.read_some(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        chunk.truncate(read);
        Ok(Some(chunk))
    }
}

/// `want_protocol` is the `GIT_PROTOCOL` the client set (`env` request).
pub(crate) fn wants_v2(git_protocol: Option<&str>) -> bool {
    git_protocol.is_some_and(|p| p.split(':').any(|kv| kv == "version=2"))
}

/// One `ERR` pkt-line (cut to a pkt-line's payload at a character
/// boundary). Written here: the SSH layer uses no forge-git API.
pub(crate) fn err_packet(text: &str) -> Vec<u8> {
    let mut line = format!("ERR {text}\n");
    if line.len() > MAX_PKT - 4 {
        let mut cut = MAX_PKT - 5;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push('\n');
    }
    let mut out = format!("{:04x}", line.len() + 4).into_bytes();
    out.extend_from_slice(line.as_bytes());
    out
}

/// One message on side-band 3 (git: `remote error: <text>`, then it
/// stops), for a push that negotiated a side-band.
pub(crate) fn band3_packet(text: &str) -> Vec<u8> {
    let mut payload = vec![3u8];
    payload.extend_from_slice(text.as_bytes());
    payload.push(b'\n');
    payload.truncate(MAX_PKT - 4);
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(&payload);
    out
}

/// A refusal's text as git shows it: `ERR` before a side-band exists,
/// side-band 3 after.
fn refusal_bytes(text: &str, sideband: bool) -> Vec<u8> {
    if sideband {
        band3_packet(text)
    } else {
        err_packet(text)
    }
}

/// The in-band answer of a refusal decided here (never a route's).
fn refusal_text(err: &ForgeError, request_id: &RequestId) -> String {
    format!(
        "{} {} (request_id: {})",
        err.code().id(),
        err.user_message(Lang::Ja),
        request_id
    )
}

/// Run one git command over the channel's `input` / `out`.
pub(crate) async fn run<R, W>(
    router: &Router,
    caller: &Caller,
    command: &SshCommand,
    git_protocol: Option<&str>,
    request_id: &RequestId,
    input: R,
    out: &mut W,
) -> Outcome
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let mut exchange = Exchange {
        router,
        caller,
        command,
        git_protocol,
        request_id,
        input: PktInput::new(input, caller.idle),
        sideband: false,
        outcome: Outcome {
            exit_status: 0,
            refused: None,
            http_request_ids: Vec::new(),
        },
    };
    let ended = match command.service {
        GitService::UploadPack => exchange.upload_pack(out).await,
        GitService::ReceivePack => exchange.receive_pack(out).await,
    };
    let sideband = exchange.sideband;
    let mut outcome = exchange.outcome;
    if let Err(stop) = ended {
        outcome.exit_status = 1;
        match stop {
            Stop::Refused(code) => outcome.refused = Some(code),
            Stop::Failed(err) => {
                telemetry::log_failure(&err, request_id);
                outcome.refused = Some(err.code().id());
                // Best effort: the client may be gone already.
                let bytes = refusal_bytes(&refusal_text(&err, request_id), sideband);
                let _ = out.write_all(&bytes).await; // ignore: see above
                let _ = out.flush().await; // ignore: see above
            }
            Stop::Client(e) => {
                tracing::debug!(
                    target: "forge_web::ssh",
                    request_id = %request_id,
                    error = %e,
                    "forge.ssh.client_gone"
                );
            }
        }
    }
    outcome
}

/// Why a command stopped early.
enum Stop {
    /// The client got an in-band refusal (already logged by its decider).
    Refused(&'static str),
    /// A failure of the bridge itself: logged and answered by [`run`].
    Failed(ForgeError),
    /// The client went away or broke the protocol: debug only.
    Client(io::Error),
}

struct Exchange<'a, R> {
    router: &'a Router,
    caller: &'a Caller,
    command: &'a SshCommand,
    git_protocol: Option<&'a str>,
    request_id: &'a RequestId,
    input: PktInput<R>,
    /// The push negotiated `side-band` / `side-band-64k`: refusals go to
    /// band 3 from then on.
    sideband: bool,
    outcome: Outcome,
}

impl<R: AsyncRead + Unpin + Send> Exchange<'_, R> {
    async fn upload_pack<W: AsyncWrite + Unpin + Send>(&mut self, out: &mut W) -> Result<(), Stop> {
        if !wants_v2(self.git_protocol) {
            let err = ForgeError::new(ErrorCode::SshProtocolV2Required)
                .with_context("service", GitService::UploadPack.name())
                .with_context("git_protocol", self.git_protocol.unwrap_or(""))
                .with_context("ssh_key_id", self.caller.ssh_key_id)
                .with_context("user_id", self.caller.user_id.get());
            telemetry::log_failure(&err, self.request_id);
            write(out, &err_packet(&refusal_text(&err, self.request_id))).await?;
            return Err(Stop::Refused(err.code().id()));
        }
        let advert = self
            .call(
                Method::GET,
                "info/refs?service=git-upload-pack",
                Body::empty(),
            )
            .await?;
        self.forward(advert, out, false).await?;
        loop {
            let first = match self.input.next().await.map_err(Stop::Client)? {
                // The client is done: EOF, or a lone flush.
                None | Some(Pkt::Flush) => return Ok(()),
                Some(Pkt::Other(bytes)) => bytes,
            };
            let (tx, body) = body_channel();
            let input = &mut self.input;
            let pump = async move {
                if tx.send(Ok(Bytes::from(first))).await.is_err() {
                    return Ok::<(), io::Error>(());
                }
                loop {
                    match input.next().await? {
                        None => return Err(protocol_error("eof_inside_request")),
                        Some(Pkt::Flush) => {
                            // The route stopped reading: it answers anyway.
                            let _ = tx.send(Ok(Bytes::from_static(b"0000"))).await; // ignore: see above
                            return Ok(());
                        }
                        Some(Pkt::Other(bytes)) => {
                            if tx.send(Ok(Bytes::from(bytes))).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                }
            };
            let call = call(
                self.router,
                self.caller,
                self.command,
                self.git_protocol,
                self.request_id,
                Method::POST,
                "git-upload-pack",
                body,
            );
            let (pumped, answer) = tokio::join!(pump, call);
            pumped.map_err(Stop::Client)?;
            let answer = answer?;
            self.forward(answer, out, false).await?;
        }
    }

    async fn receive_pack<W: AsyncWrite + Unpin + Send>(
        &mut self,
        out: &mut W,
    ) -> Result<(), Stop> {
        let advert = self
            .call(
                Method::GET,
                "info/refs?service=git-receive-pack",
                Body::empty(),
            )
            .await?;
        self.forward(advert, out, true).await?;
        // The command list (git's probe or "up to date": a lone flush, or
        // nothing at all, and the client is done).
        let first = match self.input.next().await.map_err(Stop::Client)? {
            None | Some(Pkt::Flush) => return Ok(()),
            Some(Pkt::Other(bytes)) => bytes,
        };
        let caps = capabilities(&first);
        self.sideband = caps
            .iter()
            .any(|c| *c == "side-band" || *c == "side-band-64k");
        if caps.contains(&"push-options") {
            let err = ForgeError::new(ErrorCode::SshCommandRefused)
                .with_context("reason", "push_options")
                .with_context("ssh_key_id", self.caller.ssh_key_id)
                .with_context("user_id", self.caller.user_id.get());
            telemetry::log_failure(&err, self.request_id);
            let text = refusal_text(&err, self.request_id);
            write(out, &refusal_bytes(&text, self.sideband)).await?;
            return Err(Stop::Refused(err.code().id()));
        }
        let (tx, body) = body_channel();
        let input = &mut self.input;
        let pump = async move {
            let mut delete_only = is_delete(&first);
            if tx.send(Ok(Bytes::from(first))).await.is_err() {
                return Ok::<(), io::Error>(());
            }
            loop {
                match input.next().await? {
                    None => return Err(protocol_error("eof_inside_commands")),
                    Some(Pkt::Flush) => {
                        if tx.send(Ok(Bytes::from_static(b"0000"))).await.is_err() {
                            return Ok(());
                        }
                        break;
                    }
                    Some(Pkt::Other(bytes)) => {
                        delete_only &= is_delete(&bytes);
                        if tx.send(Ok(Bytes::from(bytes))).await.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            // Deletes only: git sends no pack and keeps its side open.
            if delete_only {
                return Ok(());
            }
            while let Some(chunk) = input.next_raw().await? {
                if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                    return Ok(());
                }
            }
            Ok::<(), io::Error>(())
        };
        let call = call(
            self.router,
            self.caller,
            self.command,
            self.git_protocol,
            self.request_id,
            Method::POST,
            "git-receive-pack",
            body,
        );
        let (pumped, answer) = tokio::join!(pump, call);
        let answer = answer?;
        // A client that broke off mid-pack: the route saw a short body and
        // answered (and logged) accordingly; still pass its answer on.
        let forwarded = self.forward(answer, out, false).await;
        pumped.map_err(Stop::Client)?;
        forwarded
    }

    async fn call(&mut self, method: Method, tail: &str, body: Body) -> Result<Response, Stop> {
        call(
            self.router,
            self.caller,
            self.command,
            self.git_protocol,
            self.request_id,
            method,
            tail,
            body,
        )
        .await
    }

    /// Write a 200 answer through (without the `# service=` prefix when
    /// asked); any other status becomes one `ERR` packet.
    async fn forward<W: AsyncWrite + Unpin + Send>(
        &mut self,
        answer: Response,
        out: &mut W,
        strip_service: bool,
    ) -> Result<(), Stop> {
        let http_request_id = answer
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        self.outcome.http_request_ids.push(http_request_id.clone()); // clone: the error path names it too
        let status = answer.status();
        if status != StatusCode::OK {
            let line = error_line(answer, &http_request_id)
                .await
                .map_err(|e| self.bridge_failed(e, status, &http_request_id))?;
            write(out, &refusal_bytes(&line.text, self.sideband)).await?;
            return Err(Stop::Refused(line.code));
        }
        let mut stripper = if strip_service {
            Some(Vec::new())
        } else {
            None
        };
        let mut body = answer.into_body().into_data_stream();
        while let Some(frame) = body.next().await {
            let bytes = match frame {
                Ok(bytes) => bytes,
                // The route logged its own failure (git died, a timeout);
                // the client sees the stream end.
                Err(e) => {
                    return Err(Stop::Client(io::Error::other(e.to_string())));
                }
            };
            match stripper.as_mut() {
                None => write(out, &bytes).await?,
                Some(head) => {
                    head.extend_from_slice(&bytes);
                    if let Some(rest) = strip_service_prefix(head) {
                        write(out, &rest).await?;
                        stripper = None;
                    }
                }
            }
        }
        if let Some(head) = stripper {
            // A short answer: whatever came, unchanged.
            write(out, &head).await?;
        }
        out.flush().await.map_err(Stop::Client)?;
        Ok(())
    }

    fn bridge_failed(&self, cause: ForgeError, status: StatusCode, http_request_id: &str) -> Stop {
        Stop::Failed(
            ForgeError::new(ErrorCode::SshTransportFailed)
                .with_context("reason", "bridge")
                .with_context("service", self.command.service.name())
                .with_context("http_status", status.as_u16())
                .with_context("http_request_id", http_request_id)
                .with_context("ssh_key_id", self.caller.ssh_key_id)
                .with_context("user_id", self.caller.user_id.get())
                .with_source(cause),
        )
    }
}

async fn write<W: AsyncWrite + Unpin>(out: &mut W, bytes: &[u8]) -> Result<(), Stop> {
    out.write_all(bytes).await.map_err(Stop::Client)
}

type BodySender = mpsc::Sender<Result<Bytes, io::Error>>;

/// A request body fed chunk by chunk.
fn body_channel() -> (BodySender, Body) {
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(BODY_QUEUE);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    (tx, Body::from_stream(stream))
}

/// One request to the product router, as git's Smart HTTP client would
/// send it, with the SSH key's principal and the client's address.
#[allow(clippy::too_many_arguments)] // one call site per phase; a struct would only rename them
async fn call(
    router: &Router,
    caller: &Caller,
    command: &SshCommand,
    git_protocol: Option<&str>,
    request_id: &RequestId,
    method: Method,
    tail: &str,
    body: Body,
) -> Result<Response, Stop> {
    let failed = |reason: &'static str| {
        Stop::Failed(
            ForgeError::new(ErrorCode::SshTransportFailed)
                .with_context("reason", "bridge")
                .with_context("detail", reason)
                .with_context("service", command.service.name())
                .with_context("ssh_key_id", caller.ssh_key_id)
                .with_context("user_id", caller.user_id.get()),
        )
    };
    let uri = format!("/{}/{tail}", command.repo_path);
    let mut builder = Request::builder()
        .method(method.clone()) // clone: the content type below reads it
        .uri(uri)
        .header(USER_AGENT, AGENT)
        .header(REQUEST_ID_HEADER, request_id.as_str());
    if method == Method::POST {
        let media = match command.service {
            GitService::UploadPack => "application/x-git-upload-pack-request",
            GitService::ReceivePack => "application/x-git-receive-pack-request",
        };
        builder = builder.header(CONTENT_TYPE, media);
    }
    // Only what the bridge serves: upload-pack in v2 (refused before this
    // otherwise). Nothing the client sent reaches the route verbatim.
    if command.service == GitService::UploadPack && wants_v2(git_protocol) {
        builder = builder.header("git-protocol", HeaderValue::from_static("version=2"));
    }
    let mut request = builder.body(body).map_err(|_| failed("request_build"))?;
    request.extensions_mut().insert(caller.principal.clone()); // clone: one per request
    request
        .extensions_mut()
        .insert(crate::server::PeerAddr(caller.peer));
    let answer = router.clone().oneshot(request).await; // clone: a Router is a cheap handle
    match answer {
        Ok(answer) => Ok(answer),
        Err(never) => match never {},
    }
}

/// An error answer as git will show it.
struct ErrorLine {
    text: String,
    code: &'static str,
}

/// The route's JSON error (`{error_code, user_message, request_id}`) as
/// git's `ERR` line; anything else is `FG-GIT-0036 reason=bridge`.
async fn error_line(answer: Response, http_request_id: &str) -> Result<ErrorLine, ForgeError> {
    let bytes = axum::body::to_bytes(answer.into_body(), MAX_ERROR_BODY)
        .await
        .map_err(|e| {
            ForgeError::new(ErrorCode::SshTransportFailed)
                .with_context("detail", "error_body_unreadable")
                .with_source(io::Error::other(e.to_string()))
        })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
        ForgeError::new(ErrorCode::SshTransportFailed).with_context("detail", "error_body_not_json")
    })?;
    let field = |name: &str| value.get(name).and_then(serde_json::Value::as_str);
    let (Some(code), Some(message)) = (field("error_code"), field("user_message")) else {
        return Err(ForgeError::new(ErrorCode::SshTransportFailed)
            .with_context("detail", "error_body_fields"));
    };
    let Some(code) = ErrorCode::parse(code) else {
        return Err(ForgeError::new(ErrorCode::SshTransportFailed)
            .with_context("detail", "error_code_unknown"));
    };
    let rid = field("request_id").unwrap_or(http_request_id);
    Ok(ErrorLine {
        text: format!("{} {message} (request_id: {rid})", code.id()),
        code: code.id(),
    })
}

/// `head` starts an advertisement: once it is long enough to decide,
/// the bytes after a `# service=…` line and its flush (or all of them when
/// there is no such line). `None`: more bytes are needed.
fn strip_service_prefix(head: &[u8]) -> Option<Vec<u8>> {
    let len_hex = head.get(..4)?;
    let len = std::str::from_utf8(len_hex)
        .ok()
        .and_then(|h| usize::from_str_radix(h, 16).ok());
    let Some(len) = len.filter(|n| *n >= 4) else {
        return Some(head.to_vec());
    };
    let payload = head.get(4..len)?;
    if !payload.starts_with(b"# service=") {
        return Some(head.to_vec());
    }
    let after = head.get(len..len + 4)?;
    if after == b"0000" {
        Some(head[len + 4..].to_vec())
    } else {
        Some(head[len..].to_vec())
    }
}

/// The capabilities after the NUL of a receive-pack command list's first
/// line.
fn capabilities(pkt: &[u8]) -> Vec<&str> {
    let Some(payload) = pkt.get(4..) else {
        return Vec::new();
    };
    let Some(at) = payload.iter().position(|b| *b == 0) else {
        return Vec::new();
    };
    let caps = &payload[at + 1..];
    let caps = caps.strip_suffix(b"\n").unwrap_or(caps);
    std::str::from_utf8(caps)
        .map(|c| c.split(' ').filter(|w| !w.is_empty()).collect())
        .unwrap_or_default()
}

/// A receive-pack command that deletes its ref (`<old> <zero> <ref>`).
/// Anything that is not such a command (a non-delete, `shallow`, a
/// `push-cert` line) counts as "a pack may follow".
fn is_delete(pkt: &[u8]) -> bool {
    let Some(payload) = pkt.get(4..) else {
        return false;
    };
    let line = payload.split(|b| *b == 0).next().unwrap_or_default();
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let mut words = line.split(|b| *b == b' ');
    let (Some(old), Some(new), Some(_name)) = (words.next(), words.next(), words.next()) else {
        return false;
    };
    let hex = |w: &[u8]| (w.len() == 40 || w.len() == 64) && w.iter().all(u8::is_ascii_hexdigit);
    hex(old) && hex(new) && new.iter().all(|b| *b == b'0') && words.next().is_none()
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod tests;
