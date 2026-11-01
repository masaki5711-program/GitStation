//! The content routes (`RepoRead`, `read_repository`; M2 MR7, ADR-0015
//! "応答の安全" and Amendment M2): a file as it is, and an archive.
//!
//! - `GET|HEAD {path}/-/raw/{ref}/{file…}` — the tail is split into a
//!   revision and a path by ADR-0015's order (full commit id, longest
//!   branch, longest tag), after `authorize()`.
//!   - A file up to 4 MiB is read whole and typed: an image only when its
//!     extension **and** its first bytes agree (png / jpeg / gif / webp →
//!     `image/…`, `inline`); SVG, HTML, XML and PDF are never shown — they
//!     are `application/octet-stream` + `attachment` (no preview origin in
//!     M2); valid UTF-8 without NUL is `text/plain; charset=utf-8`
//!     (`inline`); anything else `application/octet-stream` +
//!     `attachment`. A larger file streams as `application/octet-stream`
//!     (`attachment`).
//! - `GET|HEAD {path}/-/archive/{ref}.{zip|tar.gz}` — the extension is
//!   removed once, the rest must be exactly a revision (no path);
//!   `git archive` streams below `<project>-<commit 12>/` as
//!   `application/zip` / `application/gzip`, `attachment;
//!   filename="<project>-<ref>.<ext>"`.
//!
//! Every answer: `Content-Disposition` per RFC 6266 (an ASCII `filename`
//! and the UTF-8 `filename*`), `Cache-Control: private, no-store`, `Vary:
//! Authorization, Cookie`; the security layer adds `nosniff`, `CSP:
//! sandbox; default-src 'none'` and `Cross-Origin-Resource-Policy:
//! same-origin` (ADR-0022 §3). A `HEAD` answers the headers without
//! starting a stream. A stream that ends badly is cut (never silently
//! short) and logged; a client that goes away kills git (`FG-GIT-0015`).

use std::sync::Arc;

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, VARY};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use forge_auth::ProjectCtx;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{ArchiveFormat, RawBody, RepoPath};

use crate::app::AppState;
use crate::git_stream::{GitResponse, StreamLog};
use crate::registry::Dispatched;
use crate::routes::git::{principal_key, ready};

const OCTET: &str = "application/octet-stream";
const TEXT: &str = "text/plain; charset=utf-8";

/// Extensions never shown, whatever their bytes (ADR-0015 Amendment M2:
/// no preview origin; SVG, HTML and PDF are downloads).
const DOWNLOAD_ONLY: &[&str] = &[
    "svg", "svgz", "html", "htm", "xhtml", "xht", "shtml", "mht", "mhtml", "xml", "xsl", "xslt",
    "pdf",
];

/// `FG-REPO-0023` with its `reason`.
fn not_resolved(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RepositoryPathNotResolved).with_context("reason", reason)
}

/// The image type `name`'s extension claims, if its first bytes agree.
pub(crate) fn image_type(name: &str, bytes: &[u8]) -> Option<&'static str> {
    let ext = extension(name)?;
    let (ty, ok) = match ext.as_str() {
        "png" => ("image/png", bytes.starts_with(b"\x89PNG\r\n\x1a\n")),
        "jpg" | "jpeg" => ("image/jpeg", bytes.starts_with(b"\xff\xd8\xff")),
        "gif" => (
            "image/gif",
            bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        ),
        "webp" => (
            "image/webp",
            bytes.len() >= 12
                && bytes.starts_with(b"RIFF")
                && bytes.get(8..12) == Some(&b"WEBP"[..]),
        ),
        _ => return None,
    };
    ok.then_some(ty)
}

/// The lower-cased extension of the last segment (`None`: none).
fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// `(content type, inline)` of a file read whole.
pub(crate) fn raw_type(name: &str, bytes: &[u8]) -> (&'static str, bool) {
    if let Some(image) = image_type(name, bytes) {
        return (image, true);
    }
    if extension(name).is_some_and(|e| DOWNLOAD_ONLY.contains(&e.as_str())) {
        return (OCTET, false);
    }
    if !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok() {
        return (TEXT, true);
    }
    (OCTET, false)
}

/// `Content-Disposition` (RFC 6266): `inline` / `attachment`, an ASCII
/// `filename` (other characters as `_`) and `filename*=UTF-8''…`.
pub(crate) fn disposition(inline: bool, name: &str) -> HeaderValue {
    let mut ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-+".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if ascii.trim_matches('_').is_empty() {
        ascii = "download".to_owned();
    }
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(char::from(b));
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    let kind = if inline { "inline" } else { "attachment" };
    let value = format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}");
    // Only ASCII letters, digits and `"; =*'%._-+!#$&^`|~` went in.
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

fn content_response(
    content_type: &'static str,
    disposition: HeaderValue,
    length: Option<u64>,
    body: Body,
) -> Response {
    let mut res = (StatusCode::OK, body).into_response();
    let headers = res.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CONTENT_DISPOSITION, disposition);
    if let Some(length) = length {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    }
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(VARY, HeaderValue::from_static("Authorization, Cookie"));
    res
}

/// `GET|HEAD {path}/-/raw/{ref}/{file…}`.
pub async fn raw(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    match raw_inner(&state, request).await {
        Ok(res) => res,
        Err(err) => ctx.fail(err.with_context("route", "raw")).into_response(),
    }
}

async fn raw_inner(
    state: &AppState,
    request: Dispatched<ProjectCtx>,
) -> Result<Response, ForgeError> {
    let (smart, repository) = ready(state, &request)?;
    let smart = Arc::clone(smart);
    let (project_id, repo_id) = (request.proof().id().get(), repository.id.get());
    let ids = move |e: ForgeError| {
        e.with_context("project_id", project_id)
            .with_context("repo_id", repo_id)
    };
    let spec = smart
        .browse
        .resolve_spec(repository.id, request.tail().as_slice())
        .await
        .map_err(ids)?;
    let commit = spec.rev.commit.clone(); // clone: logged with a failure
    let path: RepoPath = spec.path;
    let name = path.file_name().unwrap_or("download").to_owned();
    let head = *request.method() == Method::HEAD;
    let browse = &smart.browse;
    if head {
        let entry = browse
            .raw_entry(repository.id, &commit, &path)
            .await
            .map_err(|e| ids(e).with_context("commit", &commit))?;
        let size = entry.size.unwrap_or(0);
        if size > browse.limits().raw_inline_max_bytes {
            return Ok(content_response(
                OCTET,
                disposition(false, &name),
                Some(size),
                Body::empty(),
            ));
        }
    }
    let principal = principal_key(&request)?;
    let raw = browse
        .raw(repository.id, &commit, &path, &principal)
        .await
        .map_err(|e| ids(e).with_context("commit", &commit))?;
    match raw.body {
        RawBody::Bytes(bytes) => {
            let (ty, inline) = raw_type(&name, &bytes);
            Ok(content_response(
                ty,
                disposition(inline, &name),
                Some(raw.size),
                Body::from(bytes),
            ))
        }
        RawBody::Stream(stream) => {
            let log = StreamLog::new(&request, repository, "raw");
            let body = GitResponse::new(Vec::new(), stream, log)?.into_body();
            Ok(content_response(
                OCTET,
                disposition(false, &name),
                Some(raw.size),
                body,
            ))
        }
    }
}

/// `GET|HEAD {path}/-/archive/{ref}.{zip|tar.gz}`.
pub async fn archive(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    match archive_inner(&state, request).await {
        Ok(res) => res,
        Err(err) => ctx
            .fail(err.with_context("route", "archive"))
            .into_response(),
    }
}

/// `[A-Za-z0-9._-]`, other characters as `-` (a ref in a file name).
fn file_safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect()
}

async fn archive_inner(
    state: &AppState,
    request: Dispatched<ProjectCtx>,
) -> Result<Response, ForgeError> {
    let (smart, repository) = ready(state, &request)?;
    let smart = Arc::clone(smart);
    let (project_id, repo_id) = (request.proof().id().get(), repository.id.get());
    let ids = move |e: ForgeError| {
        e.with_context("project_id", project_id)
            .with_context("repo_id", repo_id)
    };
    let tail = request.tail().as_slice();
    let Some((last, rest)) = tail.split_last() else {
        return Err(ids(not_resolved("archive_format")));
    };
    let Some((stem, format)) = ArchiveFormat::split(last) else {
        return Err(ids(not_resolved("archive_format")));
    };
    let mut segments: Vec<&str> = rest.iter().map(String::as_str).collect();
    segments.push(stem);
    let spec = smart
        .browse
        .resolve_spec(repository.id, &segments)
        .await
        .map_err(ids)?;
    if !spec.path.is_root() {
        return Err(ids(not_resolved("path_remainder")));
    }
    let Some(project) = request
        .canonical_path()
        .and_then(|p| p.full_path().rsplit('/').next().map(str::to_owned))
    else {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "canonical_path_missing")
            .with_context("project_id", request.proof().id().get()));
    };
    let commit = spec.rev.commit.clone(); // clone: the prefix and the log use it
    let short = commit.get(..12).unwrap_or(&commit);
    let file = format!(
        "{project}-{}.{}",
        file_safe(spec.rev.display_name()),
        format.extension()
    );
    if *request.method() == Method::HEAD {
        return Ok(content_response(
            format.content_type(),
            disposition(false, &file),
            None,
            Body::empty(),
        ));
    }
    let principal = principal_key(&request)?;
    let stream = smart
        .browse
        .archive(
            repository.id,
            &commit,
            format,
            &format!("{project}-{short}"),
            &principal,
        )
        .await
        .map_err(|e| ids(e).with_context("commit", &commit))?;
    let log = StreamLog::new(&request, repository, "archive");
    let body = GitResponse::new(Vec::new(), stream, log)?.into_body();
    Ok(content_response(
        format.content_type(),
        disposition(false, &file),
        None,
        body,
    ))
}

#[cfg(test)]
#[path = "content_tests.rs"]
mod tests;
