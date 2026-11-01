//! Authentication (step D3, ADR-0019 §7; web sessions M2 MR3, ADR-0021
//! §5, §6, §10): turns the request's credentials into a
//! `forge_auth::Principal` before the guard ([`crate::guard`]) runs. The
//! registry puts it on every non-public route, so public routes (liveness,
//! readiness) never consult the token or session store.
//!
//! What counts as a credential depends on the route's [`Surface`]:
//! - **API** (the fixed `/api/v1` routes): `Authorization: Bearer <token>`,
//!   or the session cookie `__Host-gstn_session`. `Authorization: Basic` is
//!   refused without trying it (`FG-AUTH-0001 reason=basic_not_allowed`,
//!   ADR-0019 Amendment M2: a browser would remember a token typed into a
//!   Basic prompt and send it automatically, like a cookie).
//! - **git** (Smart HTTP): Bearer, or Basic whose **password** is the token
//!   (git sends Basic; the username is ignored and never logged). Cookies
//!   are not looked at.
//! - **content** (raw / archive): Bearer, Basic, or the session cookie.
//!
//! A user's password is never a credential (ADR-0010 hand-off 9): whatever
//! the Basic password is goes to the authenticator, which accepts personal
//! access tokens only.
//!
//! Outcomes:
//! - no credential → nothing inserted (the guard answers `FG-AUTH-0001` on
//!   a guarded route);
//! - two `Authorization` headers, two session cookies, a session cookie
//!   together with `Authorization` (API and content), or a credential on
//!   top of a principal another mechanism already set → `FG-AUTH-0001
//!   reason=ambiguous` (neither is tried). Only the cookie named
//!   `__Host-gstn_session` counts (ADR-0021 §10);
//! - an unreadable header or another scheme → `FG-AUTH-0001`
//!   (`malformed_header` / `unsupported_scheme`);
//! - the token authenticator's answer: a valid token inserts
//!   `Principal::authenticated(owner, is_admin, true, Credential::Token(scopes))`
//!   (an administrator's token without the `admin` scope has no
//!   administrator rights, `Principal::admin_rights`);
//! - the session authenticator's answer: a valid session inserts
//!   `Principal::authenticated(user, is_admin, true, Credential::Session)`
//!   and the [`SessionCredential`]; a state-changing request must then
//!   pass the CSRF token check ([`crate::csrf::session_check`],
//!   `FG-AUTH-0016`);
//! - `FG-AUTH-0001` / `FG-AUTH-0004` are rejections (a session's also
//!   clear the cookie, `Max-Age=0`); `FG-DB-0001` (503) and every other
//!   code pass through unchanged — an outage is never a 401 and never
//!   clears the cookie.
//!
//! Rejections are logged once, here, as `forge.auth.token_rejected` (or
//! `forge.auth.session_rejected`) with `request_id`, `error_code`, `reason`,
//! `scheme` and (when known) `public_id`. The header and cookie values, the
//! token, its secret and the Basic username never reach a log or an error
//! context.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::header::{ACCEPT_LANGUAGE, AUTHORIZATION, COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use forge_auth::{Credential, Principal};
use forge_core::authenticator::{AuthFuture, SessionAuthFuture};
use forge_core::session_token::{self, SESSION_COOKIE};
use forge_core::{Authenticator, RequestId, SessionAuthenticator};
use forge_errors::{ErrorCode, ForgeError, Lang};
use tokio::sync::Semaphore;

use crate::context;
use crate::csrf::{self, ExpectedOrigin};
use crate::error::ApiError;

/// The authenticator, carried to the per-route middleware as a request
/// extension (the registry is generic over its state; `app::build` inserts
/// it for every request).
#[derive(Clone)]
pub(crate) struct AuthenticatorExt(pub(crate) Arc<dyn Authenticator>);

/// The session authenticator (`None`: the server was built without one —
/// `forge serve` always sets it; a session cookie then answers
/// `FG-SYS-0001 reason=sessions_unconfigured`) and the origin the CSRF
/// check compares with, as a request extension (`app::build`).
#[derive(Clone)]
pub(crate) struct SessionsExt {
    pub(crate) sessions: Option<Arc<dyn SessionAuthenticator>>,
    pub(crate) origin: ExpectedOrigin,
}

/// Where a request authenticates (ADR-0021 §6, §10): which credentials
/// count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    /// The fixed routes (`/api/v1`): Bearer or the session cookie; Basic
    /// is refused.
    Api,
    /// git Smart HTTP (and any other dispatcher word but raw / archive):
    /// Bearer or Basic; cookies are not looked at.
    Git,
    /// raw / archive: Bearer, Basic or the session cookie.
    Content,
}

impl Surface {
    fn reads_cookies(self) -> bool {
        matches!(self, Surface::Api | Surface::Content)
    }
}

/// Bounds the number of authentications that run at once (each one is a
/// write-pool lookup, ADR-0019 §3): a flood of well-formed random tokens
/// must not take every write connection. Waiting for a permit is bounded by
/// the request timeout (`FG-WEB-0002`); no new code. Generic over any
/// [`Authenticator`], so forge-web stays independent of forge-db;
/// `forge serve` sets the permits from the write pool size. Session
/// lookups share the same permits ([`BoundedAuthenticator::sessions`],
/// ADR-0021 §5).
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

    /// `inner` behind the same permits as the tokens: tokens and sessions
    /// together never take more than [`Self::permits`] connections.
    pub fn sessions(&self, inner: Arc<dyn SessionAuthenticator>) -> BoundedSessions {
        BoundedSessions {
            inner,
            permits: Arc::clone(&self.permits),
        }
    }
}

/// The permit of one authentication; a closed semaphore is a bug,
/// reported, not a pass.
async fn permit(permits: &Semaphore) -> Result<tokio::sync::SemaphorePermit<'_>, ForgeError> {
    permits.acquire().await.map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "authenticator_permits_closed")
    })
}

impl Authenticator for BoundedAuthenticator {
    fn authenticate<'a>(&'a self, request_id: &'a RequestId, presented: &'a str) -> AuthFuture<'a> {
        Box::pin(async move {
            let _permit = permit(&self.permits).await?;
            self.inner.authenticate(request_id, presented).await
        })
    }
}

/// A session authenticator behind a [`BoundedAuthenticator`]'s permits.
pub struct BoundedSessions {
    inner: Arc<dyn SessionAuthenticator>,
    permits: Arc<Semaphore>,
}

impl SessionAuthenticator for BoundedSessions {
    fn authenticate_session<'a>(
        &'a self,
        request_id: &'a RequestId,
        presented: &'a str,
    ) -> SessionAuthFuture<'a> {
        Box::pin(async move {
            let _permit = permit(&self.permits).await?;
            self.inner.authenticate_session(request_id, presented).await
        })
    }
}

/// How the request authenticated (logged; never the value). The CSRF token
/// check exempts [`Scheme::Bearer`] only (ADR-0021 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scheme {
    Bearer,
    Basic,
    Session,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Scheme::Bearer => "bearer",
            Scheme::Basic => "basic",
            Scheme::Session => "session",
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
        // `presented` reads the Authorization header only.
        Scheme::Session => return Err("unsupported_scheme"),
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

/// The values of every `__Host-gstn_session` cookie of the request, in
/// order (other cookies are not credentials, ADR-0021 §10). A value that is
/// not UTF-8 is kept as an empty string (the authenticator refuses it as
/// `session_malformed`).
pub(crate) fn session_cookies(headers: &HeaderMap) -> Vec<String> {
    let name = SESSION_COOKIE.as_bytes();
    let mut found = Vec::new();
    for header in headers.get_all(COOKIE) {
        for pair in header.as_bytes().split(|b| *b == b';') {
            let pair = pair.trim_ascii();
            let (key, value) = match pair.iter().position(|b| *b == b'=') {
                Some(at) => (&pair[..at], &pair[at + 1..]),
                None => (pair, &[][..]),
            };
            if key.trim_ascii() == name {
                found.push(
                    std::str::from_utf8(value.trim_ascii())
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
        }
    }
    found
}

/// The request's `Cookie` headers without the session cookie (it has done
/// its job: no handler can echo or log it); other cookies stay.
fn strip_session_cookie(headers: &mut HeaderMap) {
    let name = SESSION_COOKIE.as_bytes();
    let kept: Vec<Vec<u8>> = headers
        .get_all(COOKIE)
        .iter()
        .flat_map(|h| h.as_bytes().split(|b| *b == b';'))
        .map(<[u8]>::trim_ascii)
        .filter(|pair| {
            let key = pair.split(|b| *b == b'=').next().unwrap_or_default();
            !pair.is_empty() && key.trim_ascii() != name
        })
        .map(<[u8]>::to_vec)
        .collect();
    headers.remove(COOKIE);
    if kept.is_empty() {
        return;
    }
    if let Ok(value) = HeaderValue::from_bytes(&kept.join(&b"; "[..])) {
        headers.insert(COOKIE, value);
    }
}

/// The `Set-Cookie` that removes the session cookie (its attributes match
/// the issued one, or the browser keeps it).
pub(crate) const CLEAR_SESSION_COOKIE: &str =
    "__Host-gstn_session=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Lax";

/// Per-route middleware (fixed routes: the API surface, before the guard).
pub(crate) async fn authenticate(mut req: Request, next: Next) -> Response {
    match authenticate_request(&mut req, Surface::Api).await {
        Ok(()) => next.run(req).await,
        Err(res) => *res,
    }
}

/// The id of the personal access token a request authenticated with (a
/// request extension next to the `Principal`; never the secret).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokenCredential(pub(crate) i64);

/// The web session a request authenticated with (a request extension next
/// to the `Principal`): ids, its expiries and its CSRF token — never the
/// cookie or its secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCredential {
    pub session_id: i64,
    pub public_id: String,
    /// `hex(HMAC-SHA256(secret, "gitstation-csrf-v1"))` (ADR-0021 §6).
    pub csrf_token: String,
    /// RFC 3339, UTC.
    pub idle_expires_at: String,
    pub absolute_expires_at: String,
}

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
/// without a credential); a rejection is the finished response. The fixed
/// routes' middleware ([`Surface::Api`]) and the dispatcher's step 4 (a
/// non-public word: [`Surface::Git`] or [`Surface::Content`]) share it.
pub(crate) async fn authenticate_request(
    req: &mut Request,
    surface: Surface,
) -> Result<(), Box<Response>> {
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
    let reject = |reason: Refusal, scheme: Option<Scheme>| {
        Err(Box::new(token_rejected(
            ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", reason),
            scheme,
            request_id.clone(), // clone: the error owns its id
            lang,
        )))
    };
    let cookies = if surface.reads_cookies() {
        session_cookies(req.headers())
    } else {
        Vec::new()
    };
    let presented = match presented(req.headers()) {
        Ok(p) => p,
        Err(reason) => return reject(reason, None),
    };
    let presented = match (presented, cookies.as_slice()) {
        (None, []) => return Ok(()),
        // A cookie and a header, or two cookies: neither is tried.
        (Some(p), [_, ..]) => return reject("ambiguous", Some(p.scheme)),
        (None, [_, _, ..]) => return reject("ambiguous", Some(Scheme::Session)),
        (None, [cookie]) => {
            let cookie = cookie.clone(); // clone: the request is borrowed mutably below
            return authenticate_session(req, &cookie, request_id, lang).await;
        }
        (Some(p), []) => p,
    };
    if presented.scheme == Scheme::Basic && surface == Surface::Api {
        // ADR-0019 Amendment M2: the API takes Bearer only; the Basic
        // password is not tried.
        return reject("basic_not_allowed", Some(Scheme::Basic));
    }
    if presented.scheme == Scheme::Basic
        && surface == Surface::Content
        && csrf::changes_state(req.method())
    {
        // ADR-0021 §6: only Bearer is exempt from the CSRF token, and Basic
        // has none (raw / archive serve GET and HEAD only, so this never
        // refuses a real client). git Smart HTTP is the documented
        // exception: its POSTs get the browser check only.
        let origin = req
            .extensions()
            .get::<SessionsExt>()
            .map(|ext| ext.origin.clone())
            .unwrap_or(ExpectedOrigin(None));
        return Err(Box::new(csrf::rejected(
            csrf::rejection("token_missing", &origin),
            req.method(),
            req.uri().path(),
            None,
            request_id,
            lang,
        )));
    }
    if req.extensions().get::<Principal>().is_some() {
        return reject("ambiguous", Some(presented.scheme));
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
            req.extensions_mut().insert(
                Principal::authenticated(
                    identity.user_id,
                    identity.is_admin,
                    // `authenticate` refuses a disabled owner (FG-AUTH-0004).
                    true,
                    Credential::Token(identity.scopes),
                )
                // ADR-0017 §4.0: an authorization input since M1 phase 8b MR7a.
                .as_agent(identity.is_agent),
            );
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

/// The session half of [`authenticate_request`]: exactly one session
/// cookie and no `Authorization`.
async fn authenticate_session(
    req: &mut Request,
    cookie: &str,
    request_id: RequestId,
    lang: Lang,
) -> Result<(), Box<Response>> {
    let internal = |reason: &'static str, request_id: RequestId| {
        Err(Box::new(
            ApiError::new(
                ForgeError::new(ErrorCode::Internal).with_context("reason", reason),
                request_id,
                lang,
            )
            .into_response(),
        ))
    };
    if req.extensions().get::<Principal>().is_some() {
        return Err(Box::new(session_rejected(
            ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", "ambiguous"),
            request_id,
            lang,
        )));
    }
    let Some(ext) = req.extensions().get::<SessionsExt>().cloned() else {
        return internal("sessions_ext_missing", request_id);
    };
    let Some(sessions) = ext.sessions else {
        return internal("sessions_unconfigured", request_id);
    };
    let identity = match sessions.authenticate_session(&request_id, cookie).await {
        Ok(identity) => identity,
        Err(err)
            if matches!(
                err.code(),
                ErrorCode::Unauthenticated | ErrorCode::AccountDisabled
            ) =>
        {
            return Err(Box::new(session_rejected(err, request_id, lang)));
        }
        // An outage (FG-DB-0001) or an unreadable row: the cookie stays.
        Err(err) => {
            return Err(Box::new(
                ApiError::new(
                    err.with_context("scheme", Scheme::Session.as_str()),
                    request_id,
                    lang,
                )
                .into_response(),
            ))
        }
    };
    // The authenticator accepted this value, so it parses; the CSRF token
    // comes from its secret (never stored, ADR-0021 §6).
    let Ok(plain) = session_token::parse(cookie) else {
        return internal("session_cookie_reparse_failed", request_id);
    };
    let csrf_token = match plain.csrf_token() {
        Ok(token) => token,
        Err(_) => return internal("csrf_mac_failed", request_id),
    };
    if let Err(refused) =
        csrf::session_check(req.method(), req.headers(), &ext.origin, plain.secret())
    {
        return Err(Box::new(match refused {
            Ok(reason) => csrf::rejected(
                csrf::rejection(reason, &ext.origin),
                req.method(),
                req.uri().path(),
                Some(&identity.public_id),
                request_id,
                lang,
            ),
            Err(err) => ApiError::new(err, request_id, lang).into_response(),
        }));
    }
    req.extensions_mut().insert(CallerProfile {
        username: identity.username,
        display_name: identity.display_name,
        is_admin: identity.is_admin,
        is_agent: false,
        token_scopes: None,
    });
    req.extensions_mut().insert(Principal::authenticated(
        identity.user_id,
        identity.is_admin,
        // `authenticate_session` refuses a disabled user (FG-AUTH-0004).
        true,
        Credential::Session,
    ));
    req.extensions_mut().insert(SessionCredential {
        session_id: identity.session_id,
        public_id: identity.public_id,
        csrf_token,
        idle_expires_at: identity.idle_expires_at,
        absolute_expires_at: identity.absolute_expires_at,
    });
    strip_session_cookie(req.headers_mut());
    Ok(())
}

/// Log the rejection once as `forge.auth.token_rejected` and answer with
/// the standard error body (401 carries the challenge, `error.rs`).
fn token_rejected(
    err: ForgeError,
    scheme: Option<Scheme>,
    request_id: RequestId,
    lang: Lang,
) -> Response {
    log_rejection("forge.auth.token_rejected", &err, scheme, &request_id);
    ApiError::new(err, request_id, lang)
        .already_logged()
        .into_response()
}

/// Log the rejection once as `forge.auth.session_rejected`, answer with the
/// standard error body and clear the cookie (an expired, revoked or
/// unknown session is of no further use; ADR-0021 §5).
fn session_rejected(err: ForgeError, request_id: RequestId, lang: Lang) -> Response {
    log_rejection(
        "forge.auth.session_rejected",
        &err,
        Some(Scheme::Session),
        &request_id,
    );
    let mut res = ApiError::new(err, request_id, lang)
        .already_logged()
        .into_response();
    res.headers_mut()
        .append(SET_COOKIE, HeaderValue::from_static(CLEAR_SESSION_COOKIE));
    res
}

fn log_rejection(
    event: &'static str,
    err: &ForgeError,
    scheme: Option<Scheme>,
    request_id: &RequestId,
) {
    // One representation: the context keys the authenticator and this
    // module set (`reason`; `public_id`, `user`, `revoked_reason` when known)
    // become fields. `user` is the internal numeric id, as FG-AUTH-0004's
    // catalog diagnosis and forge-db's errors already record it.
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
        revoked_reason = field("revoked_reason"),
        scheme = scheme.map(Scheme::as_str),
        "{event}"
    );
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
