//! The single place errors become HTTP responses (CLAUDE.md §3-1..3).
//!
//! Body: `{ "error_code", "user_message", "request_id" }` — nothing else,
//! except the integration feed's 410 (`FG-INT-0007`), which adds its fixed
//! `reason` word (plan 02 §11.2: the consumer recovers by it), and a
//! stale required check (`FG-CI-0013`, plan 18 S5), which adds its fixed
//! `hint` word (`merge_queue`: the next step a client can take).
//! A failed browser navigation of the sign-in flow (`/-/auth/…`) shows the
//! same `user_message` and `request_id` as an HTML page (`crate::error_page`).
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

/// The error context key whose value (whole seconds) a `429` or a `503`
/// answers as `Retry-After` (M2 MR6: the resolve rate limit, refused in a
/// loader; 2026-11-17 F1 / F4: forge-git's busy refusals, `FG-GIT-0004`).
/// The same word as forge-git's retry hint key (pinned by
/// `git_stream_tests`; this file does not use forge-git, ADR-0015).
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
    /// Only `FG-INT-0007` (the feed's 410, plan 02 §11.2): why the cursor
    /// cannot continue — one of [`FEED_GONE_REASONS`], never anything else.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    /// Only `FG-CI-0013` (plan 18 S5): the next step — one of [`HINTS`],
    /// never anything else.
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'static str>,
}

/// The `hint` values an error body may show (`forge_review`'s
/// `merges::evaluate::HINT_MERGE_QUEUE`, pinned by a test).
pub(crate) const HINTS: [&str; 1] = ["merge_queue"];

/// The public `hint` of a refusal (a fixed word from the context, only for
/// `FG-CI-0013`; anything else is not shown).
fn hint(err: &ForgeError) -> Option<&'static str> {
    if err.code() != forge_errors::ErrorCode::RequiredCheckStale {
        return None;
    }
    let hint = err.context().iter().find(|(k, _)| *k == "hint")?;
    HINTS.into_iter().find(|known| *known == hint.1.as_str())
}

/// The `reason` values a 410 of the integration feed may show.
pub(crate) const FEED_GONE_REASONS: [&str; 4] = [
    "retention",
    "epoch_mismatch",
    "ahead_of_head",
    "visible_set_changed",
];

/// The public `reason` of a feed 410 (a fixed word from the context;
/// anything else is not shown).
fn feed_gone_reason(err: &ForgeError) -> Option<&'static str> {
    if err.code() != forge_errors::ErrorCode::FeedCursorExpired {
        return None;
    }
    let reason = err.context().iter().find(|(k, _)| *k == "reason")?;
    FEED_GONE_REASONS
        .into_iter()
        .find(|known| *known == reason.1.as_str())
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

/// What a failure answered, as a response extension (M3 S2: the act-as
/// layer's one log line names the route's code and reason). Never sent.
#[derive(Debug, Clone)]
pub(crate) struct Failure {
    pub(crate) code: forge_errors::ErrorCode,
    pub(crate) reason: Option<String>,
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
            reason: feed_gone_reason(&self.err),
            hint: hint(&self.err),
        };
        let view = crate::error_page::ErrorView {
            user_message: body.user_message.to_owned(),
            request_id: body.request_id.to_owned(),
            lang: self.lang,
        };
        let mut res = (status, Json(body)).into_response();
        // A browser navigation of the sign-in flow shows it as a page
        // (`crate::error_page`); never sent as it is.
        res.extensions_mut().insert(view);
        res.extensions_mut().insert(Failure {
            code,
            reason: self
                .err
                .context()
                .iter()
                .find(|(k, _)| *k == "reason")
                .map(|(_, v)| v.clone()),
        });
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
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
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

#[cfg(test)]
mod retry_after_tests {
    use axum::http::header::RETRY_AFTER;
    use axum::http::StatusCode;
    use axum::response::IntoResponse as _;
    use forge_core::RequestId;
    use forge_errors::{ErrorCode, ForgeError, Lang};

    use super::{ApiError, RETRY_AFTER_CONTEXT};

    fn answer(err: ForgeError) -> axum::response::Response {
        ApiError::new(err, RequestId::generate(), Lang::En)
            .already_logged()
            .into_response()
    }

    /// 2026-11-17 F1 / F4: a busy git refusal tells the client when to
    /// try again; the body stays `error_code` / `user_message` /
    /// `request_id`.
    #[test]
    fn a_busy_503_with_a_retry_hint_answers_retry_after() {
        let res = answer(
            ForgeError::new(ErrorCode::GitBusy)
                .with_context("reason", "principal_streams")
                .with_context(RETRY_AFTER_CONTEXT, 1),
        );
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            res.headers().get(RETRY_AFTER).map(|v| v.as_bytes()),
            Some(&b"1"[..])
        );
    }

    #[test]
    fn a_503_without_a_retry_hint_has_no_retry_after() {
        let res = answer(ForgeError::new(ErrorCode::GitBusy).with_context("reason", "streams"));
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(res.headers().get(RETRY_AFTER).is_none());
    }
}

#[cfg(test)]
mod hint_tests {
    #![allow(clippy::expect_used)] // tests may fail loudly
    use axum::response::IntoResponse as _;
    use forge_core::RequestId;
    use forge_errors::{ErrorCode, ForgeError, Lang};
    use http_body_util::BodyExt as _;

    use super::ApiError;

    async fn body(err: ForgeError) -> serde_json::Value {
        let res = ApiError::new(err, RequestId::generate(), Lang::En)
            .already_logged()
            .into_response();
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("json")
    }

    /// Plan 18 S5: a direct merge refused because the merge request
    /// pipeline ran on an older target answers FG-CI-0013 with the fixed
    /// `hint` word the merge evaluation put in the context; the body names
    /// nothing else of the context.
    #[tokio::test]
    async fn a_stale_check_body_carries_the_queue_hint() {
        let refused = forge_review::merges::evaluate::blocked(&[
            forge_review::BlockReason::RequiredCheckStale {
                name: forge_core::CheckName::parse("ci/pipeline").expect("name"),
            },
        ]);
        let json = body(refused).await;
        assert_eq!(json["error_code"], ErrorCode::RequiredCheckStale.id());
        assert_eq!(json["hint"], "merge_queue");
        assert!(json.get("reasons").is_none() && json.get("details").is_none());
        // Another code with the same context word shows no hint, and a word
        // outside the closed list is never shown.
        let other =
            body(ForgeError::new(ErrorCode::MergeBlocked).with_context("hint", "merge_queue"))
                .await;
        assert!(other.get("hint").is_none());
        let unknown =
            body(ForgeError::new(ErrorCode::RequiredCheckStale).with_context("hint", "x")).await;
        assert!(unknown.get("hint").is_none());
    }
}
