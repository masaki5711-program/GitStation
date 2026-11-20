//! The cookie translation of a plain-HTTP trial server
//! (`web.allow_insecure_http` with an `http` `web.base_url` off the
//! loopback; `forge_core::config::BaseUrl::is_plain_http`).
//!
//! forge issues its browser cookies as `__Host-gstn_session` and
//! `__Host-gstn_oidc` with `Secure` (ADR-0021 §5, §10). A browser on a
//! plain-`http` origin that is not the loopback is not in a secure context:
//! it drops such a `Set-Cookie`, and the sign-in can never complete. On such
//! a server (and only there) this outermost middleware translates at the
//! edge, so the rest of forge-web keeps one cookie vocabulary:
//!
//! - **response**: a `Set-Cookie` of one of [`TRANSLATED`] loses the
//!   `__Host-` prefix and the `Secure` attribute (`HttpOnly`,
//!   `SameSite=Lax`, `Path=/`, `Max-Age` stay);
//! - **request**: the unprefixed cookie is renamed back to its `__Host-`
//!   name, and any `__Host-` cookie of [`TRANSLATED`] the request carries
//!   is dropped first (only the translated one counts, never two sources).
//!
//! Other cookies and every other header pass untouched. On a secure origin
//! the middleware is not installed at all (`app::build`).

use axum::extract::Request;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use forge_core::session_token::SESSION_COOKIE;
use forge_oidc::STATE_COOKIE;

/// The `__Host-` cookies translated on a plain-HTTP server.
pub(crate) const TRANSLATED: [&str; 2] = [SESSION_COOKIE, STATE_COOKIE];

const HOST_PREFIX: &str = "__Host-";

/// Middleware (`app::build`, outermost, plain-HTTP servers only).
pub(crate) async fn plain_http_cookies(mut req: Request, next: Next) -> Response {
    translate_request_cookies(req.headers_mut());
    let mut res = next.run(req).await;
    translate_set_cookies(res.headers_mut());
    res
}

/// The unprefixed name of a translated cookie (`gstn_session`).
fn plain_name(host_name: &str) -> &str {
    host_name.strip_prefix(HOST_PREFIX).unwrap_or(host_name)
}

/// Request side: `gstn_session=…` → `__Host-gstn_session=…`; a presented
/// `__Host-gstn_session` / `__Host-gstn_oidc` is dropped. The `Cookie`
/// headers are folded into one (as `auth::strip_session_cookie` does).
pub(crate) fn translate_request_cookies(headers: &mut HeaderMap) {
    if !headers.contains_key(COOKIE) {
        return;
    }
    let mut kept: Vec<Vec<u8>> = Vec::new();
    for pair in headers
        .get_all(COOKIE)
        .iter()
        .flat_map(|h| h.as_bytes().split(|b| *b == b';'))
        .map(<[u8]>::trim_ascii)
        .filter(|pair| !pair.is_empty())
    {
        let at = pair.iter().position(|b| *b == b'=');
        let key = at.map_or(pair, |at| &pair[..at]).trim_ascii();
        let rest = at.map_or(&[][..], |at| &pair[at..]);
        if TRANSLATED.iter().any(|name| key == name.as_bytes()) {
            continue;
        }
        match TRANSLATED
            .iter()
            .find(|name| key == plain_name(name).as_bytes())
        {
            Some(name) => {
                let mut renamed = name.as_bytes().to_vec();
                renamed.extend_from_slice(rest);
                kept.push(renamed);
            }
            None => kept.push(pair.to_vec()),
        }
    }
    headers.remove(COOKIE);
    if kept.is_empty() {
        return;
    }
    if let Ok(value) = HeaderValue::from_bytes(&kept.join(&b"; "[..])) {
        headers.insert(COOKIE, value);
    }
}

/// Response side: each `Set-Cookie` of a translated cookie loses the
/// `__Host-` prefix and the `Secure` attribute.
pub(crate) fn translate_set_cookies(headers: &mut HeaderMap) {
    if !headers.contains_key(SET_COOKIE) {
        return;
    }
    let values: Vec<HeaderValue> = headers
        .get_all(SET_COOKIE)
        .iter()
        .map(|value| translate_set_cookie(value).unwrap_or_else(|| value.clone()))
        .collect();
    headers.remove(SET_COOKIE);
    for value in values {
        headers.append(SET_COOKIE, value);
    }
}

/// The plain-HTTP form of one `Set-Cookie`, or `None` when it is not a
/// translated cookie (it then passes unchanged).
fn translate_set_cookie(value: &HeaderValue) -> Option<HeaderValue> {
    let text = value.to_str().ok()?;
    let mut parts = text.split(';').map(str::trim);
    let first = parts.next()?;
    let (name, cookie_value) = first.split_once('=')?;
    let name = name.trim();
    if !TRANSLATED.contains(&name) {
        return None;
    }
    let mut out = format!("{}={}", plain_name(name), cookie_value.trim());
    for attr in parts.filter(|a| !a.is_empty() && !a.eq_ignore_ascii_case("secure")) {
        out.push_str("; ");
        out.push_str(attr);
    }
    HeaderValue::from_str(&out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie_header(headers: &HeaderMap) -> Option<&str> {
        headers.get(COOKIE).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn the_plain_cookies_are_renamed_and_host_ones_dropped() {
        let mut h = HeaderMap::new();
        h.append(
            COOKIE,
            HeaderValue::from_static("a=1; gstn_session=gss_x_y; __Host-gstn_session=forged"),
        );
        h.append(COOKIE, HeaderValue::from_static("gstn_oidc=sealed; b=2"));
        translate_request_cookies(&mut h);
        assert_eq!(
            cookie_header(&h),
            Some("a=1; __Host-gstn_session=gss_x_y; __Host-gstn_oidc=sealed; b=2")
        );
        assert_eq!(h.get_all(COOKIE).iter().count(), 1);
    }

    #[test]
    fn other_cookies_pass_and_an_only_host_cookie_leaves_no_header() {
        let mut h = HeaderMap::new();
        h.insert(
            COOKIE,
            HeaderValue::from_static("theme=dark; gstn_sessionx=1"),
        );
        translate_request_cookies(&mut h);
        assert_eq!(cookie_header(&h), Some("theme=dark; gstn_sessionx=1"));

        let mut h = HeaderMap::new();
        h.insert(COOKIE, HeaderValue::from_static("__Host-gstn_oidc=x"));
        translate_request_cookies(&mut h);
        assert!(h.get(COOKIE).is_none());

        let mut h = HeaderMap::new();
        translate_request_cookies(&mut h);
        assert!(h.get(COOKIE).is_none());
    }

    #[test]
    fn the_issued_and_clearing_cookies_lose_prefix_and_secure() {
        let mut h = HeaderMap::new();
        // The exact forms issued by routes::auth and auth.rs.
        h.append(
            SET_COOKIE,
            HeaderValue::from_static(
                "__Host-gstn_session=gss_a_b; Max-Age=43200; Path=/; Secure; HttpOnly; SameSite=Lax",
            ),
        );
        h.append(
            SET_COOKIE,
            HeaderValue::from_static(crate::auth::CLEAR_SESSION_COOKIE),
        );
        h.append(
            SET_COOKIE,
            HeaderValue::from_static(
                "__Host-gstn_oidc=sealed; Max-Age=600; Path=/; Secure; HttpOnly; SameSite=Lax",
            ),
        );
        h.append(SET_COOKIE, HeaderValue::from_static("other=1; Secure"));
        translate_set_cookies(&mut h);
        let got: Vec<&str> = h
            .get_all(SET_COOKIE)
            .iter()
            .map(|v| v.to_str().expect("ascii"))
            .collect();
        assert_eq!(
            got,
            [
                "gstn_session=gss_a_b; Max-Age=43200; Path=/; HttpOnly; SameSite=Lax",
                "gstn_session=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax",
                "gstn_oidc=sealed; Max-Age=600; Path=/; HttpOnly; SameSite=Lax",
                "other=1; Secure",
            ]
        );
    }

    #[test]
    fn the_translated_names_are_the_host_cookies() {
        assert_eq!(TRANSLATED, ["__Host-gstn_session", "__Host-gstn_oidc"]);
        assert_eq!(plain_name(SESSION_COOKIE), "gstn_session");
        assert_eq!(plain_name(STATE_COOKIE), "gstn_oidc");
    }
}
