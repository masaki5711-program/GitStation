//! The single place errors become HTTP responses (CLAUDE.md §3-1..3).
//!
//! Body: `{ "error_code", "user_message", "request_id" }` — nothing else.
//! Context, sources, paths and git stderr stay in the log, written once here.

use axum::http::header::{HeaderValue, CONTENT_LANGUAGE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_core::{telemetry, RequestId};
use forge_errors::{ForgeError, Lang};
use serde::Serialize;

/// A failure on its way to the client.
#[derive(Debug)]
pub struct ApiError {
    err: ForgeError,
    request_id: RequestId,
    lang: Lang,
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
        }
    }

    pub fn code(&self) -> forge_errors::ErrorCode {
        self.err.code()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // The boundary: log exactly once, with the code and request id.
        telemetry::log_failure(&self.err, &self.request_id);
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
        res
    }
}
