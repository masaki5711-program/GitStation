//! Request bodies of the API's write routes (M1 phase 8b MR6): read,
//! bounded and typed **before** anything is loaded, because a write's
//! authorization inputs (a role to grant, a protection level, a parent)
//! live in the body.
//!
//! [`read_json`] — called by the registry for a `route_with_body` route,
//! after authentication — refuses, in this order, each as one logged
//! failure and without reading further:
//! 1. a `Content-Type` other than `application/json` (optionally with
//!    `charset=utf-8`): `FG-WEB-0006 reason=content_type`;
//! 2. a `Content-Length` above the route's cap: `FG-WEB-0009
//!    declared=true`; a body that grows past it while streaming:
//!    `FG-WEB-0009 declared=false` (the cap is the route's, at most
//!    [`MAX_JSON_BODY_BYTES`], and never above the server's
//!    `body_limit_bytes`);
//! 3. a body that stops arriving (the server's read-idle timeout,
//!    `FG-WEB-0002 operation=request_read kind=idle`), takes longer than
//!    `server.body_read_timeout_ms` in total (`kind=total`), or whose
//!    client goes away or breaks the protocol (`kind=aborted`);
//! 4. JSON that does not parse into the route's type, which refuses
//!    unknown fields (`#[serde(deny_unknown_fields)]`, checked for every
//!    body route by the registry tests through the OpenAPI schema):
//!    `FG-WEB-0006 reason=json_syntax | unknown_field | missing_field |
//!    type`. The log carries field names and positions, never values.

use axum::body::Body;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::request::Parts;
use forge_errors::{ErrorCode, ForgeError};
use futures_util::StreamExt as _;
use serde::de::DeserializeOwned;

/// The largest cap a JSON body route may declare.
pub const MAX_JSON_BODY_BYTES: usize = 64 * 1024;

/// A route's request body type: strict JSON (`deny_unknown_fields`) with an
/// OpenAPI schema.
pub trait RequestBody: DeserializeOwned + utoipa::ToSchema + Send + Sync + 'static {}

impl<T: DeserializeOwned + utoipa::ToSchema + Send + Sync + 'static> RequestBody for T {}

/// The total time a JSON body may take to arrive
/// (`AppConfig::body_read_timeout`), a request extension set by
/// `app::build`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BodyReadTimeout(pub(crate) std::time::Duration);

/// Used when the extension is absent (a route built outside `app::build`).
const DEFAULT_BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The server-wide body limit (`AppConfig::body_limit_bytes`), a request
/// extension set by `app::build`: no route reads more, whatever its cap.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BodyLimit(pub(crate) usize);

/// `FG-WEB-0006` with a `reason`.
pub(crate) fn request_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid).with_context("reason", reason)
}

/// `FG-WEB-0006 reason=param_invalid` for a path or query parameter.
pub(crate) fn param_invalid(param: &'static str, rule: &'static str) -> ForgeError {
    request_invalid("param_invalid")
        .with_context("param", param)
        .with_context("rule", rule)
}

/// A body stream error: the server's read-idle timeout (`kind=idle`, logged
/// once more with the peer by the connection loop), or the client went away
/// or broke the protocol while sending (`kind=aborted`; the error is in
/// `source`).
fn read_failed(e: axum::Error, bytes_in: usize) -> ForgeError {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    let mut idle = false;
    while let Some(c) = cause {
        if matches!(
            c.downcast_ref::<crate::server::ReadError>(),
            Some(crate::server::ReadError::Idle)
        ) {
            idle = true;
            break;
        }
        cause = c.source();
    }
    ForgeError::new(ErrorCode::RequestTimeout)
        .with_context("operation", "request_read")
        .with_context("kind", if idle { "idle" } else { "aborted" })
        .with_context("bytes_in", bytes_in)
        .with_source(e)
}

/// Read and parse the body of a JSON route (see the module docs).
pub(crate) async fn read_json<B: DeserializeOwned>(
    parts: &Parts,
    body: Body,
    route_cap: usize,
) -> Result<B, ForgeError> {
    check_content_type(parts)?;
    let cap = parts
        .extensions
        .get::<BodyLimit>()
        .map_or(route_cap, |l| route_cap.min(l.0));
    let too_large = |declared: bool| {
        ForgeError::new(ErrorCode::RequestTooLarge)
            .with_context("limit_bytes", cap)
            .with_context("declared", declared)
    };
    if let Some(length) = parts.headers.get(CONTENT_LENGTH) {
        // hyper already refused a malformed Content-Length; anything that
        // is not a number here is treated as too large rather than trusted.
        let declared = length
            .to_str()
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        if declared > cap as u64 {
            return Err(too_large(true));
        }
    }
    let total = parts
        .extensions
        .get::<BodyReadTimeout>()
        .map_or(DEFAULT_BODY_READ_TIMEOUT, |t| t.0);
    let mut bytes = Vec::new();
    let read = async {
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| read_failed(e, bytes.len()))?;
            if bytes.len().saturating_add(chunk.len()) > cap {
                return Err(too_large(false));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(())
    };
    match tokio::time::timeout(total, read).await {
        Ok(read) => read?,
        Err(_) => {
            return Err(ForgeError::new(ErrorCode::RequestTimeout)
                .with_context("operation", "request_read")
                .with_context("kind", "total")
                .with_context("timeout_ms", total.as_millis())
                .with_context("bytes_in", bytes.len()));
        }
    }
    parse(&bytes)
}

/// `application/json`, optionally `; charset=utf-8` (JSON is UTF-8, RFC
/// 8259); exactly one header.
fn check_content_type(parts: &Parts) -> Result<(), ForgeError> {
    let mut values = parts.headers.get_all(CONTENT_TYPE).iter();
    let refused = |shown: &str| {
        let shown: String = shown
            .chars()
            .filter(|c| c.is_ascii_graphic() || *c == ' ')
            .take(64)
            .collect();
        request_invalid("content_type").with_context("content_type", shown)
    };
    let (Some(value), None) = (values.next(), values.next()) else {
        return Err(refused(if parts.headers.contains_key(CONTENT_TYPE) {
            "<repeated>"
        } else {
            "<missing>"
        }));
    };
    let text = value.to_str().map_err(|_| refused("<not_ascii>"))?;
    let mut fields = text.split(';').map(str::trim);
    let media = fields.next().unwrap_or_default();
    let params_ok = fields.all(|p| {
        p.split_once('=').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("charset")
                && v.trim().trim_matches('"').eq_ignore_ascii_case("utf-8")
        })
    });
    if media.eq_ignore_ascii_case("application/json") && params_ok {
        Ok(())
    } else {
        Err(refused(media))
    }
}

/// Parse the bytes; classify a failure without its message (serde's
/// messages quote values: `invalid type: string "…"`).
fn parse<B: DeserializeOwned>(bytes: &[u8]) -> Result<B, ForgeError> {
    serde_json::from_slice(bytes).map_err(|e| {
        let at = |err: ForgeError| {
            err.with_context("line", e.line())
                .with_context("column", e.column())
        };
        match e.classify() {
            serde_json::error::Category::Data => {
                let text = e.to_string();
                if let Some(field) = quoted_field(&text, "unknown field `") {
                    request_invalid("unknown_field").with_context("field", field)
                } else if let Some(field) = quoted_field(&text, "missing field `") {
                    request_invalid("missing_field").with_context("field", field)
                } else if let Some(field) = quoted_field(&text, "duplicate field `") {
                    at(request_invalid("json_syntax")
                        .with_context("rule", "duplicate_field")
                        .with_context("field", field))
                } else {
                    at(request_invalid("type"))
                }
            }
            serde_json::error::Category::Syntax
            | serde_json::error::Category::Eof
            | serde_json::error::Category::Io => at(request_invalid("json_syntax")),
        }
    })
}

/// The field name serde quotes after `prefix` — a name only, shown when it
/// looks like one (`[A-Za-z0-9_.-]`, at most 64 characters), else
/// `<not_shown>`: an unknown key is chosen by the client.
fn quoted_field(message: &str, prefix: &str) -> Option<String> {
    let rest = message.strip_prefix(prefix)?;
    let name = rest.split('`').next()?;
    let plain = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    Some(if plain {
        name.to_owned()
    } else {
        "<not_shown>".to_owned()
    })
}

#[cfg(test)]
#[path = "body_tests.rs"]
mod tests;
