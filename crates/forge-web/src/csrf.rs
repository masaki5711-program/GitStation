//! Cross-site request checks of state-changing requests (M2 MR3,
//! ADR-0021 §6). Two independent checks, both `FG-AUTH-0016`
//! (`CsrfRejected`, 403) with a `reason`:
//!
//! 1. **The browser check** ([`browser_origin`], a middleware on every
//!    request, before authentication): a request that is not GET / HEAD /
//!    OPTIONS and carries fetch metadata (`Sec-Fetch-Site` / `-Mode` /
//!    `-Dest`) or an `Origin` must have `Sec-Fetch-Site: same-origin` when
//!    present (`fetch_site`) and an `Origin` equal to `web.base_url`'s origin
//!    (`origin_missing`, `origin_mismatch`) — whatever its credentials:
//!    Bearer and anonymous requests included. git and `gstn` send neither
//!    header, so they are unaffected; a page of another site posting to
//!    git Smart HTTP with a remembered Basic credential is not.
//! 2. **The token check** ([`session_check`], called by the cookie
//!    authentication): a state-changing request authenticated by anything
//!    but `Authorization: Bearer` on the API or raw / archive — in practice
//!    the session cookie — must also send `Origin` (`origin_missing`) and
//!    `X-GitStation-CSRF` with the session's token (`token_missing`,
//!    `token_mismatch`; HMAC-SHA256 of the cookie's secret, compared in
//!    constant time, never stored: `forge_core::session_token`).
//!
//! Rejections are logged once, here, as `forge.auth.csrf_rejected` with
//! `request_id`, `error_code`, `reason`, `method`, the log-safe `path` and,
//! for a session, `public_id`; the expected origin (configuration) is in
//! the error context. Header values are never logged.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{ACCEPT_LANGUAGE, ORIGIN};
use axum::http::{HeaderMap, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use forge_core::session_token::{self, CSRF_HEADER};
use forge_core::token::TokenSecret;
use forge_core::{log_safe_path, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use crate::context;
use crate::error::ApiError;

pub(crate) const SEC_FETCH_SITE: &str = "sec-fetch-site";
pub(crate) const SEC_FETCH_MODE: &str = "sec-fetch-mode";
pub(crate) const SEC_FETCH_DEST: &str = "sec-fetch-dest";

/// The origin a browser's `Origin` must equal (`web.base_url`), shared by
/// both checks; `None` when it is not configured (then every browser
/// state-changing request is refused: `origin_mismatch`, `base_url=unset`).
#[derive(Debug, Clone)]
pub(crate) struct ExpectedOrigin(pub(crate) Option<Arc<str>>);

/// Why a request was refused (`reason` in the log and the error).
pub(crate) type Reason = &'static str;

/// GET, HEAD and OPTIONS change nothing (ADR-0015: no GET has side effects).
pub(crate) fn changes_state(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// The request carries a browser's marks: fetch metadata or `Origin`.
fn from_a_browser(headers: &HeaderMap) -> bool {
    [SEC_FETCH_SITE, SEC_FETCH_MODE, SEC_FETCH_DEST]
        .iter()
        .any(|h| headers.contains_key(*h))
        || headers.contains_key(ORIGIN)
}

/// `Sec-Fetch-Site`, when present, is `same-origin`; then `Origin` is
/// present (once) and equal to the expected origin.
fn site_and_origin(headers: &HeaderMap, expected: &ExpectedOrigin) -> Result<(), Reason> {
    let mut sites = headers.get_all(SEC_FETCH_SITE).iter();
    if let Some(site) = sites.next() {
        if site.as_bytes() != b"same-origin" || sites.next().is_some() {
            return Err("fetch_site");
        }
    }
    let mut origins = headers.get_all(ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return Err("origin_missing");
    };
    let Some(expected) = expected.0.as_deref() else {
        return Err("origin_mismatch");
    };
    if origins.next().is_some() || origin.as_bytes() != expected.as_bytes() {
        return Err("origin_mismatch");
    }
    Ok(())
}

/// Check 1 on `headers` (see the module documentation).
pub(crate) fn browser_check(
    method: &Method,
    headers: &HeaderMap,
    expected: &ExpectedOrigin,
) -> Result<(), Reason> {
    if !changes_state(method) || !from_a_browser(headers) {
        return Ok(());
    }
    site_and_origin(headers, expected)
}

/// Check 2 for a state-changing request authenticated by the session whose
/// cookie secret is `secret`: the browser check with `Origin` required,
/// then the CSRF token. A failure of the MAC itself is `FG-SYS-0001`
/// (`Err(Err(_))`), never a pass.
pub(crate) fn session_check(
    method: &Method,
    headers: &HeaderMap,
    expected: &ExpectedOrigin,
    secret: &TokenSecret,
) -> Result<(), Result<Reason, ForgeError>> {
    if !changes_state(method) {
        return Ok(());
    }
    site_and_origin(headers, expected).map_err(Ok)?;
    let mut tokens = headers.get_all(CSRF_HEADER).iter();
    let Some(token) = tokens.next() else {
        return Err(Ok("token_missing"));
    };
    let Ok(token) = token.to_str() else {
        return Err(Ok("token_mismatch"));
    };
    if tokens.next().is_some() {
        return Err(Ok("token_mismatch"));
    }
    match session_token::csrf_matches(secret, token) {
        Ok(true) => Ok(()),
        Ok(false) => Err(Ok("token_mismatch")),
        Err(e) => Err(Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "csrf_mac_failed")
            .with_context("source", format!("{e:?}")))),
    }
}

/// The `FG-AUTH-0016` error of `reason`, with the expected origin when the
/// origin was the problem.
pub(crate) fn rejection(reason: Reason, expected: &ExpectedOrigin) -> ForgeError {
    let err = ForgeError::new(ErrorCode::CsrfRejected).with_context("reason", reason);
    if reason == "origin_mismatch" {
        match expected.0.as_deref() {
            Some(origin) => err.with_context("expected_origin", origin),
            None => err.with_context("base_url", "unset"),
        }
    } else {
        err
    }
}

/// Log the refusal once as `forge.auth.csrf_rejected` and answer 403.
pub(crate) fn rejected(
    err: ForgeError,
    method: &Method,
    path: &str,
    public_id: Option<&str>,
    request_id: RequestId,
    lang: forge_errors::Lang,
) -> Response {
    let reason = err
        .context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.as_str());
    tracing::info!(
        target: "forge_web::csrf",
        error_code = err.code().id(),
        request_id = %request_id,
        reason,
        method = %method,
        path = %log_safe_path(path),
        public_id,
        "forge.auth.csrf_rejected"
    );
    ApiError::new(err, request_id, lang)
        .already_logged()
        .into_response()
}

/// Middleware: check 1 for every request (`app::build`, inside the
/// request-id layer, before any route or authentication).
pub(crate) async fn browser_origin(
    State(expected): State<ExpectedOrigin>,
    req: Request,
    next: Next,
) -> Response {
    match browser_check(req.method(), req.headers(), &expected) {
        Ok(()) => next.run(req).await,
        Err(reason) => {
            let request_id = req
                .extensions()
                .get::<RequestId>()
                .cloned()
                .unwrap_or_else(RequestId::generate);
            let lang = context::language(req.headers().get(ACCEPT_LANGUAGE));
            rejected(
                rejection(reason, &expected),
                req.method(),
                req.uri().path(),
                None,
                request_id,
                lang,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly (rust/testing.md)

    use axum::http::HeaderValue;

    use super::*;

    fn expected() -> ExpectedOrigin {
        ExpectedOrigin(Some(Arc::from("https://git.example.test")))
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).expect("header"));
        }
        h
    }

    fn check(pairs: &[(&'static str, &str)]) -> Result<(), Reason> {
        browser_check(&Method::POST, &headers(pairs), &expected())
    }

    #[test]
    fn safe_methods_and_non_browser_requests_are_not_checked() {
        let cross = headers(&[("sec-fetch-site", "cross-site")]);
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert_eq!(browser_check(&m, &cross, &expected()), Ok(()));
        }
        assert_eq!(
            browser_check(&Method::POST, &HeaderMap::new(), &expected()),
            Ok(())
        );
    }

    #[test]
    fn the_browser_check_needs_same_origin_and_the_exact_origin() {
        assert_eq!(
            check(&[
                ("sec-fetch-site", "same-origin"),
                ("origin", "https://git.example.test")
            ]),
            Ok(())
        );
        assert_eq!(check(&[("origin", "https://git.example.test")]), Ok(()));
        for site in ["same-site", "cross-site", "none"] {
            let h = [
                ("sec-fetch-site", site),
                ("origin", "https://git.example.test"),
            ];
            assert_eq!(check(&h), Err("fetch_site"), "{site}");
        }
        assert_eq!(check(&[("sec-fetch-mode", "cors")]), Err("origin_missing"));
        for origin in [
            "https://other.example.test",
            "http://git.example.test",
            "https://git.example.test:8443",
            "https://git.example.test.evil.test",
            "null",
        ] {
            assert_eq!(
                check(&[("origin", origin)]),
                Err("origin_mismatch"),
                "{origin}"
            );
        }
        let twice = [
            ("origin", "https://git.example.test"),
            ("origin", "https://git.example.test"),
        ];
        assert_eq!(check(&twice), Err("origin_mismatch"));
        let unset = ExpectedOrigin(None);
        assert_eq!(
            browser_check(
                &Method::POST,
                &headers(&[("origin", "https://git.example.test")]),
                &unset
            ),
            Err("origin_mismatch")
        );
    }

    #[test]
    fn the_session_check_needs_origin_and_this_sessions_token() {
        let session = session_token::generate().expect("rng");
        let token = session.csrf_token().expect("mac");
        let check = |h: &HeaderMap| {
            session_check(&Method::DELETE, h, &expected(), session.secret())
                .map_err(|e| e.expect("no MAC failure"))
        };
        let mut ok = headers(&[("origin", "https://git.example.test")]);
        ok.insert(CSRF_HEADER, HeaderValue::from_str(&token).expect("ascii"));
        assert_eq!(check(&ok), Ok(()));
        assert_eq!(
            check(&headers(&[("origin", "https://git.example.test")])),
            Err("token_missing")
        );
        let mut wrong = headers(&[("origin", "https://git.example.test")]);
        wrong.insert(CSRF_HEADER, HeaderValue::from_static("00"));
        assert_eq!(check(&wrong), Err("token_mismatch"));
        let mut no_origin = HeaderMap::new();
        no_origin.insert(CSRF_HEADER, HeaderValue::from_str(&token).expect("ascii"));
        assert_eq!(check(&no_origin), Err("origin_missing"));
        assert!(
            session_check(
                &Method::GET,
                &HeaderMap::new(),
                &expected(),
                session.secret()
            )
            .is_ok(),
            "a GET is never checked"
        );
    }
}
