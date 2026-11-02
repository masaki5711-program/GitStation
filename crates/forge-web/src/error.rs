//! The single place errors become HTTP responses (CLAUDE.md §3-1..3).
//!
//! Body: `{ "error_code", "user_message", "request_id" }` — nothing else.
//! Context, sources, paths and git stderr stay in the log, written once here.
//! Every 401 carries the authentication challenges (one place for them:
//! [`challenges`], applied to the whole application by [`challenge_layer`]).

use axum::extract::Request;
use axum::http::header::{HeaderValue, CONTENT_LANGUAGE, RETRY_AFTER, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_core::web_path::{parse_web_path, ParsedPath, RouteWord};
use forge_core::{telemetry, RequestId};
use forge_errors::{ForgeError, Lang};
use serde::Serialize;

const BASIC: &str = "Basic realm=\"GitStation\", charset=\"UTF-8\"";

/// The error context key whose value (whole seconds) a `429` answers as
/// `Retry-After` (M2 MR6: the resolve rate limit, refused in a loader).
pub(crate) const RETRY_AFTER_CONTEXT: &str = "retry_after_s";
const BEARER: &str = "Bearer realm=\"GitStation\"";

/// The challenges of a 401 (RFC 9110 §11.6.1) when nothing more is known
/// about the request, Basic first: git asks its credential helper only on a
/// Basic challenge, and the Basic password is a personal access token
/// (ADR-0019 §7). Never on a 503 or 403, so a client does not drop valid
/// credentials because of an outage.
const CHALLENGES: [&str; 2] = [BASIC, BEARER];

/// What a 401 answers (ADR-0019 Amendment M2, ADR-0021 §6), by the raw
/// path as the dispatcher reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChallengeClass {
    /// git Smart HTTP: always Basic and Bearer (git needs Basic).
    Git,
    /// raw / archive: Basic and Bearer, but Bearer only for a browser.
    Content,
    /// Everything else (the API): Bearer only — the API refuses Basic, and
    /// a Basic challenge would make a browser prompt for (and remember) a
    /// token.
    Api,
}

impl ChallengeClass {
    /// The class of a request path (and query: git's `service`).
    pub(crate) fn of(path: &str, query: Option<&str>) -> Self {
        match parse_web_path(path, query) {
            Ok(ParsedPath::Git { .. }) => Self::Git,
            Ok(ParsedPath::Scoped {
                word: RouteWord::Raw | RouteWord::Archive,
                ..
            }) => Self::Content,
            _ => Self::Api,
        }
    }
}

/// A browser sent the request: a navigation or subresource load
/// (`Sec-Fetch-Mode` / `Sec-Fetch-Dest`) or one carrying the session
/// cookie. Only the presence is read; values never reach a log.
pub(crate) fn browser_request(headers: &HeaderMap) -> bool {
    headers.contains_key(crate::csrf::SEC_FETCH_MODE)
        || headers.contains_key(crate::csrf::SEC_FETCH_DEST)
        || !crate::auth::session_cookies(headers).is_empty()
}

/// The `WWW-Authenticate` challenges of a 401 for `class` (the single
/// decision, ADR-0019 Amendment M2).
pub(crate) fn challenges(class: ChallengeClass, browser: bool) -> &'static [&'static str] {
    match (class, browser) {
        (ChallengeClass::Git, _) | (ChallengeClass::Content, false) => &CHALLENGES,
        (ChallengeClass::Content, true) | (ChallengeClass::Api, _) => &[BEARER],
    }
}

/// Middleware (`app::build`, inside the request-id layer): every 401 of the
/// application carries exactly the challenges of its request's class.
pub(crate) async fn challenge_layer(req: Request, next: Next) -> Response {
    let class = ChallengeClass::of(req.uri().path(), req.uri().query());
    let browser = browser_request(req.headers());
    let mut res = next.run(req).await;
    if res.status() == StatusCode::UNAUTHORIZED {
        let headers = res.headers_mut();
        headers.remove(WWW_AUTHENTICATE);
        for challenge in challenges(class, browser) {
            headers.append(WWW_AUTHENTICATE, HeaderValue::from_static(challenge));
        }
    }
    res
}

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
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry = self
                .err
                .context()
                .iter()
                .find(|(k, _)| *k == RETRY_AFTER_CONTEXT)
                .and_then(|(_, v)| v.parse::<u64>().ok());
            if let Some(secs) = retry {
                res.headers_mut()
                    .insert(RETRY_AFTER, HeaderValue::from(secs));
            }
        }
        res
    }
}
