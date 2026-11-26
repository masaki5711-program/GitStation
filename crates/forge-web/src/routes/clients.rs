//! The `gstn` CLI distribution (`ClientDownload`, runbook cli-distribution):
//!
//! - `GET /api/v1/clients` → `{"items": [{"name": "gstn", "target",
//!   "version", "sha256", "size", "download_path"}]}`: the binaries this
//!   server ships, only the targets actually present.
//! - `GET /api/v1/clients/gstn/{target}/binary` → the bytes,
//!   `application/octet-stream`, `Content-Disposition: attachment;
//!   filename="gstn.exe"` (windows) / `"gstn"` (linux), `X-Checksum-Sha256`.
//!   An unknown or absent target is `404 FG-WEB-0012`.
//!
//! The binaries and `manifest.json` live in one directory (`clients.dir`,
//! default `<install>/share/forge/clients`) written by the image build;
//! forge only reads it. `{target}` is matched against a closed enum and
//! never used as a path: the file name on disk is fixed per target. Every
//! read of the directory is bounded ([`ClientDist::io_timeout`] per file
//! operation and per streamed chunk); a manifest is at most 64 KiB.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use forge_auth::InstanceCtx;
use forge_errors::{ErrorCode, ForgeError};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::registry::Dispatched;
use crate::routes::write::{json, no_store};

/// `GET` the list.
pub const LIST: &str = "/api/v1/clients";
/// `GET` one binary.
pub const BINARY: &str = "/api/v1/clients/gstn/{target}/binary";

const BINARY_PREFIX: &str = "/api/v1/clients/gstn/";
const BINARY_SUFFIX: &str = "/binary";
/// The manifest's file name inside the directory.
pub const MANIFEST: &str = "manifest.json";
/// The largest manifest read.
const MANIFEST_MAX: u64 = 64 * 1024;
/// The size of one read of a binary.
const READ_CHUNK: usize = 64 * 1024;
/// The default bound of one file operation (and of one streamed chunk).
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(10);
/// The default bound of a whole download.
pub const DEFAULT_DOWNLOAD_DEADLINE: Duration = Duration::from_secs(600);
/// The only client name distributed.
const NAME: &str = "gstn";

/// A platform `gstn` is built for. Closed: a path segment that is not one
/// of these is never looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    WindowsX86_64,
    LinuxX86_64,
}

impl Target {
    /// Every target, in listing order.
    pub const ALL: [Target; 2] = [Target::WindowsX86_64, Target::LinuxX86_64];

    /// The name in the API and the manifest.
    pub fn as_str(self) -> &'static str {
        match self {
            Target::WindowsX86_64 => "windows-x86_64",
            Target::LinuxX86_64 => "linux-x86_64",
        }
    }

    /// The target an API / manifest name stands for.
    pub fn parse(raw: &str) -> Option<Target> {
        Target::ALL.into_iter().find(|t| t.as_str() == raw)
    }

    /// The file's name inside the client directory (fixed per target).
    pub fn file_name(self) -> &'static str {
        match self {
            Target::WindowsX86_64 => "gstn-windows-x86_64.exe",
            Target::LinuxX86_64 => "gstn-linux-x86_64",
        }
    }

    /// The name the browser saves it under.
    pub fn download_name(self) -> &'static str {
        match self {
            Target::WindowsX86_64 => "gstn.exe",
            Target::LinuxX86_64 => "gstn",
        }
    }

    fn download_path(self) -> String {
        format!("{BINARY_PREFIX}{}{BINARY_SUFFIX}", self.as_str())
    }
}

/// The client directory (`forge serve` sets it from `clients.dir`).
#[derive(Debug, Clone)]
pub struct ClientDist {
    dir: PathBuf,
    io_timeout: Duration,
    download_deadline: Duration,
}

impl ClientDist {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            io_timeout: DEFAULT_IO_TIMEOUT,
            download_deadline: DEFAULT_DOWNLOAD_DEADLINE,
        }
    }

    /// The bound of a whole download (the stream ends with an error past it).
    #[must_use]
    pub fn with_download_deadline(mut self, deadline: Duration) -> Self {
        self.download_deadline = deadline;
        self
    }

    /// The bound of one file operation and of one streamed chunk.
    #[must_use]
    pub fn with_io_timeout(mut self, io_timeout: Duration) -> Self {
        self.io_timeout = io_timeout;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn broken(&self, reason: &'static str) -> ForgeError {
        ForgeError::new(ErrorCode::ClientDistributionInvalid)
            .with_context("reason", reason)
            .with_context("dir", self.dir.display().to_string())
    }

    async fn bounded<T>(
        &self,
        operation: &'static str,
        io: impl std::future::Future<Output = std::io::Result<T>>,
    ) -> Result<std::io::Result<T>, ForgeError> {
        tokio::time::timeout(self.io_timeout, io)
            .await
            .map_err(|_| {
                self.broken("timeout")
                    .with_context("operation", operation)
                    .with_context("timeout_ms", self.io_timeout.as_millis())
            })
    }

    /// The manifest's entries; `None` when the directory or the manifest
    /// does not exist (nothing is shipped).
    pub async fn manifest(&self) -> Result<Option<Vec<Shipped>>, ForgeError> {
        use tokio::io::AsyncReadExt as _;
        let path = self.dir.join(MANIFEST);
        let file = match self
            .bounded("manifest_open", tokio::fs::File::open(&path))
            .await?
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.broken("manifest_unreadable").with_source(e)),
        };
        let mut bytes = Vec::new();
        let mut limited = file.take(MANIFEST_MAX + 1);
        self.bounded("manifest_read", limited.read_to_end(&mut bytes))
            .await?
            .map_err(|e| self.broken("manifest_unreadable").with_source(e))?;
        if bytes.len() as u64 > MANIFEST_MAX {
            return Err(self.broken("manifest_too_large"));
        }
        parse_manifest(&bytes)
            .map(Some)
            .map_err(|rule| self.broken("manifest_invalid").with_context("rule", rule))
    }

    /// The size of a target's file; `None` when it is missing.
    async fn file_size(&self, target: Target) -> Result<Option<u64>, ForgeError> {
        let path = self.dir.join(target.file_name());
        match self
            .bounded("binary_stat", tokio::fs::metadata(&path))
            .await?
        {
            Ok(m) if m.is_file() => Ok(Some(m.len())),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(self
                .broken("read_failed")
                .with_context("target", target.as_str())
                .with_source(e)),
        }
    }

    fn mismatch(&self, s: &Shipped, actual: u64) -> ForgeError {
        self.broken("size_mismatch")
            .with_context("target", s.target.as_str())
            .with_context("expected", s.size)
            .with_context("actual", actual)
    }
}

/// One manifest entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shipped {
    pub target: Target,
    pub version: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    clients: Vec<RawEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    name: String,
    target: String,
    version: String,
    sha256: String,
    size: u64,
}

/// The manifest's entries, or the rule it breaks.
fn parse_manifest(bytes: &[u8]) -> Result<Vec<Shipped>, &'static str> {
    let raw: RawManifest = serde_json::from_slice(bytes).map_err(|_| "json")?;
    let mut out: Vec<Shipped> = Vec::with_capacity(raw.clients.len());
    for e in raw.clients {
        if e.name != NAME {
            return Err("name");
        }
        let target = Target::parse(&e.target).ok_or("target")?;
        if out.iter().any(|s| s.target == target) {
            return Err("duplicate_target");
        }
        if e.sha256.len() != 64
            || !e
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("sha256");
        }
        if e.version.is_empty()
            || e.version.len() > 64
            || !e
                .version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        {
            return Err("version");
        }
        if e.size == 0 {
            return Err("size");
        }
        out.push(Shipped {
            target,
            version: e.version,
            sha256: e.sha256,
            size: e.size,
        });
    }
    Ok(out)
}

/// One distributed binary.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientItem {
    /// Always `gstn`.
    pub name: String,
    /// `windows-x86_64` or `linux-x86_64`.
    pub target: String,
    pub version: String,
    /// Lower-case hex SHA-256 of the file.
    pub sha256: String,
    /// Bytes.
    pub size: u64,
    /// Where to `GET` the bytes (same credentials).
    pub download_path: String,
}

/// `200` of the list.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientList {
    pub items: Vec<ClientItem>,
}

fn dist(state: &AppState) -> Result<&ClientDist, ForgeError> {
    state.clients.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::ClientDistributionInvalid)
            .with_context("reason", "clients_unconfigured")
    })
}

fn not_found(reason: &'static str, target: &str) -> ForgeError {
    let shown: String = target.chars().take(32).collect();
    ForgeError::new(ErrorCode::ClientBinaryNotFound)
        .with_context("reason", reason)
        .with_context("target", shown)
}

/// `GET /api/v1/clients` → `200`.
pub async fn list(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    let built = async {
        let dist = dist(&state)?;
        let Some(entries) = dist.manifest().await? else {
            // Nothing shipped is not an error for the caller, but an
            // installation that should ship gstn must be able to see why.
            tracing::warn!(
                target: "forge_web",
                event = "forge.clients.no_manifest",
                request_id = %ctx.request_id,
                error_code = ErrorCode::ClientBinaryNotFound.id(),
                reason = "no_manifest",
                dir = %dist.dir.display(),
                "no client manifest; nothing is distributed"
            );
            return json(ClientList { items: Vec::new() });
        };
        let shipped = entries.len();
        let mut items = Vec::new();
        for s in entries {
            match dist.file_size(s.target).await? {
                Some(actual) if actual == s.size => items.push(ClientItem {
                    name: NAME.to_owned(),
                    target: s.target.as_str().to_owned(),
                    download_path: s.target.download_path(),
                    version: s.version,
                    sha256: s.sha256,
                    size: s.size,
                }),
                // A broken entry is logged and left out; the others stay
                // downloadable.
                Some(actual) => {
                    let e = dist.mismatch(&s, actual);
                    tracing::warn!(
                        target: "forge_web",
                        event = "forge.clients.entry_skipped",
                        request_id = %ctx.request_id,
                        error_code = e.code().id(),
                        target_name = s.target.as_str(),
                        expected = s.size,
                        actual,
                        "client binary size differs from the manifest; not listed"
                    );
                }
                None => tracing::warn!(
                    target: "forge_web",
                    event = "forge.clients.entry_skipped",
                    request_id = %ctx.request_id,
                    error_code = ErrorCode::ClientBinaryNotFound.id(),
                    target_name = s.target.as_str(),
                    reason = "file_missing",
                    "client binary in the manifest is missing; not listed"
                ),
            }
        }
        if shipped > 0 && items.is_empty() {
            // Every entry of the manifest is broken: the distribution is.
            return Err(dist
                .broken("nothing_servable")
                .with_context("entries", shipped));
        }
        json(ClientList { items })
    }
    .await;
    match built {
        Ok(v) => no_store(StatusCode::OK, v),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// The `{target}` of a binary path (unknown names are `None`).
fn target_of(path: &str) -> Result<Target, ForgeError> {
    let raw = path
        .strip_prefix(BINARY_PREFIX)
        .and_then(|r| r.strip_suffix(BINARY_SUFFIX))
        .unwrap_or("");
    Target::parse(raw).ok_or_else(|| not_found("unknown_target", raw))
}

/// `GET /api/v1/clients/gstn/{target}/binary` → `200` the bytes.
pub async fn binary(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    let path = request.raw_path().to_owned();
    let built = async {
        let target = target_of(&path)?;
        let dist = dist(&state)?;
        let Some(entries) = dist.manifest().await? else {
            return Err(not_found("no_manifest", target.as_str()));
        };
        let shipped = entries
            .into_iter()
            .find(|s| s.target == target)
            .ok_or_else(|| not_found("not_in_manifest", target.as_str()))?;
        stream(dist, shipped, ctx.request_id.clone()).await // clone: the body logs with it
    }
    .await;
    match built {
        Ok(res) => res,
        Err(e) => ctx.fail(e).into_response(),
    }
}

async fn stream(
    dist: &ClientDist,
    s: Shipped,
    request_id: forge_core::RequestId,
) -> Result<Response, ForgeError> {
    let path = dist.dir.join(s.target.file_name());
    let file = match dist
        .bounded("binary_open", tokio::fs::File::open(&path))
        .await?
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(not_found("file_missing", s.target.as_str()))
        }
        Err(e) => {
            return Err(dist
                .broken("read_failed")
                .with_context("target", s.target.as_str())
                .with_source(e))
        }
    };
    let actual = dist
        .bounded("binary_stat", file.metadata())
        .await?
        .map_err(|e| dist.broken("read_failed").with_source(e))?
        .len();
    if actual != s.size {
        return Err(dist.mismatch(&s, actual));
    }
    // Built before the body: a header that cannot be sent is an error, never
    // a download without its checksum.
    let checksum = HeaderValue::from_str(&s.sha256).map_err(|_| {
        dist.broken("header_invalid")
            .with_context("target", s.target.as_str())
    })?;
    let body = body_stream(
        file,
        Transfer {
            expected: s.size,
            sent: 0,
            io_timeout: dist.io_timeout,
            deadline: tokio::time::Instant::now() + dist.download_deadline,
            request_id,
            target: s.target,
        },
    );
    let mut res = Body::from_stream(body).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static(match s.target {
            Target::WindowsX86_64 => "attachment; filename=\"gstn.exe\"",
            Target::LinuxX86_64 => "attachment; filename=\"gstn\"",
        }),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(s.size));
    h.insert("x-checksum-sha256", checksum);
    Ok(res)
}

/// One download in flight: what it must send and how long it may take.
struct Transfer {
    expected: u64,
    sent: u64,
    io_timeout: Duration,
    deadline: tokio::time::Instant,
    request_id: forge_core::RequestId,
    target: Target,
}

impl Transfer {
    /// The one log event of a download that did not complete (the response
    /// status is already sent, so the log is the only trace), and the error
    /// that ends the body.
    fn fail(&self, reason: &'static str, source: Option<&std::io::Error>) -> std::io::Error {
        tracing::warn!(
            target: "forge_web",
            event = "forge.clients.download_incomplete",
            request_id = %self.request_id,
            error_code = ErrorCode::ClientDistributionInvalid.id(),
            reason,
            target_name = self.target.as_str(),
            sent = self.sent,
            expected = self.expected,
            io_error = source.map(|e| e.kind().to_string()).unwrap_or_default(),
            "client binary download ended before all bytes were sent"
        );
        let kind = match reason {
            "timeout" | "deadline" => std::io::ErrorKind::TimedOut,
            "short_read" => std::io::ErrorKind::UnexpectedEof,
            _ => std::io::ErrorKind::Other,
        };
        std::io::Error::new(kind, reason)
    }
}

/// The body: at most `expected` bytes in [`READ_CHUNK`] reads, each bounded
/// by `io_timeout`, the whole by `deadline`; a read error, a timeout or a
/// file that ends early is logged once and ends the body with an error.
fn body_stream<R>(
    reader: R,
    transfer: Transfer,
) -> impl futures_util::Stream<Item = std::io::Result<axum::body::Bytes>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt as _;
    let limited = reader.take(transfer.expected);
    futures_util::stream::unfold(Some((limited, transfer)), |state| async move {
        let (mut f, mut t) = state?;
        if t.sent == t.expected {
            return None;
        }
        let now = tokio::time::Instant::now();
        if now >= t.deadline {
            let e = t.fail("deadline", None);
            return Some((Err(e), None));
        }
        let wait = t.io_timeout.min(t.deadline - now);
        let mut buf = vec![0u8; READ_CHUNK];
        match tokio::time::timeout(wait, f.read(&mut buf)).await {
            Ok(Ok(0)) => {
                let e = t.fail("short_read", None);
                Some((Err(e), None))
            }
            Ok(Ok(n)) => {
                buf.truncate(n);
                t.sent += n as u64;
                Some((Ok(axum::body::Bytes::from(buf)), Some((f, t))))
            }
            Ok(Err(e)) => {
                let err = t.fail("read_failed", Some(&e));
                Some((Err(err), None))
            }
            Err(_) => {
                let reason = if tokio::time::Instant::now() >= t.deadline {
                    "deadline"
                } else {
                    "timeout"
                };
                let e = t.fail(reason, None);
                Some((Err(e), None))
            }
        }
    })
}

#[cfg(test)]
mod unit {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn targets_are_a_closed_set_and_paths_never_reach_the_disk() {
        assert_eq!(
            target_of("/api/v1/clients/gstn/linux-x86_64/binary").expect("linux"),
            Target::LinuxX86_64
        );
        assert_eq!(
            target_of("/api/v1/clients/gstn/windows-x86_64/binary").expect("windows"),
            Target::WindowsX86_64
        );
        for bad in [
            "/api/v1/clients/gstn/../manifest.json/binary",
            "/api/v1/clients/gstn/darwin-arm64/binary",
            "/api/v1/clients/gstn//binary",
            "/api/v1/clients/gstn/linux-x86_64/../binary",
        ] {
            let err = target_of(bad).expect_err(bad);
            assert_eq!(err.code(), ErrorCode::ClientBinaryNotFound, "{bad}");
        }
        for t in Target::ALL {
            assert_eq!(Target::parse(t.as_str()), Some(t));
            assert!(!t.file_name().contains(['/', '\\']));
            assert_eq!(
                t.download_path(),
                format!("/api/v1/clients/gstn/{}/binary", t.as_str())
            );
        }
    }

    fn transfer(expected: u64, deadline: Duration) -> Transfer {
        Transfer {
            expected,
            sent: 0,
            io_timeout: Duration::from_secs(10),
            deadline: tokio::time::Instant::now() + deadline,
            request_id: forge_core::RequestId::generate(),
            target: Target::LinuxX86_64,
        }
    }

    async fn drain(
        body: impl futures_util::Stream<Item = std::io::Result<axum::body::Bytes>>,
    ) -> (u64, Option<std::io::ErrorKind>) {
        use futures_util::StreamExt as _;
        let mut body = std::pin::pin!(body);
        let mut sent = 0;
        while let Some(item) = body.next().await {
            match item {
                Ok(b) => sent += b.len() as u64,
                Err(e) => return (sent, Some(e.kind())),
            }
        }
        (sent, None)
    }

    #[tokio::test]
    async fn a_complete_body_ends_cleanly_and_a_short_one_with_an_error() {
        let bytes: &'static [u8] = b"0123456789";
        assert_eq!(
            drain(body_stream(bytes, transfer(10, Duration::from_secs(60)))).await,
            (10, None)
        );
        // The file shrank after the size check: 10 bytes for 16 promised.
        assert_eq!(
            drain(body_stream(bytes, transfer(16, Duration::from_secs(60)))).await,
            (10, Some(std::io::ErrorKind::UnexpectedEof))
        );
        // Never more than promised, even if the file grew.
        assert_eq!(
            drain(body_stream(bytes, transfer(4, Duration::from_secs(60)))).await,
            (4, None)
        );
    }

    #[tokio::test]
    async fn a_read_error_and_the_deadline_end_the_body_with_an_error() {
        let failing = tokio_test_reader_error();
        assert_eq!(
            drain(body_stream(failing, transfer(10, Duration::from_secs(60)))).await,
            (0, Some(std::io::ErrorKind::Other))
        );
        let bytes: &'static [u8] = b"0123456789";
        assert_eq!(
            drain(body_stream(bytes, transfer(10, Duration::ZERO))).await,
            (0, Some(std::io::ErrorKind::TimedOut))
        );
    }

    /// A reader whose every read fails.
    fn tokio_test_reader_error() -> impl tokio::io::AsyncRead + Unpin + Send + 'static {
        struct Failing;
        impl tokio::io::AsyncRead for Failing {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::Error::other("disk gone")))
            }
        }
        Failing
    }

    #[test]
    fn the_manifest_is_strict() {
        let ok = format!(
            r#"{{"clients":[{{"name":"gstn","target":"linux-x86_64","version":"0.1.0+abc","sha256":"{SHA}","size":3}}]}}"#
        );
        let got = parse_manifest(ok.as_bytes()).expect("valid");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].target, Target::LinuxX86_64);
        let entry = |name: &str, target: &str, version: &str, sha: &str, size: u64| {
            format!(
                r#"{{"name":"{name}","target":"{target}","version":"{version}","sha256":"{sha}","size":{size}}}"#
            )
        };
        let cases = [
            ("not json".to_owned(), "json"),
            (
                format!(
                    r#"{{"clients":[{}]}}"#,
                    entry("git", "linux-x86_64", "1", SHA, 3)
                ),
                "name",
            ),
            (
                format!(r#"{{"clients":[{}]}}"#, entry("gstn", "../x", "1", SHA, 3)),
                "target",
            ),
            (
                format!(
                    r#"{{"clients":[{},{}]}}"#,
                    entry("gstn", "linux-x86_64", "1", SHA, 3),
                    entry("gstn", "linux-x86_64", "1", SHA, 3)
                ),
                "duplicate_target",
            ),
            (
                format!(
                    r#"{{"clients":[{}]}}"#,
                    entry("gstn", "linux-x86_64", "1", "ABC", 3)
                ),
                "sha256",
            ),
            (
                format!(
                    r#"{{"clients":[{}]}}"#,
                    entry("gstn", "linux-x86_64", "1 2", SHA, 3)
                ),
                "version",
            ),
            (
                format!(
                    r#"{{"clients":[{}]}}"#,
                    entry("gstn", "linux-x86_64", "1", SHA, 0)
                ),
                "size",
            ),
        ];
        for (text, rule) in cases {
            assert_eq!(parse_manifest(text.as_bytes()).expect_err(rule), rule);
        }
    }
}
