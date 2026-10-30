//! Token authentication (step D3, ADR-0019 §7): turns the request's
//! credentials into a `forge_auth::Principal` before the guard
//! ([`crate::guard`]) runs. The registry puts it on every non-public route,
//! so public routes (liveness, readiness) never consult the token store.
//!
//! Accepted: `Authorization: Bearer <token>`, or `Authorization: Basic`
//! whose **password** is the token (git sends Basic; the username is ignored
//! and never logged). A user's password is never a credential (ADR-0010
//! hand-off 9): whatever the Basic password is goes to the authenticator,
//! which accepts personal access tokens only.
//!
//! Outcomes:
//! - no `Authorization` header → nothing inserted (the guard answers
//!   `FG-AUTH-0001` on a guarded route);
//! - two headers, or a header on top of a principal another mechanism
//!   already set → `FG-AUTH-0001 reason=ambiguous` (neither is tried);
//! - an unreadable header or another scheme → `FG-AUTH-0001`
//!   (`malformed_header` / `unsupported_scheme`);
//! - the authenticator's answer: a valid token inserts
//!   `Principal::authenticated(owner, is_admin, true, Credential::Token(scopes))`
//!   (an administrator's token without the `admin` scope has no
//!   administrator rights, `Principal::admin_rights`); `FG-AUTH-0001` /
//!   `FG-AUTH-0004` are rejections; `FG-DB-0001` (503) and every other code
//!   pass through unchanged — an outage is never a 401.
//!
//! Rejections are logged once, here, as `forge.auth.token_rejected` with
//! `request_id`, `error_code`, `reason`, `scheme` and (when known)
//! `public_id`. The header value, the token, its secret and the Basic
//! username never reach a log or an error context.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::header::{ACCEPT_LANGUAGE, AUTHORIZATION};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use forge_auth::{Credential, Principal};
use forge_core::{Authenticator, RequestId};
use forge_errors::{ErrorCode, ForgeError, Lang};

use crate::context;
use crate::error::ApiError;

/// The authenticator, carried to the per-route middleware as a request
/// extension (the registry is generic over its state; `app::build` inserts
/// it for every request).
#[derive(Clone)]
pub(crate) struct AuthenticatorExt(pub(crate) Arc<dyn Authenticator>);

/// How the token was presented (logged; never the value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Bearer,
    Basic,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Scheme::Bearer => "bearer",
            Scheme::Basic => "basic",
        }
    }
}

/// The token as the client sent it. No `Debug`: it is a secret.
struct Presented {
    scheme: Scheme,
    token: String,
}

/// Why the header itself was refused (`reason` in the log).
type Refusal = &'static str;

/// The single `Authorization` credential of the request, if any.
fn presented(headers: &HeaderMap) -> Result<Option<Presented>, Refusal> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err("ambiguous");
    }
    let text = value.to_str().map_err(|_| "malformed_header")?;
    let (scheme, rest) = text.split_once(' ').unwrap_or((text, ""));
    let rest = rest.trim_matches([' ', '\t']);
    let scheme = if scheme.eq_ignore_ascii_case("bearer") {
        Scheme::Bearer
    } else if scheme.eq_ignore_ascii_case("basic") {
        Scheme::Basic
    } else {
        // The "scheme" may be a bare secret: it is never logged.
        return Err("unsupported_scheme");
    };
    if rest.is_empty() {
        return Err("malformed_header");
    }
    let token = match scheme {
        Scheme::Bearer => rest.to_owned(),
        Scheme::Basic => basic_password(rest)?,
    };
    Ok(Some(Presented { scheme, token }))
}

/// The password of `Basic base64(username:password)`; the username is
/// dropped here.
fn basic_password(encoded: &str) -> Result<String, Refusal> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "malformed_header")?;
    let text = String::from_utf8(raw).map_err(|_| "malformed_header")?;
    let (_username, password) = text.split_once(':').ok_or("malformed_header")?;
    Ok(password.to_owned())
}

/// Per-route middleware (non-public routes, before the guard).
pub(crate) async fn authenticate(mut req: Request, next: Next) -> Response {
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let lang = context::language(req.headers().get(ACCEPT_LANGUAGE));
    let presented = match presented(req.headers()) {
        Ok(None) => return next.run(req).await,
        Ok(Some(p)) => p,
        Err(reason) => {
            return token_rejected(
                ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", reason),
                None,
                request_id,
                lang,
            )
        }
    };
    if req.extensions().get::<Principal>().is_some() {
        return token_rejected(
            ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", "ambiguous"),
            Some(presented.scheme),
            request_id,
            lang,
        );
    }
    let Some(AuthenticatorExt(authenticator)) = req.extensions().get::<AuthenticatorExt>().cloned()
    else {
        // Only reachable if `app::build` stopped inserting it: fail closed,
        // loudly.
        return ApiError::new(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "authenticator_missing"),
            request_id,
            lang,
        )
        .into_response();
    };
    match authenticator
        .authenticate(&request_id, &presented.token)
        .await
    {
        Ok(identity) => {
            req.extensions_mut().insert(Principal::authenticated(
                identity.user_id,
                identity.is_admin,
                // `authenticate` refuses a disabled owner (FG-AUTH-0004).
                true,
                Credential::Token(identity.scopes),
            ));
            // The credential has done its job: no handler can echo or log it.
            req.headers_mut().remove(AUTHORIZATION);
            next.run(req).await
        }
        Err(err)
            if matches!(
                err.code(),
                ErrorCode::Unauthenticated | ErrorCode::AccountDisabled
            ) =>
        {
            token_rejected(err, Some(presented.scheme), request_id, lang)
        }
        // FG-DB-0001 (503), FG-DB-0019 and anything else: not a verdict on
        // the credentials. Logged once by the renderer.
        Err(err) => ApiError::new(
            err.with_context("scheme", presented.scheme.as_str()),
            request_id,
            lang,
        )
        .into_response(),
    }
}

/// Log the rejection once as `forge.auth.token_rejected` and answer with
/// the standard error body (401 carries the challenge, `error.rs`).
fn token_rejected(
    err: ForgeError,
    scheme: Option<Scheme>,
    request_id: RequestId,
    lang: Lang,
) -> Response {
    let field = |key: &str| {
        err.context()
            .iter()
            .find(|(k, _)| *k == key)
            .map_or("", |(_, v)| v.as_str())
    };
    let context = err
        .context()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");
    tracing::info!(
        target: "forge_web::auth",
        error_code = err.code().id(),
        request_id = %request_id,
        reason = field("reason"),
        public_id = field("public_id"),
        scheme = scheme.map_or("", Scheme::as_str),
        context = %context,
        "forge.auth.token_rejected"
    );
    ApiError::new(err, request_id, lang)
        .already_logged()
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(values: &[&[u8]]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(AUTHORIZATION, HeaderValue::from_bytes(v).expect("header"));
        }
        h
    }

    fn token_of(values: &[&[u8]]) -> Result<Option<(Scheme, String)>, Refusal> {
        presented(&headers(values)).map(|p| p.map(|p| (p.scheme, p.token)))
    }

    #[test]
    fn bearer_and_basic_yield_the_token_and_drop_the_username() {
        assert_eq!(
            token_of(&[b"Bearer  fgp_x \t"]),
            Ok(Some((Scheme::Bearer, "fgp_x".into())))
        );
        // "user:fgp_a:b" — the password may itself contain ':'.
        let enc = base64::engine::general_purpose::STANDARD.encode("user:fgp_a:b");
        assert_eq!(
            token_of(&[format!("BASIC {enc}").as_bytes()]),
            Ok(Some((Scheme::Basic, "fgp_a:b".into())))
        );
    }

    #[test]
    fn unreadable_headers_and_other_schemes_are_refused() {
        assert_eq!(token_of(&[]), Ok(None));
        assert_eq!(token_of(&[b"Bearer a", b"Bearer b"]), Err("ambiguous"));
        assert_eq!(token_of(&[b"Bearer \xff"]), Err("malformed_header"));
        assert_eq!(token_of(&[b"Basic !!"]), Err("malformed_header"));
        assert_eq!(token_of(&[b"Basic"]), Err("malformed_header"));
        assert_eq!(token_of(&[b"Token abc"]), Err("unsupported_scheme"));
        assert_eq!(token_of(&[b"fgp_bare"]), Err("unsupported_scheme"));
        let no_colon = base64::engine::general_purpose::STANDARD.encode("fgp_x");
        assert_eq!(
            token_of(&[format!("Basic {no_colon}").as_bytes()]),
            Err("malformed_header")
        );
    }
}
