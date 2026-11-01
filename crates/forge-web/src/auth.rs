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
use forge_core::authenticator::AuthFuture;
use forge_core::{Authenticator, RequestId};
use forge_errors::{ErrorCode, ForgeError, Lang};
use tokio::sync::Semaphore;

use crate::context;
use crate::error::ApiError;

/// The authenticator, carried to the per-route middleware as a request
/// extension (the registry is generic over its state; `app::build` inserts
/// it for every request).
#[derive(Clone)]
pub(crate) struct AuthenticatorExt(pub(crate) Arc<dyn Authenticator>);

/// Bounds the number of authentications that run at once (each one is a
/// write-pool lookup, ADR-0019 §3): a flood of well-formed random tokens
/// must not take every write connection. Waiting for a permit is bounded by
/// the request timeout (`FG-WEB-0002`); no new code. Generic over any
/// [`Authenticator`], so forge-web stays independent of forge-db;
/// `forge serve` sets the permits from the write pool size.
pub struct BoundedAuthenticator {
    inner: Arc<dyn Authenticator>,
    permits: Arc<Semaphore>,
    size: usize,
}

impl BoundedAuthenticator {
    /// `permits` = 0 is raised to 1 (zero would refuse every token).
    pub fn new(inner: Arc<dyn Authenticator>, permits: usize) -> Self {
        let size = permits.max(1);
        Self {
            inner,
            permits: Arc::new(Semaphore::new(size)),
            size,
        }
    }

    /// The number of authentications that may run at once.
    pub fn permits(&self) -> usize {
        self.size
    }
}

impl Authenticator for BoundedAuthenticator {
    fn authenticate<'a>(&'a self, request_id: &'a RequestId, presented: &'a str) -> AuthFuture<'a> {
        Box::pin(async move {
            // The semaphore is never closed (this value owns it); a closed
            // one is a bug, reported, not a pass.
            let _permit = self.permits.acquire().await.map_err(|_| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "authenticator_permits_closed")
            })?;
            self.inner.authenticate(request_id, presented).await
        })
    }
}

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
    // The scheme ends at the first ASCII space or tab.
    let (scheme, rest) = text.split_once([' ', '\t']).unwrap_or((text, ""));
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
    match authenticate_request(&mut req).await {
        Ok(()) => next.run(req).await,
        Err(res) => *res,
    }
}

/// The id of the personal access token a request authenticated with (a
/// request extension next to the `Principal`; never the secret).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokenCredential(pub(crate) i64);

/// What the authenticator returned about the caller's account, next to the
/// `Principal` (`GET /api/v1/user` shows it; never an authorization input:
/// `authorize()` reads the `Principal` only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerProfile {
    pub username: String,
    pub display_name: String,
    /// The account's administrator flag (rights also need a session or the
    /// token's `admin` scope: `Principal::admin_rights`).
    pub is_admin: bool,
    /// The token was issued for an AI agent (ADR-0017 §4.0).
    pub is_agent: bool,
    /// The token's scopes (`None` for a session).
    pub token_scopes: Option<Vec<forge_core::Scope>>,
}

/// Turn the request's credentials into a `Principal` extension (none
/// without an `Authorization` header); a rejection is the finished
/// response. The fixed routes' middleware and the dispatcher's step 4 (a
/// non-public word) share it.
pub(crate) async fn authenticate_request(req: &mut Request) -> Result<(), Box<Response>> {
    let lang = context::language(req.headers().get(ACCEPT_LANGUAGE));
    let Some(request_id) = req.extensions().get::<RequestId>().cloned() else {
        // Only reachable if the request-id middleware stopped running
        // outside the routes: fail closed and loudly, never authenticate
        // under an id nobody can find. The response still needs an id.
        return Err(Box::new(
            ApiError::new(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "request_id_missing"),
                RequestId::generate(),
                lang,
            )
            .into_response(),
        ));
    };
    let presented = match presented(req.headers()) {
        Ok(None) => return Ok(()),
        Ok(Some(p)) => p,
        Err(reason) => {
            return Err(Box::new(token_rejected(
                ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", reason),
                None,
                request_id,
                lang,
            )))
        }
    };
    if req.extensions().get::<Principal>().is_some() {
        return Err(Box::new(token_rejected(
            ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", "ambiguous"),
            Some(presented.scheme),
            request_id,
            lang,
        )));
    }
    let Some(AuthenticatorExt(authenticator)) = req.extensions().get::<AuthenticatorExt>().cloned()
    else {
        // Only reachable if `app::build` stopped inserting it: fail closed,
        // loudly.
        return Err(Box::new(
            ApiError::new(
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "authenticator_missing"),
                request_id,
                lang,
            )
            .into_response(),
        ));
    };
    match authenticator
        .authenticate(&request_id, &presented.token)
        .await
    {
        Ok(identity) => {
            req.extensions_mut().insert(CallerProfile {
                username: identity.username,
                display_name: identity.display_name,
                is_admin: identity.is_admin,
                is_agent: identity.is_agent,
                token_scopes: Some(identity.scopes.clone()), // clone: the principal holds them too
            });
            req.extensions_mut().insert(Principal::authenticated(
                identity.user_id,
                identity.is_admin,
                // `authenticate` refuses a disabled owner (FG-AUTH-0004).
                true,
                Credential::Token(identity.scopes),
            ));
            // Which token: the push record names it (`Pusher::credential_id`).
            req.extensions_mut()
                .insert(TokenCredential(identity.token_id));
            // The credential has done its job: no handler can echo or log it.
            req.headers_mut().remove(AUTHORIZATION);
            Ok(())
        }
        Err(err)
            if matches!(
                err.code(),
                ErrorCode::Unauthenticated | ErrorCode::AccountDisabled
            ) =>
        {
            Err(Box::new(token_rejected(
                err,
                Some(presented.scheme),
                request_id,
                lang,
            )))
        }
        // FG-DB-0001 (503), FG-DB-0019 and anything else: not a verdict on
        // the credentials. Logged once by the renderer.
        Err(err) => Err(Box::new(
            ApiError::new(
                err.with_context("scheme", presented.scheme.as_str()),
                request_id,
                lang,
            )
            .into_response(),
        )),
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
    // One representation: the context keys the authenticator and this
    // module set (`reason`; `public_id`, `user` when known) become fields.
    // `user` is the internal numeric id, as FG-AUTH-0004's catalog
    // diagnosis and forge-db's token errors already record it.
    let field = |key: &str| {
        err.context()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    };
    tracing::info!(
        target: "forge_web::auth",
        error_code = err.code().id(),
        request_id = %request_id,
        reason = field("reason"),
        public_id = field("public_id"),
        user = field("user"),
        scheme = scheme.map(Scheme::as_str),
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

    #[tokio::test]
    async fn a_missing_request_id_is_an_internal_error_not_a_fresh_id() {
        use axum::body::Body;
        use forge_auth::Action;
        use tower::ServiceExt;

        use crate::app::AppState;
        use crate::registry::{Registry, Verb};

        async fn ok(
            _: AppState,
            _: crate::registry::Dispatched<forge_auth::InstanceCtx>,
        ) -> &'static str {
            "ok"
        }
        // The product stack minus the request-id middleware.
        let state = AppState::new(
            "test",
            Vec::new(),
            crate::tests::no_tokens(),
            crate::tests::no_lookups(),
        );
        let router = Registry::<AppState>::new()
            .route(Verb::Get, "/guarded", Action::UserCreate, "x", ok)
            .into_router()
            .expect("router")
            .layer(axum::Extension(AuthenticatorExt(Arc::clone(
                &state.authenticator,
            ))))
            .with_state(state);
        let req = axum::http::Request::get("/guarded")
            .header(AUTHORIZATION, "Bearer fgp_x")
            .body(Body::empty())
            .expect("request");
        let res = router.oneshot(req).await.expect("infallible");
        assert_eq!(res.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn a_tab_separates_the_scheme_too() {
        assert_eq!(
            token_of(&[b"Bearer\tfgp_x"]),
            Ok(Some((Scheme::Bearer, "fgp_x".into())))
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
