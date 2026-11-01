//! The browser sign-in and the logouts (ADR-0021 §3, §8): `GET
//! /-/auth/login` and `GET /-/auth/callback`, both `WebLogin` (public: the
//! browser has no session yet; M2 MR4); `POST /-/auth/logout`
//! (`SessionLogout`, sessions only) and `POST /-/auth/backchannel-logout`
//! (`OidcBackchannelLogout`, public; M2 MR5). The decisions are
//! forge-oidc's; this module is HTTP only — the query, the form bodies,
//! the cookies, the redirects, the rate limit of the start.
//!
//! - Login: at most [`LOGIN_LIMIT`] starts per peer (an IPv6 peer counts as
//!   its /64) per [`LOGIN_WINDOW`] (`FG-WEB-0010`, `Retry-After`); then a
//!   302 to the provider with the sealed state cookie `__Host-gstn_oidc`
//!   (HttpOnly, Secure, SameSite=Lax, `Path=/`, 10 minutes).
//! - Callback: the same limit per peer, counted apart from the starts
//!   (a callback costs a token request and a JWKS lookup at the provider);
//!   every answer clears the state cookie (single use); success
//!   sets `__Host-gstn_session` (Max-Age = the absolute lifetime) and 302s
//!   to `next`. Failures carry `event=forge.auth.login_failed` and are
//!   logged once by the error renderer.
//! - Logout: the page posts a form whose `csrf` field is the session's
//!   CSRF token (moved into `X-GitStation-CSRF` by [`crate::auth`] before
//!   the one CSRF check); the session is revoked (`reason=logout`), the
//!   cookie cleared, and a 303 sends the browser to the provider's
//!   `end_session_endpoint` with `id_token_hint` (home without one). A
//!   request without a session is the guard's `FG-AUTH-0001`.
//! - Back-channel logout: an `application/x-www-form-urlencoded` body of
//!   at most 64 KiB with one `logout_token`; cookies and `Authorization`
//!   are never read. 200 (empty) when the token is valid, whatever it
//!   revoked; `FG-AUTH-0017` (400, `event=forge.auth.backchannel_logout_failed`)
//!   otherwise.
//! - Without `auth.oidc`, all four answer `FG-AUTH-0018` (503, info).
//!
//! Every answer is `Cache-Control: no-store`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::header::{CACHE_CONTROL, LOCATION, RETRY_AFTER, SET_COOKIE, USER_AGENT};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use forge_auth::InstanceCtx;
use forge_core::session_token::SESSION_COOKIE;
use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use forge_oidc::{CallbackInput, STATE_COOKIE, STATE_MAX_AGE_SECS};

use crate::app::AppState;
use crate::auth::{cookies_named, session_cookies};
use crate::registry::Dispatched;

pub const LOGIN_PATH: &str = "/-/auth/login";
pub const CALLBACK_PATH: &str = forge_oidc::CALLBACK_PATH;

/// Login starts per peer per window (ADR-0021 §3).
pub const LOGIN_LIMIT: u32 = 20;
pub const LOGIN_WINDOW: Duration = Duration::from_secs(60);
/// Peers remembered at once; beyond it expired windows are dropped first,
/// then the peer whose window started first (a full table never refuses
/// everyone).
const MAX_PEERS: usize = 10_000;

const CLEAR_STATE_COOKIE: &str =
    "__Host-gstn_oidc=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Lax";

/// The fixed-window counter of login starts (or callbacks) per peer.
#[derive(Debug, Default)]
pub struct LoginLimiter {
    peers: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    /// The poisoned table was logged (once per process).
    poison_logged: AtomicBool,
}

impl LoginLimiter {
    /// Count one start of `peer`; `Err(retry_after_secs)` over the limit.
    pub fn check(&self, peer: IpAddr, now: Instant) -> Result<(), u64> {
        let key = crate::server::peer_key(peer);
        let Ok(mut peers) = self.peers.lock() else {
            // A poisoned table (a panic while it was held): refuse rather
            // than run unlimited, and say so once — every later refusal is
            // the same cause.
            if !self.poison_logged.swap(true, Ordering::Relaxed) {
                telemetry::log_failure(
                    &ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "login_limiter_poisoned"),
                    &RequestId::generate(),
                );
            }
            return Err(LOGIN_WINDOW.as_secs());
        };
        if peers.len() >= MAX_PEERS && !peers.contains_key(&key) {
            peers.retain(|_, (start, _)| now.duration_since(*start) < LOGIN_WINDOW);
            if peers.len() >= MAX_PEERS {
                let oldest = peers
                    .iter()
                    .min_by_key(|(_, (start, _))| *start)
                    .map(|(peer, _)| *peer);
                if let Some(oldest) = oldest {
                    peers.remove(&oldest);
                }
            }
        }
        let entry = peers.entry(key).or_insert((now, 0));
        if now.duration_since(entry.0) >= LOGIN_WINDOW {
            *entry = (now, 0);
        }
        if entry.1 >= LOGIN_LIMIT {
            let left = LOGIN_WINDOW.saturating_sub(now.duration_since(entry.0));
            return Err(left.as_secs().max(1));
        }
        entry.1 += 1;
        Ok(())
    }
}

fn rate_limited(request: &Dispatched<InstanceCtx>, route: &'static str, retry: u64) -> Response {
    let err = ForgeError::new(ErrorCode::RateLimited)
        .with_context("route", route)
        .with_context("limit", LOGIN_LIMIT)
        .with_context("window_s", LOGIN_WINDOW.as_secs());
    // The callback's refusal also clears the state cookie: the login has
    // to start again anyway.
    let mut res = fail(request, err, route == "callback");
    res.headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from(retry));
    res
}

fn not_configured(route: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::LoginNotConfigured).with_context("route", route)
}

fn no_store(res: &mut Response) {
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

/// The query parameter `name` (decoded), the first occurrence.
fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

fn fail(request: &Dispatched<InstanceCtx>, err: ForgeError, clear_state: bool) -> Response {
    let err = err.with_context("event", "forge.auth.login_failed");
    let mut res = request.ctx().fail(err).into_response();
    if clear_state {
        res.headers_mut()
            .append(SET_COOKIE, HeaderValue::from_static(CLEAR_STATE_COOKIE));
    }
    no_store(&mut res);
    res
}

fn redirect(location: &str) -> Result<Response, ForgeError> {
    let value = HeaderValue::from_str(location).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "location_not_a_header")
    })?;
    let mut res = StatusCode::FOUND.into_response();
    res.headers_mut().insert(LOCATION, value);
    no_store(&mut res);
    Ok(res)
}

fn cookie(value: String) -> Result<HeaderValue, ForgeError> {
    HeaderValue::from_str(&value).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "cookie_not_a_header")
    })
}

/// `GET /-/auth/login?next=`.
pub async fn login(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let Some(oidc) = state.oidc.clone() else {
        return fail(&request, not_configured("login"), false);
    };
    let Some(peer) = request.peer() else {
        let err = ForgeError::new(ErrorCode::Internal).with_context("reason", "peer_missing");
        return fail(&request, err, false);
    };
    if let Err(retry) = state.login_limiter.check(peer, Instant::now()) {
        return rate_limited(&request, "login", retry);
    }
    let next = query_param(request.raw_query(), "next");
    let start = match oidc.start(next.as_deref()).await {
        Ok(start) => start,
        Err(e) => return fail(&request, e, false),
    };
    if start.next_rejected {
        tracing::info!(
            target: "forge_web::auth",
            request_id = %request.ctx().request_id,
            reason = "next_rejected",
            "forge.auth.login_started"
        );
    }
    let built = redirect(&start.location).and_then(|mut res| {
        let value = cookie(format!(
            "{STATE_COOKIE}={}; Max-Age={STATE_MAX_AGE_SECS}; Path=/; Secure; HttpOnly; SameSite=Lax",
            start.state_cookie
        ))?;
        res.headers_mut().append(SET_COOKIE, value);
        Ok(res)
    });
    built.unwrap_or_else(|e| fail(&request, e, false))
}

/// `GET /-/auth/callback?code=&state=` (or `error=`).
pub async fn callback(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let Some(oidc) = state.oidc.clone() else {
        return fail(&request, not_configured("callback"), true);
    };
    let Some(peer) = request.peer() else {
        let err = ForgeError::new(ErrorCode::Internal).with_context("reason", "peer_missing");
        return fail(&request, err, true);
    };
    if let Err(retry) = state.callback_limiter.check(peer, Instant::now()) {
        return rate_limited(&request, "callback", retry);
    }
    let query = request.raw_query();
    let code = query_param(query, "code");
    let state_param = query_param(query, "state");
    let error = query_param(query, "error");
    let state_cookie = cookies_named(request.headers(), STATE_COOKIE)
        .into_iter()
        .next();
    let sessions = session_cookies(request.headers());
    // Two session cookies: neither is trusted to be the browser's own.
    let session_cookie = sessions.first().filter(|_| sessions.len() == 1).cloned();
    let user_agent = request
        .headers()
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let request_id = request.ctx().request_id.clone();
    let done = oidc
        .complete(CallbackInput {
            request_id: &request_id,
            code: code.as_deref(),
            state: state_param.as_deref(),
            error: error.as_deref(),
            state_cookie: state_cookie.as_deref(),
            session_cookie: session_cookie.as_deref(),
            source_ip: request.peer(),
            user_agent: user_agent.as_deref(),
        })
        .await;
    let done = match done {
        Ok(done) => done,
        Err(e) => return fail(&request, e, true),
    };
    let built = redirect(&done.next).and_then(|mut res| {
        let session = cookie(format!(
            "{SESSION_COOKIE}={}; Max-Age={}; Path=/; Secure; HttpOnly; SameSite=Lax",
            done.token.reveal(),
            done.max_age.as_secs()
        ))?;
        let headers = res.headers_mut();
        headers.append(SET_COOKIE, HeaderValue::from_static(CLEAR_STATE_COOKIE));
        headers.append(SET_COOKIE, session);
        Ok(res)
    });
    built.unwrap_or_else(|e| fail(&request, e, true))
}

// --- M2 MR5: the logouts (ADR-0021 §8) ---

pub const LOGOUT_PATH: &str = forge_oidc::LOGOUT_PATH;
pub const BACKCHANNEL_LOGOUT_PATH: &str = forge_oidc::BACKCHANNEL_LOGOUT_PATH;

/// The form field that carries the CSRF token of `POST /-/auth/logout`
/// (the page posts a `<form>`, not a fetch; [`crate::auth`] moves it into
/// `X-GitStation-CSRF` before the CSRF check).
pub const LOGOUT_CSRF_FIELD: &str = "csrf";

/// The largest logout form (`csrf=<64 hex>` and a little room).
pub const MAX_LOGOUT_FORM_BYTES: usize = 4 * 1024;

fn see_other(location: &str) -> Result<Response, ForgeError> {
    let value = HeaderValue::from_str(location).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "location_not_a_header")
    })?;
    let mut res = StatusCode::SEE_OTHER.into_response();
    res.headers_mut().insert(LOCATION, value);
    no_store(&mut res);
    Ok(res)
}

/// `POST /-/auth/logout` (`SessionLogout`, sessions only): revoke the
/// request's own session (`reason=logout`), clear the cookie, and 303 to
/// the provider's logout with `id_token_hint` (the ID token is only ever
/// in this `Location`, never in a body). Without `auth.oidc` it is
/// `FG-AUTH-0018` and the session is left as it is (ADR-0021 row 30).
pub async fn logout(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the request is consumed below
    let fail = |err: ForgeError| {
        let mut res = ctx
            .fail(err.with_context("event", "forge.auth.logout_failed"))
            .into_response();
        no_store(&mut res);
        res
    };
    let (Some(user), Some(session)) = (request.principal().user(), request.session()) else {
        // `prove(SessionLogout)` allowed an active session principal.
        return fail(
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "session_credential_missing"),
        );
    };
    let session_id = session.session_id;
    let public_id = session.public_id.clone(); // clone: logged after the change
    let user_agent = request
        .headers()
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let location = match state.oidc.clone() {
        Some(oidc) => {
            oidc.logout(forge_oidc::LogoutRequest {
                request_id: &ctx.request_id,
                user,
                session_id,
                source_ip: request.peer(),
                user_agent: user_agent.as_deref(),
            })
            .await
        }
        None => Err(not_configured("logout")),
    };
    let location = match location {
        Ok(location) => location,
        Err(e) => return fail(e),
    };
    tracing::info!(
        target: "forge_web::auth",
        request_id = %ctx.request_id,
        user_id = user.get(),
        public_id = %public_id,
        "forge.auth.logged_out"
    );
    let built = see_other(&location).map(|mut res| {
        res.headers_mut().append(
            SET_COOKIE,
            HeaderValue::from_static(crate::auth::CLEAR_SESSION_COOKIE),
        );
        res
    });
    built.unwrap_or_else(fail)
}

fn logout_body(detail: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::LogoutTokenInvalid)
        .with_context("reason", "body")
        .with_context("detail", detail)
}

/// The one `logout_token` of an `application/x-www-form-urlencoded` body.
fn logout_token_of(content_type: Option<&str>, body: &[u8]) -> Result<String, ForgeError> {
    let form = content_type.and_then(|c| c.split(';').next()).map(|c| {
        c.trim()
            .eq_ignore_ascii_case("application/x-www-form-urlencoded")
    });
    if form != Some(true) {
        return Err(logout_body("content_type"));
    }
    let mut tokens = url::form_urlencoded::parse(body)
        .filter(|(k, _)| k == "logout_token")
        .map(|(_, v)| v.into_owned());
    match (tokens.next(), tokens.next()) {
        (Some(token), None) if !token.is_empty() => Ok(token),
        (Some(_), Some(_)) => Err(logout_body("form")),
        _ => Err(logout_body("token_missing")),
    }
}

/// `POST /-/auth/backchannel-logout` (`OidcBackchannelLogout`, public:
/// the signed logout token is the proof; cookies and `Authorization` are
/// never read): 200 with an empty body when the token is valid, however
/// many sessions it ended; `FG-AUTH-0017` (400) otherwise. `no-store`.
pub async fn backchannel_logout(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the request is consumed by the body read
    let fail = |err: ForgeError| {
        let mut res = ctx
            .fail(err.with_context("event", "forge.auth.backchannel_logout_failed"))
            .into_response();
        no_store(&mut res);
        res
    };
    let Some(oidc) = state.oidc.clone() else {
        return fail(not_configured("backchannel_logout"));
    };
    let content_type = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let peer = request.peer();
    let body =
        match axum::body::to_bytes(request.into_body(), forge_oidc::MAX_LOGOUT_BODY_BYTES).await {
            Ok(body) => body,
            Err(_) => return fail(logout_body("too_large")),
        };
    let token = match logout_token_of(content_type.as_deref(), &body) {
        Ok(token) => token,
        Err(e) => return fail(e),
    };
    match oidc.backchannel_logout(&ctx.request_id, &token, peer).await {
        Ok(_revoked) => {
            let mut res = StatusCode::OK.into_response();
            no_store(&mut res);
            res
        }
        Err(e) => fail(e),
    }
}

#[cfg(test)]
mod logout_tests {
    use super::*;

    const FORM: Option<&str> = Some("application/x-www-form-urlencoded");

    fn detail(err: &ForgeError) -> String {
        err.context()
            .iter()
            .find(|(k, _)| *k == "detail")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn the_logout_token_is_the_one_form_field() {
        assert_eq!(
            logout_token_of(FORM, b"logout_token=a.b.c").expect("one"),
            "a.b.c"
        );
        assert_eq!(
            logout_token_of(
                Some("application/x-www-form-urlencoded; charset=UTF-8"),
                b"x=1&logout_token=a%2Eb.c"
            )
            .expect("decoded"),
            "a.b.c"
        );
        for (ct, body, want) in [
            (Some("application/json"), &b"{}"[..], "content_type"),
            (None, &b"logout_token=x"[..], "content_type"),
            (FORM, &b""[..], "token_missing"),
            (FORM, &b"logout_token="[..], "token_missing"),
            (FORM, &b"logout_token=a&logout_token=b"[..], "form"),
        ] {
            let err = logout_token_of(ct, body).expect_err(want);
            assert_eq!(err.code(), ErrorCode::LogoutTokenInvalid);
            assert_eq!(detail(&err), want);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limiter_counts_per_peer_and_window() {
        let limiter = LoginLimiter::default();
        let t0 = Instant::now();
        let a: IpAddr = "192.0.2.1".parse().expect("ip");
        let b: IpAddr = "192.0.2.2".parse().expect("ip");
        for _ in 0..LOGIN_LIMIT {
            assert!(limiter.check(a, t0).is_ok());
        }
        let retry = limiter.check(a, t0).expect_err("over");
        assert!((1..=60).contains(&retry));
        assert!(limiter.check(b, t0).is_ok(), "another peer");
        // One /64: the same IPv6 subscriber.
        let v6a: IpAddr = "2001:db8::1".parse().expect("ip");
        let v6b: IpAddr = "2001:db8::2".parse().expect("ip");
        for _ in 0..LOGIN_LIMIT {
            assert!(limiter.check(v6a, t0).is_ok());
        }
        assert!(limiter.check(v6b, t0).is_err());
        assert!(limiter.check(a, t0 + LOGIN_WINDOW).is_ok(), "new window");
    }

    /// A full table evicts the peer whose window started first instead of
    /// refusing every new peer.
    #[test]
    fn a_full_limiter_evicts_the_oldest_peer_not_everyone() {
        let limiter = LoginLimiter::default();
        let t0 = Instant::now();
        let first: IpAddr = "198.51.100.1".parse().expect("ip");
        assert!(limiter.check(first, t0).is_ok());
        for i in 1..MAX_PEERS {
            let n = u32::try_from(i).expect("fits");
            let peer = IpAddr::from(std::net::Ipv4Addr::from(0x0a00_0000_u32 + n));
            assert!(limiter.check(peer, t0 + Duration::from_millis(1)).is_ok());
        }
        let newcomer: IpAddr = "203.0.113.9".parse().expect("ip");
        assert!(
            limiter
                .check(newcomer, t0 + Duration::from_millis(2))
                .is_ok(),
            "a new peer is admitted"
        );
        let peers = limiter.peers.lock().expect("table");
        assert_eq!(peers.len(), MAX_PEERS);
        assert!(!peers.contains_key(&first), "the oldest was evicted");
    }

    /// A poisoned table refuses (fail closed) and logs FG-SYS-0001 once.
    #[test]
    fn a_poisoned_limiter_refuses_and_logs_once() {
        let limiter = std::sync::Arc::new(LoginLimiter::default());
        let held = std::sync::Arc::clone(&limiter);
        let _ = std::thread::spawn(move || {
            let _guard = held.peers.lock().expect("table");
            panic!("poison the table (test)");
        })
        .join();
        let peer: IpAddr = "192.0.2.7".parse().expect("ip");
        assert!(limiter.check(peer, Instant::now()).is_err());
        assert!(limiter.poison_logged.load(Ordering::Relaxed));
        assert!(limiter.check(peer, Instant::now()).is_err(), "still closed");
    }
}
