//! Acting for a person at the authentication layer (M3 S2, ADR-0025 §1,
//! §2, §5.3, §6): after the token authenticated, a request carrying
//! `Forge-Act-As` (or `Forge-Assertion`) becomes an act-as principal or is
//! refused — **never** served as the token's own owner with the header
//! ignored — git Smart HTTP and raw / archive included (refused as
//! `RepoRead`, review of ADR-0025 §4).
//!
//! Order (each step before the next): the route's action
//! (`act_as_rule() == Denied` → `FG-AUTH-0003 reason=act_as_not_allowed`,
//! nothing parsed or looked up), the headers' shape (`FG-INT-0006`
//! `multiple_headers` / `malformed` / `missing_value` / `email_form`;
//! `FG-AUTH-0020` `multiple_headers` / `malformed` / `unexpected`), the token
//! (`forge_auth::act_as_admission`), then forge-integ's resolution (the
//! integration, the keys, the person, the assertion). A failed
//! resolution of the person counts against the service account's limit
//! (60 a minute, then `FG-WEB-0010`).
//!
//! Only this module builds an act-as principal (`Principal::act_as`, test
//! `only_forge_web_builds_act_as_principals`). The header values never
//! reach a log or an error: they are removed from the request once read;
//! the log carries the form (`user` / `employee`), the first 8 characters
//! of the keyed HMAC of the value and the assertion's public id.
//!
//! After the route answered, [`finish`] logs the one `forge.auth.act_as`
//! line of an accepted act-as request (with the route's error code when it
//! refused) and rejects its verified assertion unless the action consumed
//! it — so a verified assertion is never usable twice. A request cut off
//! before the route answered does the same through the drop of its
//! [`Pending`] guard (`outcome=cancelled`, `request_cancelled`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use forge_auth::{AccountKind, Action, Principal};
use forge_core::act_as::{
    assertion_invalid, ActAsTarget, AssertionToken, TargetRefusal, ACT_AS_HEADER, ASSERTION_HEADER,
};
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError, Lang};

use crate::auth::{Surface, TokenCredential};
use crate::error::{ApiError, Failure};
use crate::routes::namespaces::CallerLimiter;

/// Failed resolutions of the person allowed per service account and
/// window (ADR-0025 §1).
pub const RESOLUTION_FAILURE_LIMIT: u32 = 60;
pub const RESOLUTION_FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// The act-as service and its limiter, as a request extension
/// (`app::build` inserts it for every request).
#[derive(Clone)]
pub(crate) struct ActAsExt {
    pub(crate) act_as: Option<Arc<forge_integ::ActAs>>,
    pub(crate) failures: Arc<CallerLimiter>,
}

/// The limiter of failed resolutions (one per server).
pub(crate) fn failure_limiter() -> CallerLimiter {
    CallerLimiter::new(
        "act_as_resolution",
        RESOLUTION_FAILURE_LIMIT,
        RESOLUTION_FAILURE_WINDOW,
    )
}

/// What the request's `forge.auth.act_as` line names (never a header
/// value or an employee number).
#[derive(Debug, Clone)]
pub(crate) struct ActAsLog {
    pub(crate) action: Action,
    pub(crate) form: &'static str,
    pub(crate) integration: i64,
    pub(crate) via: i64,
    pub(crate) person: i64,
    pub(crate) credential_id: i64,
    pub(crate) assertion_public_id: Option<String>,
    pub(crate) claimed_prefix: String,
}

/// What [`finish`] needs once the route answered.
pub(crate) struct Accepted {
    log: ActAsLog,
    principal: Principal,
    request_id: RequestId,
    act_as: Arc<forge_integ::ActAs>,
}

/// The headers of the request: `Ok(None)` when it carries neither.
struct Headers {
    raw: String,
    target: ActAsTarget,
    assertion: Option<AssertionToken>,
}

fn headers(h: &HeaderMap) -> Result<Option<Headers>, ForgeError> {
    let names: Vec<_> = h.get_all(ACT_AS_HEADER).iter().collect();
    let assertions: Vec<_> = h.get_all(ASSERTION_HEADER).iter().collect();
    if names.is_empty() && assertions.is_empty() {
        return Ok(None);
    }
    if names.len() > 1 {
        return Err(TargetRefusal::MultipleHeaders.error());
    }
    if assertions.len() > 1 {
        return Err(assertion_invalid("multiple_headers"));
    }
    let Some(value) = names.first() else {
        return Err(assertion_invalid("unexpected"));
    };
    let raw = value
        .to_str()
        .map_err(|_| TargetRefusal::Malformed.error())?
        .to_owned();
    let target = ActAsTarget::parse(&raw).map_err(TargetRefusal::error)?;
    let assertion = match assertions.first() {
        None => None,
        Some(v) => {
            let text = v.to_str().map_err(|_| assertion_invalid("malformed"))?;
            Some(AssertionToken::parse(text).map_err(assertion_invalid)?)
        }
    };
    Ok(Some(Headers {
        raw,
        target,
        assertion,
    }))
}

fn strip(h: &mut HeaderMap) {
    h.remove(ACT_AS_HEADER);
    h.remove(ASSERTION_HEADER);
}

fn field<'a>(err: &'a ForgeError, key: &str) -> Option<&'a str> {
    err.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

/// Log a refusal at this layer as the request's one `forge.auth.act_as`
/// line and answer it (the renderer does not log it again).
fn refused(
    err: ForgeError,
    action: Option<Action>,
    via: Option<i64>,
    request_id: RequestId,
    lang: Lang,
) -> Box<Response> {
    tracing::info!(
        target: "forge_web::auth",
        event = "forge.auth.act_as",
        outcome = "refused",
        error_code = err.code().id(),
        request_id = %request_id,
        reason = field(&err, "reason"),
        action = action.map(Action::name),
        act_as_rule = action.map(|a| a.act_as_rule().as_str()),
        actor.via = via,
        integration_id = field(&err, "integration_id"),
        assertion_public_id = field(&err, "assertion_public_id"),
        "forge.auth.act_as"
    );
    Box::new(
        ApiError::new(err, request_id, lang)
            .already_logged()
            .into_response(),
    )
}

/// Whether the request carries `Forge-Act-As` or `Forge-Assertion`.
pub(crate) fn carries_act_as(h: &HeaderMap) -> bool {
    h.contains_key(ACT_AS_HEADER) || h.contains_key(ASSERTION_HEADER)
}

/// A public route (`Action::is_public`: the shell, the assets, liveness /
/// readiness, the sign-in, the back-channel logout) never acts for a
/// person and never authenticates: a request carrying the act-as headers
/// is refused before the route runs — `FG-AUTH-0003
/// reason=act_as_not_allowed`, as on every route outside the allowlist —
/// never served with the headers ignored (ADR-0025 §2; review 2026-11-09,
/// authz MEDIUM). Nothing is parsed or looked up.
pub(crate) fn refuse_on_public(req: &Request, action: Action) -> Result<(), Box<Response>> {
    if !carries_act_as(req.headers()) {
        return Ok(());
    }
    let err = match forge_auth::act_as_route(action) {
        Err(e) => e,
        // A public action is never in the allowlist; should one be, the
        // public route still has no principal to act with.
        Ok(()) => ForgeError::new(ErrorCode::Forbidden)
            .with_context("reason", "act_as_not_allowed")
            .with_context("action", action.name()),
    };
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let lang = crate::context::language(req.headers().get(axum::http::header::ACCEPT_LANGUAGE));
    Err(refused(err, Some(action), None, request_id, lang))
}

/// Per-route middleware of the fixed public routes ([`refuse_on_public`]).
pub(crate) async fn public_route(
    axum::extract::State(action): axum::extract::State<Action>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    match refuse_on_public(&req, action) {
        Ok(()) => next.run(req).await,
        Err(res) => *res,
    }
}

/// The act-as step of the authentication (see the module documentation).
/// `action` is the fixed route's (`None`: a dispatcher word — raw /
/// archive, decided like `RepoRead`, which never acts for a person).
pub(crate) async fn admit(
    req: &mut Request,
    _surface: Surface,
    action: Option<Action>,
    request_id: &RequestId,
    lang: Lang,
) -> Result<Option<Accepted>, Box<Response>> {
    // Review (authz MEDIUM): git Smart HTTP and raw / archive never act for
    // a person, and the header is never ignored: they are refused like
    // every action outside the allowlist (`RepoRead` / `RepoPush` are
    // `Denied`).
    let action = action.unwrap_or(Action::RepoRead);
    let fail = |err: ForgeError, via: Option<i64>| {
        Err(refused(err, Some(action), via, request_id.clone(), lang))
    };
    if !carries_act_as(req.headers()) {
        return Ok(None);
    }
    let Some(token) = req.extensions().get::<Principal>().cloned() else {
        // Review (security LOW a): no credential to act with — refused
        // here, never stripped to continue anonymously.
        strip(req.headers_mut());
        return Err(refused(
            ForgeError::new(ErrorCode::Unauthenticated)
                .with_context("reason", "act_as_without_credential"),
            Some(action),
            None,
            request_id.clone(),
            lang,
        ));
    };
    let via = token.user().map(|u| u.get());
    // A route never acted for is refused whatever the headers say (the
    // feed's `act_as_refused` included: I1c).
    if let Err(e) = forge_auth::act_as_route(action) {
        strip(req.headers_mut());
        return fail(e, via);
    }
    let parsed = headers(req.headers());
    strip(req.headers_mut());
    let parsed = match parsed {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(None),
        Err(e) => return fail(e, via),
    };
    if let Err(e) = forge_auth::act_as_admission(&token) {
        return fail(e, via);
    }
    let ext = req.extensions().get::<ActAsExt>().cloned();
    let Some((act_as, failures)) = ext.and_then(|e| e.act_as.map(|a| (a, e.failures))) else {
        return fail(
            ForgeError::new(ErrorCode::Forbidden).with_context("reason", "act_as_not_configured"),
            via,
        );
    };
    let token_id = req.extensions().get::<TokenCredential>().map(|t| t.0);
    let parts = match act_as
        .resolve(
            &token,
            token_id,
            &parsed.raw,
            &parsed.target,
            parsed.assertion.as_ref(),
        )
        .await
    {
        Ok(parts) => parts,
        Err(e) if e.code() == ErrorCode::ActAsTargetInvalid => {
            let over = token
                .user()
                .and_then(|u| failures.check(u, Instant::now()).err());
            return match over {
                Some(retry) => fail(
                    failures
                        .refused(retry)
                        .with_context("reason", "act_as_resolution_failures"),
                    via,
                ),
                None => fail(e, via),
            };
        }
        Err(e) => return fail(e, via),
    };
    let log = ActAsLog {
        action,
        form: parsed.target.form(),
        integration: parts.credential.integration.get(),
        via: parts.credential.via.get(),
        person: parts.person.get(),
        credential_id: parts.credential.via_token,
        assertion_public_id: parts
            .credential
            .assertion
            .as_ref()
            .map(|a| a.public_id.clone()),
        claimed_prefix: parts.credential.claimed_mac.chars().take(8).collect(),
    };
    let principal = Principal::act_as(parts.person, true, AccountKind::Human, parts.credential);
    req.extensions_mut().insert(principal.clone()); // clone: finish() keeps one
    Ok(Some(Accepted {
        log,
        principal,
        request_id: request_id.clone(),
        act_as,
    }))
}

/// What [`finish`] reads of the route's answer (owned: a `Response` is
/// not `Sync`, so it is never held across an await).
pub(crate) fn answered(res: &Response) -> (axum::http::StatusCode, Option<Failure>) {
    (res.status(), res.extensions().get::<Failure>().cloned())
}

/// After the route answered (see the module documentation).
pub(crate) async fn finish(
    accepted: &Accepted,
    (status, failure): (axum::http::StatusCode, Option<Failure>),
) {
    let failure = failure.as_ref();
    let l = &accepted.log;
    tracing::info!(
        target: "forge_web::auth",
        event = "forge.auth.act_as",
        outcome = if failure.is_some() { "refused" } else { "allowed" },
        status = status.as_u16(),
        error_code = failure.map(|f| f.code.id()),
        reason = failure.and_then(|f| f.reason.as_deref()),
        request_id = %accepted.request_id,
        action = l.action.name(),
        act_as_rule = l.action.act_as_rule().as_str(),
        target_form = l.form,
        actor.user = l.person,
        actor.via = l.via,
        integration_id = l.integration,
        credential_id = l.credential_id,
        assertion_public_id = l.assertion_public_id.as_deref(),
        claimed_mac = l.claimed_prefix.as_str(),
        "forge.auth.act_as"
    );
    if l.assertion_public_id.is_none() {
        return;
    }
    let reason = if status.is_success() {
        "not_used"
    } else if status.is_client_error() {
        "action_refused"
    } else {
        "action_failed"
    };
    reject_unused(accepted, reason).await;
}

/// Reject the request's verified assertion unless the action consumed it,
/// logged once here (success or failure, with its code).
async fn reject_unused(accepted: &Accepted, reason: &'static str) {
    let public_id = accepted.log.assertion_public_id.as_deref();
    match accepted
        .act_as
        .reject_unused(&accepted.principal, &accepted.request_id, reason)
        .await
    {
        Ok(true) => tracing::info!(
            target: "forge_web::auth",
            event = "forge.auth.act_as_assertion_rejected",
            request_id = %accepted.request_id,
            assertion_public_id = public_id,
            reject_reason = reason,
            "a verified assertion the action did not consume was rejected"
        ),
        Ok(false) => {}
        // The assertion stays claimed until it expires (5 minutes); the
        // failure is logged once, here, with its code.
        Err(e) => tracing::error!(
            target: "forge_web::auth",
            event = "forge.auth.act_as_assertion_reject_failed",
            error_code = e.code().id(),
            request_id = %accepted.request_id,
            assertion_public_id = public_id,
            reject_reason = reason,
            error = %e,
            "the unused assertion could not be rejected; it expires by itself (runbook act-as)"
        ),
    }
}

/// An accepted act-as request between [`admit`] and [`finish`] (review of
/// ADR-0025, 2026-11-09, security LOW b). When the request is cut off
/// before [`finish`] completed (the client went away, so the server drops
/// the request's future — while the route runs or while `finish` waits on
/// the database), dropping the guard
/// logs the request as `cancelled` and rejects its verified assertion
/// (`request_cancelled`) in a task of its own — so it never stays
/// `claimed` until it expires. The task is one `reject_unused` (one
/// short transaction, bounded by the pool's acquire and statement
/// timeouts); without a runtime (never in `forge serve`) the failure is
/// logged and the assertion expires by itself (5 minutes, as before).
pub(crate) struct Pending(Option<Accepted>);

impl Pending {
    pub(crate) fn new(accepted: Accepted) -> Self {
        Self(Some(accepted))
    }

    /// The accepted request, for [`finish`]: the guard stays armed while
    /// `finish` runs (a cut-off inside it still rejects the assertion;
    /// `reject_unused` is idempotent — it touches a `claimed` row only).
    pub(crate) fn accepted(&self) -> Option<&Accepted> {
        self.0.as_ref()
    }

    /// `finish` completed: the guard has nothing left to do.
    pub(crate) fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        let Some(accepted) = self.0.take() else {
            return;
        };
        let l = &accepted.log;
        // The route may have run to its end (and committed) before the cut:
        // `cancelled` says the answer never reached the client, not that
        // nothing happened.
        tracing::info!(
            target: "forge_web::auth",
            event = "forge.auth.act_as",
            outcome = "cancelled",
            note = "the request was cut off before it answered; the action may already have been committed",
            request_id = %accepted.request_id,
            action = l.action.name(),
            act_as_rule = l.action.act_as_rule().as_str(),
            target_form = l.form,
            actor.user = l.person,
            actor.via = l.via,
            integration_id = l.integration,
            credential_id = l.credential_id,
            assertion_public_id = l.assertion_public_id.as_deref(),
            claimed_mac = l.claimed_prefix.as_str(),
            "forge.auth.act_as"
        );
        if l.assertion_public_id.is_none() {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                // Logged before the task runs: a task lost at the runtime's
                // shutdown leaves this line without its `rejected` /
                // `reject_failed` follow-up (the assertion then expires).
                tracing::info!(
                    target: "forge_web::auth",
                    event = "forge.auth.act_as_assertion_reject_scheduled",
                    request_id = %accepted.request_id,
                    assertion_public_id = l.assertion_public_id.as_deref(),
                    reject_reason = "request_cancelled",
                    "the unused assertion of a cut-off request will be rejected"
                );
                runtime.spawn(async move {
                    reject_unused(&accepted, "request_cancelled").await;
                });
            }
            Err(_) => tracing::error!(
                target: "forge_web::auth",
                event = "forge.auth.act_as_assertion_reject_failed",
                error_code = ErrorCode::Internal.id(),
                request_id = %accepted.request_id,
                assertion_public_id = l.assertion_public_id.as_deref(),
                reject_reason = "request_cancelled",
                reason = "no_runtime",
                "the unused assertion of a cut-off request could not be rejected; it expires by itself (runbook act-as)"
            ),
        }
    }
}

#[cfg(test)]
#[path = "act_as_tests.rs"]
mod tests;
