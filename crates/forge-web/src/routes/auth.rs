//! The browser sign-in (M2 MR4, ADR-0021 §3): `GET /-/auth/login` and
//! `GET /-/auth/callback`, both `WebLogin` (public: the browser has no
//! session yet). The decisions are forge-oidc's; this module is HTTP only —
//! the query, the cookies, the redirects, the rate limit of the start.
//!
//! - Login: at most [`LOGIN_LIMIT`] starts per peer (an IPv6 peer counts as
//!   its /64) per [`LOGIN_WINDOW`] (`FG-WEB-0010`, `Retry-After`); then a
//!   302 to the provider with the sealed state cookie `__Host-gstn_oidc`
//!   (HttpOnly, Secure, SameSite=Lax, `Path=/`, 10 minutes).
//! - Callback: every answer clears the state cookie (single use); success
//!   sets `__Host-gstn_session` (Max-Age = the absolute lifetime) and 302s
//!   to `next`. Failures carry `event=forge.auth.login_failed` and are
//!   logged once by the error renderer.
//! - Without `auth.oidc`, both answer `FG-AUTH-0018` (503, info).
//!
//! Every answer is `Cache-Control: no-store`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::header::{CACHE_CONTROL, LOCATION, RETRY_AFTER, SET_COOKIE, USER_AGENT};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use forge_auth::InstanceCtx;
use forge_core::session_token::SESSION_COOKIE;
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
/// and a still-full table refuses (fail closed, logged as the 429).
const MAX_PEERS: usize = 10_000;

const CLEAR_STATE_COOKIE: &str =
    "__Host-gstn_oidc=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Lax";

/// The fixed-window counter of login starts per peer.
#[derive(Debug, Default)]
pub struct LoginLimiter {
    peers: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl LoginLimiter {
    /// Count one start of `peer`; `Err(retry_after_secs)` over the limit.
    pub fn check(&self, peer: IpAddr, now: Instant) -> Result<(), u64> {
        let key = crate::server::peer_key(peer);
        let Ok(mut peers) = self.peers.lock() else {
            // A poisoned table: refuse rather than run unlimited.
            return Err(LOGIN_WINDOW.as_secs());
        };
        if peers.len() >= MAX_PEERS && !peers.contains_key(&key) {
            peers.retain(|_, (start, _)| now.duration_since(*start) < LOGIN_WINDOW);
            if peers.len() >= MAX_PEERS {
                return Err(LOGIN_WINDOW.as_secs());
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
        let err = ForgeError::new(ErrorCode::RateLimited)
            .with_context("route", "login")
            .with_context("limit", LOGIN_LIMIT)
            .with_context("window_s", LOGIN_WINDOW.as_secs());
        let mut res = fail(&request, err, false);
        res.headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(retry));
        return res;
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
}
