//! The one place the CSP and the other security headers are decided
//! (ADR-0022 §3, the table). Every response passes [`security_headers`],
//! the outermost middleware; a response's class comes from a marker its
//! handler sets ([`ResponseClass::Shell`] / [`ResponseClass::Asset`]) or,
//! without one, from the raw request path:
//!
//! | class | `Content-Security-Policy` | other |
//! |---|---|---|
//! | shell | `'self'` only (+ the IdP origin in `form-action`, MR4) | — |
//! | asset | as the shell | `Cross-Origin-Resource-Policy: same-origin` |
//! | API `/api/…` | `default-src 'none'; frame-ancestors 'none'` | `Cache-Control: private, no-store`, `Vary: Authorization, Cookie` unless the route set them |
//! | raw / archive | `sandbox; default-src 'none'` | `Cross-Origin-Resource-Policy: same-origin` |
//! | login `/-/auth/…` | as the API | `Cache-Control: no-store` |
//! | other (health, redirects, every error not from a marked handler) | as the API | `Cache-Control: no-store` unless the route set one |
//! | Smart HTTP | none (git clients read no CSP; unchanged) | none |
//!
//! Every class but Smart HTTP also gets `X-Content-Type-Options: nosniff`,
//! `Referrer-Policy: same-origin`, `Cross-Origin-Opener-Policy:
//! same-origin`, the `Permissions-Policy` below and, when configured,
//! `Strict-Transport-Security`.
//!
//! An error answered on an asset or shell path is not marked, so it gets
//! the strict "other" policy — never an asset's year-long cache (the cache
//! header is the asset handler's own).

use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::header::{
    HeaderName, HeaderValue, CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY,
    STRICT_TRANSPORT_SECURITY, VARY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use forge_core::web_path::{parse_web_path, ParsedPath, RouteWord};
use forge_errors::{ErrorCode, ForgeError};

/// What a response is, for the header table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseClass {
    /// The SPA shell (`index.html`); set by `routes::shell`.
    Shell,
    /// A file under `/-/assets/`; set by `routes::assets` on a found file.
    Asset,
    Api,
    /// `/{namespace}/-/raw/…`, `/-/archive/…` (MR7).
    Raw,
    /// `/-/auth/…` (MR4).
    Auth,
    /// `{namespace}.git/…`.
    Git,
    Other,
}

const CSP_SHELL_BASE: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; \
base-uri 'none'; frame-ancestors 'none'; form-action 'self'";
const CSP_NONE: &str = "default-src 'none'; frame-ancestors 'none'";
const CSP_RAW: &str = "sandbox; default-src 'none'";
const PERMISSIONS: &str = "camera=(), microphone=(), geolocation=(), usb=(), payment=()";
const CORP: HeaderName = HeaderName::from_static("cross-origin-resource-policy");
const COOP: HeaderName = HeaderName::from_static("cross-origin-opener-policy");
const PERMISSIONS_POLICY: HeaderName = HeaderName::from_static("permissions-policy");

/// The prepared header values (built once, at router assembly).
#[derive(Debug, Clone)]
pub struct SecurityHeaders {
    shell_csp: HeaderValue,
    hsts: Option<HeaderValue>,
}

impl SecurityHeaders {
    /// `hsts`: `web.hsts` (`max-age` only, never `includeSubDomains`;
    /// `None` = not sent, TLS may end elsewhere). `form_action`: the IdP
    /// origin the logout form redirects to (ADR-0021 §8; `auth.oidc`,
    /// MR4) — a configuration value, so the shell's bytes stay the same.
    pub fn new(hsts: Option<Duration>, form_action: Option<&str>) -> Result<Self, ForgeError> {
        let shell = match form_action {
            None => CSP_SHELL_BASE.to_owned(),
            Some(origin) => {
                if !is_origin(origin) {
                    return Err(ForgeError::new(ErrorCode::ConfigInvalid)
                        .with_context("reason", "form_action_origin_invalid")
                        .with_context("origin", origin));
                }
                format!("{CSP_SHELL_BASE} {origin}")
            }
        };
        let shell_csp = HeaderValue::from_str(&shell).map_err(|_| {
            ForgeError::new(ErrorCode::ConfigInvalid)
                .with_context("reason", "form_action_origin_invalid")
        })?;
        let hsts = hsts
            .filter(|d| d.as_secs() > 0)
            .map(|d| HeaderValue::from_str(&format!("max-age={}", d.as_secs())))
            .transpose()
            .map_err(|_| {
                ForgeError::new(ErrorCode::Internal).with_context("reason", "hsts_header_invalid")
            })?;
        Ok(Self { shell_csp, hsts })
    }
}

impl Default for SecurityHeaders {
    fn default() -> Self {
        Self {
            shell_csp: HeaderValue::from_static(CSP_SHELL_BASE),
            hsts: None,
        }
    }
}

/// `scheme://host[:port]`, nothing after: no path, query, space, `;` or
/// quote (it ends up inside a CSP directive).
fn is_origin(origin: &str) -> bool {
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"));
    rest.is_some_and(|host| {
        !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
    })
}

/// The class of a response without a marker, by the raw path.
pub(crate) fn class_of_path(raw_path: &str, query: Option<&str>) -> ResponseClass {
    if raw_path == "/api" || raw_path.starts_with("/api/") {
        return ResponseClass::Api;
    }
    if raw_path == "/-/auth" || raw_path.starts_with("/-/auth/") {
        return ResponseClass::Auth;
    }
    match parse_web_path(raw_path, query) {
        Ok(ParsedPath::Git { .. }) => ResponseClass::Git,
        Ok(ParsedPath::Scoped {
            word: RouteWord::Raw | RouteWord::Archive,
            ..
        }) => ResponseClass::Raw,
        Ok(
            ParsedPath::Scoped { .. } | ParsedPath::NamespaceHome(_) | ParsedPath::Instance { .. },
        )
        | Err(_) => ResponseClass::Other,
    }
}

/// The middleware (outermost layer).
pub(crate) async fn security_headers(
    State(config): State<SecurityHeaders>,
    req: Request,
    next: Next,
) -> Response {
    let by_path = class_of_path(req.uri().path(), req.uri().query());
    let mut res = next.run(req).await;
    let class = res
        .extensions()
        .get::<ResponseClass>()
        .copied()
        .unwrap_or(by_path);
    apply(&config, class, res.headers_mut());
    res
}

fn apply(config: &SecurityHeaders, class: ResponseClass, headers: &mut HeaderMap) {
    let csp = match class {
        ResponseClass::Git => return,
        ResponseClass::Shell | ResponseClass::Asset => config.shell_csp.clone(),
        ResponseClass::Raw => HeaderValue::from_static(CSP_RAW),
        ResponseClass::Api | ResponseClass::Auth | ResponseClass::Other => {
            HeaderValue::from_static(CSP_NONE)
        }
    };
    headers.insert(CONTENT_SECURITY_POLICY, csp);
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    headers.insert(COOP, HeaderValue::from_static("same-origin"));
    headers.insert(PERMISSIONS_POLICY, HeaderValue::from_static(PERMISSIONS));
    if let Some(hsts) = &config.hsts {
        headers.insert(STRICT_TRANSPORT_SECURITY, hsts.clone());
    }
    match class {
        ResponseClass::Asset | ResponseClass::Raw => {
            headers.insert(CORP, HeaderValue::from_static("same-origin"));
        }
        ResponseClass::Api => {
            if !headers.contains_key(CACHE_CONTROL) {
                headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
            }
            if !headers.contains_key(VARY) {
                headers.insert(VARY, HeaderValue::from_static("Authorization, Cookie"));
            }
        }
        ResponseClass::Auth => {
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
        ResponseClass::Other => {
            // Errors, health answers, redirects: never kept by a cache
            // (a route's own Cache-Control stays).
            if !headers.contains_key(CACHE_CONTROL) {
                headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            }
        }
        ResponseClass::Shell | ResponseClass::Git => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_fall_into_their_classes() {
        let q = Some("service=git-upload-pack");
        for (path, query, class) in [
            ("/api/v1/projects", None, ResponseClass::Api),
            ("/api", None, ResponseClass::Api),
            ("/-/auth/login", None, ResponseClass::Auth),
            ("/acme/x.git/info/refs", q, ResponseClass::Git),
            ("/acme/x.git/git-upload-pack", None, ResponseClass::Git),
            ("/acme/x/-/raw/main/a.txt", None, ResponseClass::Raw),
            ("/acme/x/-/archive/main.tar.gz", None, ResponseClass::Raw),
            // A shell path or an asset without the handler's marker
            // (an error) is "other": the strict policy, no asset cache.
            ("/acme/x", None, ResponseClass::Other),
            ("/-/assets/missing.js", None, ResponseClass::Other),
            ("/healthz", None, ResponseClass::Other),
            ("/../x", None, ResponseClass::Other),
        ] {
            assert_eq!(class_of_path(path, query), class, "{path}");
        }
    }

    #[test]
    fn the_form_action_origin_is_validated_and_appended() {
        let h =
            SecurityHeaders::new(None, Some("https://sso.example.internal:8443")).expect("origin");
        assert!(h
            .shell_csp
            .to_str()
            .expect("ascii")
            .ends_with("form-action 'self' https://sso.example.internal:8443"));
        for bad in [
            "sso.example",
            "https://",
            "https://a/b",
            "https://a; script-src *",
            "javascript:x",
            "https://a'",
        ] {
            let err = SecurityHeaders::new(None, Some(bad)).expect_err(bad);
            assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{bad}");
        }
    }

    #[test]
    fn hsts_is_max_age_only_and_off_by_default() {
        assert!(SecurityHeaders::default().hsts.is_none());
        assert!(SecurityHeaders::new(Some(Duration::ZERO), None)
            .expect("zero")
            .hsts
            .is_none());
        let h = SecurityHeaders::new(Some(Duration::from_secs(31_536_000)), None).expect("hsts");
        assert_eq!(h.hsts.expect("set"), "max-age=31536000");
    }
}
