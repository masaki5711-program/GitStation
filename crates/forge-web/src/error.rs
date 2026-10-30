//! The single place errors become HTTP responses (CLAUDE.md §3-1..3).
//!
//! Body: `{ "error_code", "user_message", "request_id" }` — nothing else.
//! Context, sources, paths and git stderr stay in the log, written once here.
//! Every 401 carries the authentication challenges (one place for them).

use axum::http::header::{HeaderValue, CONTENT_LANGUAGE, WWW_AUTHENTICATE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_core::{telemetry, RequestId};
use forge_errors::{ForgeError, Lang};
use serde::Serialize;

/// The challenges of a 401 (RFC 9110 §11.6.1), Basic first: git asks its
/// credential helper only on a Basic challenge, and the Basic password is a
/// personal access token (ADR-0019 §7). Never on a 503 or 403, so a client
/// does not drop valid credentials because of an outage.
const CHALLENGES: [&str; 2] = [
    "Basic realm=\"GitStation\", charset=\"UTF-8\"",
    "Bearer realm=\"GitStation\"",
];

/// A failure on its way to the client.
#[derive(Debug)]
pub struct ApiError {
    err: ForgeError,
    request_id: RequestId,
    lang: Lang,
    /// `false` when the same failure was already logged (e.g. an ongoing
    /// readiness failure answered from the cache).
    log: bool,
}

#[derive(Serialize)]
struct Body<'a> {
    error_code: &'a str,
    user_message: &'a str,
    request_id: &'a str,
}

impl ApiError {
    pub fn new(err: ForgeError, request_id: RequestId, lang: Lang) -> Self {
        Self {
            err,
            request_id,
            lang,
            log: true,
        }
    }

    /// The failure was already logged where it was first observed; answer
    /// with the same body without logging it again.
    #[must_use]
    pub fn already_logged(mut self) -> Self {
        self.log = false;
        self
    }

    pub fn code(&self) -> forge_errors::ErrorCode {
        self.err.code()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // The boundary: log exactly once, with the code and request id.
        if self.log {
            telemetry::log_failure(&self.err, &self.request_id);
        }
        let code = self.err.code();
        let status =
            StatusCode::from_u16(code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = Body {
            error_code: code.id(),
            user_message: self.err.user_message(self.lang),
            request_id: self.request_id.as_str(),
        };
        let mut res = (status, Json(body)).into_response();
        res.headers_mut().insert(
            CONTENT_LANGUAGE,
            HeaderValue::from_static(self.lang.as_str()),
        );
        if status == StatusCode::UNAUTHORIZED {
            for challenge in CHALLENGES {
                res.headers_mut()
                    .append(WWW_AUTHENTICATE, HeaderValue::from_static(challenge));
            }
        }
        res
    }
}
