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
//! - **response**: a `Set-Cookie` of one of [`TRANSLATED`] gets its
//!   plain-HTTP name and loses the `Secure` attribute (`HttpOnly`,
//!   `SameSite=Lax`, `Path=/`, `Max-Age` stay);
//! - **request**: the plain-HTTP cookie is renamed back to its `__Host-`
//!   name, and any `__Host-` cookie of [`TRANSLATED`] the request carries
//!   is dropped first (only the translated one counts, never two sources).
//!
//! The plain-HTTP names carry the base URL's port (`gstn_<port>_session`,
//! `gstn_<port>_oidc`) because browsers do not isolate cookies by port:
//! another forge (or any app) on the same host under a different port does
//! not overwrite or read these by accident. A co-hosted app can still read
//! and plant them on purpose (ADR-0021 Amendment 2026-11-20: trial only;
//! the real fix is an own host name or TLS).
//!
//! Other cookies and every other header pass untouched. On a secure origin
//! the middleware is not installed at all (`app::build`).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use forge_core::session_token::SESSION_COOKIE;
use forge_oidc::STATE_COOKIE;

/// The `__Host-` cookies translated on a plain-HTTP server.
pub(crate) const TRANSLATED: [&str; 2] = [SESSION_COOKIE, STATE_COOKIE];

/// The plain-HTTP names of [`TRANSLATED`], in the same order, for one port.
#[derive(Debug, Clone)]
pub(crate) struct PlainNames(Arc<[String; 2]>);

impl PlainNames {
    /// `gstn_<port>_session`, `gstn_<port>_oidc` (the port of `web.base_url`,
    /// 80 when it names none).
    pub(crate) fn for_port(port: u16) -> Self {
        Self(Arc::new([
            format!("gstn_{port}_session"),
            format!("gstn_{port}_oidc"),
        ]))
    }

    /// The plain name of a `__Host-` name of [`TRANSLATED`].
    fn plain(&self, host_name: &str) -> Option<&str> {
        TRANSLATED
            .iter()
            .position(|n| *n == host_name)
            .map(|i| self.0[i].as_str())
    }

    /// The `__Host-` name of a plain name.
    fn host(&self, plain_name: &[u8]) -> Option<&'static str> {
        self.0
            .iter()
            .position(|n| n.as_bytes() == plain_name)
            .map(|i| TRANSLATED[i])
    }
}

/// Middleware (`app::build`, outermost, plain-HTTP servers only).
pub(crate) async fn plain_http_cookies(
    State(names): State<PlainNames>,
    mut req: Request,
    next: Next,
) -> Response {
    translate_request_cookies(&names, req.headers_mut());
    let mut res = next.run(req).await;
    translate_set_cookies(&names, res.headers_mut());
    res
}

/// Request side: `gstn_<port>_session=...` becomes `__Host-gstn_session=...`;
/// a presented `__Host-gstn_session` / `__Host-gstn_oidc` is dropped. The
/// `Cookie` headers are folded into one (as `auth::strip_session_cookie`).
pub(crate) fn translate_request_cookies(names: &PlainNames, headers: &mut HeaderMap) {
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
        match names.host(key) {
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

/// Response side: each `Set-Cookie` of a translated cookie gets its plain
/// name and loses the `Secure` attribute.
pub(crate) fn translate_set_cookies(names: &PlainNames, headers: &mut HeaderMap) {
    if !headers.contains_key(SET_COOKIE) {
        return;
    }
    let values: Vec<HeaderValue> = headers
        .get_all(SET_COOKIE)
        .iter()
        .map(|value| translate_set_cookie(names, value).unwrap_or_else(|| value.clone()))
        .collect();
    headers.remove(SET_COOKIE);
    for value in values {
        headers.append(SET_COOKIE, value);
    }
}

/// The plain-HTTP form of one `Set-Cookie`, or `None` when it is not a
/// translated cookie (it then passes unchanged).
fn translate_set_cookie(names: &PlainNames, value: &HeaderValue) -> Option<HeaderValue> {
    let text = value.to_str().ok()?;
    let mut parts = text.split(';').map(str::trim);
    let first = parts.next()?;
    let (name, cookie_value) = first.split_once('=')?;
    let plain = names.plain(name.trim())?;
    let mut out = format!("{plain}={}", cookie_value.trim());
    for attr in parts.filter(|a| !a.is_empty() && !a.eq_ignore_ascii_case("secure")) {
        out.push_str("; ");
        out.push_str(attr);
    }
    HeaderValue::from_str(&out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> PlainNames {
        PlainNames::for_port(3306)
    }

    fn cookie_header(headers: &HeaderMap) -> Option<&str> {
        headers.get(COOKIE).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn the_plain_cookies_are_renamed_and_host_ones_dropped() {
        let mut h = HeaderMap::new();
        h.append(
            COOKIE,
            HeaderValue::from_static("a=1; gstn_3306_session=gss_x_y; __Host-gstn_session=forged; gstn_8080_session=other"),
        );
        h.append(
            COOKIE,
            HeaderValue::from_static("gstn_3306_oidc=sealed; b=2"),
        );
        translate_request_cookies(&names(), &mut h);
        assert_eq!(
            cookie_header(&h),
            Some("a=1; __Host-gstn_session=gss_x_y; gstn_8080_session=other; __Host-gstn_oidc=sealed; b=2")
        );
        assert_eq!(h.get_all(COOKIE).iter().count(), 1);
    }

    #[test]
    fn other_cookies_pass_and_an_only_host_cookie_leaves_no_header() {
        let mut h = HeaderMap::new();
        h.insert(
            COOKIE,
            HeaderValue::from_static("theme=dark; gstn_session=1"),
        );
        translate_request_cookies(&names(), &mut h);
        assert_eq!(cookie_header(&h), Some("theme=dark; gstn_session=1"));

        let mut h = HeaderMap::new();
        h.insert(COOKIE, HeaderValue::from_static("__Host-gstn_oidc=x"));
        translate_request_cookies(&names(), &mut h);
        assert!(h.get(COOKIE).is_none());

        let mut h = HeaderMap::new();
        translate_request_cookies(&names(), &mut h);
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
        translate_set_cookies(&names(), &mut h);
        let got: Vec<&str> = h
            .get_all(SET_COOKIE)
            .iter()
            .map(|v| v.to_str().expect("ascii"))
            .collect();
        assert_eq!(
            got,
            [
                "gstn_3306_session=gss_a_b; Max-Age=43200; Path=/; HttpOnly; SameSite=Lax",
                "gstn_3306_session=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax",
                "gstn_3306_oidc=sealed; Max-Age=600; Path=/; HttpOnly; SameSite=Lax",
                "other=1; Secure",
            ]
        );
    }

    #[test]
    fn the_translated_names_are_the_host_cookies() {
        assert_eq!(TRANSLATED, ["__Host-gstn_session", "__Host-gstn_oidc"]);
        let n = PlainNames::for_port(80);
        assert_eq!(n.plain(SESSION_COOKIE), Some("gstn_80_session"));
        assert_eq!(n.plain(STATE_COOKIE), Some("gstn_80_oidc"));
        assert_eq!(n.host(b"gstn_80_oidc"), Some(STATE_COOKIE));
        assert_eq!(n.plain("other"), None);
    }
}
