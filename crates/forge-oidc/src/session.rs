//! The identity provider's side of a running web session (M2 MR5,
//! ADR-0021 §7, §8):
//!
//! - [`RefreshingSessions`]: a session authenticator around the database's
//!   one. When a cookie-authenticated request finds the session's last
//!   check older than 5 minutes (`SessionIdentity::refresh_due`), the one
//!   request that wins the claim (`forge_db::sessions::claim_refresh`)
//!   refreshes at the provider (3 s) and decides again with
//!   `authorize(anonymous, SessionContinue, Admission)`. `invalid_grant`, a
//!   refusal, a changed `sub` / `iss`, an unverifiable ID token or a fourth
//!   answer in a row without an ID token revoke the session and make this
//!   request `FG-AUTH-0001 reason=session_revoked`. A provider that cannot
//!   be reached (`FG-AUTH-0015`, logged as warn once per claim, so once per
//!   session per 5 minutes) leaves the session as it is: signed-in people
//!   keep working, and the absolute lifetime never moves.
//! - [`Oidc::logout`] (`POST /-/auth/logout`): revoke the session
//!   (`reason=logout`) and build the provider's `end_session_endpoint` URL
//!   with the sealed ID token as `id_token_hint`.
//! - [`Oidc::backchannel_logout`] (`POST /-/auth/backchannel-logout`):
//!   verify the logout token (`FG-AUTH-0017`), refuse a `jti` seen in the
//!   last 10 minutes, and revoke the issuer's sessions of its `sid` (or,
//!   without one, its `sub`) with `reason=backchannel`.
//!
//! Tokens, claims and the provider's bodies are never logged.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use forge_auth::{authorize, Action, AdmissionAccount, Principal, Resource};
use forge_core::authenticator::{SessionAuthFuture, SessionIdentity};
use forge_core::{RequestId, SessionAuthenticator, SessionRevokeReason};
use forge_db::change::{Actor, ChangeContext};
use forge_db::identities::LoginAccount;
use forge_db::sessions::{self, LogoutTarget, RefreshClaim, Refreshed, RevokeOutcome};
use forge_errors::{ErrorCode, ForgeError};
use serde_json::Value;

use crate::discovery::Discovery;
use crate::error::{internal, loggable, logout_token, unavailable};
use crate::flow::{now_secs, Oidc};
use crate::jwt::{Expected, IdClaims};

/// Answers in a row without an ID token the check accepts (the access
/// role cannot be confirmed without one): the fourth revokes the session
/// (`id_token_missing`, fail closed; ADR-0021 §7, MR5 decision 29).
pub const MAX_REFRESH_WITHOUT_ID_TOKEN: i16 = 3;

/// How long a back-channel logout's `jti` is remembered (ADR-0021 §8).
pub const JTI_MEMORY_SECS: u64 = 600;
/// The most `jti`s remembered at once (`replay_table_full` beyond).
pub const MAX_JTIS: usize = 10_000;

/// The seen `jti`s of back-channel logouts, in this process (several
/// processes would need a table: deferred, ADR-0021 §8).
#[derive(Debug, Default)]
pub(crate) struct JtiCache {
    seen: Mutex<HashMap<String, u64>>,
}

impl JtiCache {
    /// Remember `jti` at `now`: `replayed` when it was seen in the last
    /// [`JTI_MEMORY_SECS`]; `replay_table_full` when [`MAX_JTIS`] fresh ones
    /// are held (a flood: refused rather than forgetting a fresh one).
    pub(crate) fn remember(&self, jti: &str, now: u64) -> Result<(), ForgeError> {
        let mut seen = self
            .seen
            .lock()
            .map_err(|_| internal("jti_table_poisoned"))?;
        let fresh = |at: u64| now < at.saturating_add(JTI_MEMORY_SECS);
        if seen.get(jti).copied().is_some_and(fresh) {
            return Err(logout_token("replayed"));
        }
        if seen.len() >= MAX_JTIS && !seen.contains_key(jti) {
            seen.retain(|_, at| fresh(*at));
            if seen.len() >= MAX_JTIS {
                return Err(logout_token("replay_table_full"));
            }
        }
        seen.insert(jti.to_owned(), now);
        Ok(())
    }

    /// Forget `jti` after its logout failed for another reason (the
    /// provider's retry must not be a replay).
    pub(crate) fn forget(&self, jti: &str) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.remove(jti);
        }
    }
}

/// The RP logout's request: whose session, and the audit's request facts.
#[derive(Debug, Clone, Copy)]
pub struct LogoutRequest<'a> {
    pub request_id: &'a RequestId,
    /// The session's user (the audit actor).
    pub user: forge_core::UserId,
    /// The request's own session (`forge.web_sessions.id`).
    pub session_id: i64,
    pub source_ip: Option<IpAddr>,
    pub user_agent: Option<&'a str>,
}

/// What the refresh check decided for this request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshVerdict {
    /// Go on (refreshed, not due, another request holds the claim, or the
    /// provider could not be reached).
    Continue,
    /// The session was revoked with this reason.
    Revoked(SessionRevokeReason),
}

/// What the token endpoint said to a refresh.
enum Grant {
    Tokens {
        id_token: Option<String>,
        refresh_token: Option<String>,
    },
    /// `invalid_grant`: the provider ended the session or the person.
    Rejected,
}

/// Log a failure that does not stop the request (warn whatever the code's
/// severity: an unreachable provider during the refresh is expected to
/// pass, ADR-0021 §7).
fn warn_and_go_on(e: &ForgeError, request_id: &RequestId, public_id: &str) {
    let context: Vec<String> = e
        .context()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    tracing::warn!(
        target: "forge_oidc",
        request_id = %request_id,
        error_code = e.code().id(),
        public_id,
        context = %context.join(" "),
        "forge.auth.session_refresh_failed"
    );
}

fn system(request_id: &RequestId) -> ChangeContext {
    ChangeContext::new(Actor::System, request_id.clone()) // clone: the change owns its id
}

impl Oidc {
    /// The refresh check of one cookie-authenticated request (see the
    /// module documentation). `Err` only when the database fails (the
    /// request is then `FG-DB-0001`, like its authentication).
    ///
    /// The check runs as its own task, awaited here: a request that goes
    /// away (the browser closes, the request times out) does not stop it
    /// between the token endpoint's answer and `store_refreshed`. The
    /// provider rotates refresh tokens, so losing the rotated one would make
    /// the next refresh `invalid_grant` and end a good session (review of
    /// MR5, MEDIUM).
    pub async fn refresh_check(
        self: &Arc<Self>,
        request_id: &RequestId,
        identity: &SessionIdentity,
    ) -> Result<RefreshVerdict, ForgeError> {
        let me = Arc::clone(self);
        let request_id_owned = request_id.clone(); // clone: the task owns its id
        let session_id = identity.session_id;
        tokio::spawn(async move { me.refresh_detached(&request_id_owned, session_id).await })
            .await
            .map_err(|e| internal("refresh_task_failed").with_context("panicked", e.is_panic()))?
    }

    async fn refresh_detached(
        &self,
        request_id: &RequestId,
        session_id: i64,
    ) -> Result<RefreshVerdict, ForgeError> {
        let Some(claim) = sessions::claim_refresh(&self.db, session_id).await? else {
            return Ok(RefreshVerdict::Continue);
        };
        let aad = claim.public_id.as_bytes();
        let refresh_token = match self
            .refresh_keys
            .open(aad, &claim.refresh_token_enc)
            .map(String::from_utf8)
        {
            Ok(Ok(token)) => token,
            Ok(Err(_)) | Err(_) => {
                // The key file no longer opens it (a kid removed too early)
                // or the row was altered: it can never refresh again.
                let e = ForgeError::new(ErrorCode::CredentialDataInvalid)
                    .with_context("reason", "refresh_token_unreadable")
                    .with_context("public_id", &claim.public_id);
                forge_core::telemetry::log_failure(&e, request_id);
                return self
                    .end(
                        request_id,
                        &claim,
                        SessionRevokeReason::IdpRevoked,
                        "refresh_token_unreadable",
                    )
                    .await;
            }
        };
        let doc = match self.discovered().await {
            Ok(doc) => doc,
            Err(e) => {
                warn_and_go_on(&e, request_id, &claim.public_id);
                return Ok(RefreshVerdict::Continue);
            }
        };
        let (id_token, new_refresh) = match self.refresh_grant(&doc, &refresh_token).await {
            Ok(Grant::Tokens {
                id_token,
                refresh_token,
            }) => (id_token, refresh_token),
            Ok(Grant::Rejected) => {
                return self
                    .end(
                        request_id,
                        &claim,
                        SessionRevokeReason::IdpRevoked,
                        "invalid_grant",
                    )
                    .await;
            }
            Err(e) => {
                warn_and_go_on(&e, request_id, &claim.public_id);
                return Ok(RefreshVerdict::Continue);
            }
        };
        let access_role = match id_token.as_deref() {
            Some(token) => match self.verify_refreshed(&doc, token, &claim).await {
                Ok(claims) => self.has_access_role(&claims.claims),
                Err(e) if e.code() == ErrorCode::IdentityProviderUnavailable => {
                    // The JWKS could not be fetched: nothing was learned.
                    warn_and_go_on(&e, request_id, &claim.public_id);
                    return Ok(RefreshVerdict::Continue);
                }
                Err(e) => {
                    forge_core::telemetry::log_failure(
                        &e.with_context("public_id", &claim.public_id),
                        request_id,
                    );
                    return self
                        .end(
                            request_id,
                            &claim,
                            SessionRevokeReason::IdpRevoked,
                            "id_token_invalid",
                        )
                        .await;
                }
            },
            None if claim.without_id_token >= MAX_REFRESH_WITHOUT_ID_TOKEN => {
                return self
                    .end(
                        request_id,
                        &claim,
                        SessionRevokeReason::IdpRevoked,
                        "id_token_missing",
                    )
                    .await;
            }
            None => {
                tracing::info!(
                    target: "forge_oidc",
                    request_id = %request_id,
                    public_id = %claim.public_id,
                    in_a_row = claim.without_id_token + 1,
                    "forge.auth.session_refresh_without_id_token"
                );
                // The last confirmed role stands (ADR-0021 §7, row 32): a
                // live session's last confirmed role is always "present" —
                // the session was issued only with it (SessionCreate), and
                // every refresh whose ID token lacked it revoked the
                // session. The account is still read again below.
                true
            }
        };
        let account = match sessions::account_of(&self.db, claim.user).await? {
            LoginAccount::Active => AdmissionAccount::Active,
            LoginAccount::Disabled => AdmissionAccount::Disabled,
            LoginAccount::NotHuman => AdmissionAccount::NotHuman,
            LoginAccount::New => AdmissionAccount::New,
        };
        if let Err(denied) = authorize(
            &Principal::anonymous(),
            Action::SessionContinue,
            &Resource::Admission {
                access_role,
                account,
            },
        ) {
            forge_core::telemetry::log_failure(
                &denied.with_context("public_id", &claim.public_id),
                request_id,
            );
            let reason = if account == AdmissionAccount::Disabled {
                SessionRevokeReason::UserDisabled
            } else {
                SessionRevokeReason::IdpRevoked
            };
            return self
                .end(request_id, &claim, reason, "session_continue_denied")
                .await;
        }
        let sealed_refresh = new_refresh
            .as_deref()
            .map(|t| self.refresh_keys.seal(aad, t.as_bytes()))
            .transpose()?;
        let sealed_id = id_token
            .as_deref()
            .map(|t| self.id_keys.seal(aad, t.as_bytes()))
            .transpose()?;
        // `false`: revoked meanwhile; the next request sees it.
        let _stored = sessions::store_refreshed(
            &self.db,
            claim.session_id,
            Refreshed {
                refresh_token_enc: sealed_refresh.as_deref(),
                id_token_enc: sealed_id.as_deref(),
            },
        )
        .await?;
        Ok(RefreshVerdict::Continue)
    }

    /// Revoke the claimed session (`why` is logged with the reason).
    async fn end(
        &self,
        request_id: &RequestId,
        claim: &RefreshClaim,
        reason: SessionRevokeReason,
        why: &'static str,
    ) -> Result<RefreshVerdict, ForgeError> {
        let _outcome =
            sessions::revoke(&self.db, &system(request_id), claim.session_id, reason).await?;
        tracing::info!(
            target: "forge_oidc",
            request_id = %request_id,
            public_id = %claim.public_id,
            user_id = claim.user.get(),
            revoked_reason = reason.as_str(),
            why,
            "forge.auth.session_refresh_revoked"
        );
        Ok(RefreshVerdict::Revoked(reason))
    }

    /// `grant_type=refresh_token` at the token endpoint (3 s).
    async fn refresh_grant(
        &self,
        doc: &Discovery,
        refresh_token: &str,
    ) -> Result<Grant, ForgeError> {
        let reply = self
            .http
            .post_form(
                "refresh",
                &doc.token_endpoint,
                (&self.settings.client_id, &self.settings.client_secret),
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                    ("scope", "openid"),
                ],
                self.settings.timeouts.refresh,
            )
            .await?;
        let body: Option<Value> = serde_json::from_slice(&reply.body).ok();
        match reply.status {
            200 => {}
            400 | 401 => {
                let oauth = body
                    .as_ref()
                    .and_then(|b| b.get("error"))
                    .and_then(Value::as_str)
                    .map_or_else(|| "none".to_owned(), loggable);
                if oauth == "invalid_grant" {
                    return Ok(Grant::Rejected);
                }
                // The client itself, or a request the provider does not
                // take: nothing is known about the person.
                return Err(unavailable("refresh", "client_rejected")
                    .with_context("status", reply.status)
                    .with_context("oauth_error", oauth));
            }
            other => return Err(unavailable("refresh", "status").with_context("status", other)),
        }
        let body = body
            .ok_or_else(|| unavailable("refresh", "invalid_json").with_context("detail", "json"))?;
        let text = |name: &str| {
            body.get(name)
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
        };
        Ok(Grant::Tokens {
            id_token: text("id_token"),
            refresh_token: text("refresh_token"),
        })
    }

    /// A refreshed ID token: ADR-0021 §9 without `nonce`, then the same
    /// `iss` and `sub` as the session (`FG-AUTH-0012 reason=subject_changed`).
    async fn verify_refreshed(
        &self,
        doc: &Discovery,
        token: &str,
        claim: &RefreshClaim,
    ) -> Result<IdClaims, ForgeError> {
        let kid = self.verifier.kid(token)?;
        let keys = self
            .jwks
            .for_kid(
                &self.http,
                &doc.jwks_uri,
                &kid,
                self.settings.timeouts.fetch,
            )
            .await?;
        let expected = Expected {
            issuer: &self.settings.issuer,
            client_id: &self.settings.client_id,
            nonce: None,
            now: now_secs()?,
            leeway: 60,
        };
        let claims = self.verifier.verify_id_token(token, &keys, &expected)?;
        if claims.subject != claim.subject || self.settings.issuer != claim.issuer {
            return Err(crate::error::id_token("subject_changed"));
        }
        Ok(claims)
    }

    /// `POST /-/auth/logout`: revoke `session_id` (`reason=logout`, actor
    /// the user) and return where to send the browser — the provider's
    /// `end_session_endpoint` with `id_token_hint`, `client_id` and
    /// `post_logout_redirect_uri` = `web.base_url` + `/`; the home page when
    /// the provider has no such endpoint or cannot be reached (logged; the
    /// person is signed out of GitStation either way).
    pub async fn logout(&self, request: LogoutRequest<'_>) -> Result<String, ForgeError> {
        let mut ctx = ChangeContext::new(Actor::User(request.user), request.request_id.clone()); // clone: the change owns its id
        ctx.credential_id = Some(request.session_id);
        ctx.source_ip = request.source_ip;
        ctx.user_agent = request.user_agent.map(|u| u.chars().take(512).collect());
        let ctx = &ctx;
        let session_id = request.session_id;
        let view = sessions::logout_view(&self.db, session_id).await?;
        let mut hint = None;
        if let Some((public_id, sealed)) = view {
            let _outcome: RevokeOutcome =
                sessions::revoke(&self.db, ctx, session_id, SessionRevokeReason::Logout).await?;
            if let Some(sealed) = sealed {
                match self
                    .id_keys
                    .open(public_id.as_bytes(), &sealed)
                    .map(String::from_utf8)
                {
                    Ok(Ok(token)) => hint = Some(token),
                    Ok(Err(_)) | Err(_) => forge_core::telemetry::log_failure(
                        &ForgeError::new(ErrorCode::CredentialDataInvalid)
                            .with_context("reason", "id_token_unreadable")
                            .with_context("public_id", &public_id),
                        &ctx.request_id,
                    ),
                }
            }
        }
        let home = format!("{}/", self.settings.base_origin);
        let doc = match self.discovered().await {
            Ok(doc) => doc,
            Err(e) => {
                warn_and_go_on(&e, &ctx.request_id, "-");
                return Ok(home);
            }
        };
        let Some(endpoint) = doc.end_session_endpoint.as_deref() else {
            return Ok(home);
        };
        // The session is revoked by now: a bad endpoint must not turn the
        // logout into an error page that keeps the cookie (review of MR5).
        let Ok(mut url) = url::Url::parse(endpoint) else {
            let e = unavailable("discovery", "invalid_json")
                .with_context("detail", "end_session_endpoint");
            warn_and_go_on(&e, &ctx.request_id, "-");
            return Ok(home);
        };
        {
            let mut q = url.query_pairs_mut();
            if let Some(hint) = hint.as_deref() {
                q.append_pair("id_token_hint", hint);
            }
            q.append_pair("client_id", &self.settings.client_id);
            q.append_pair("post_logout_redirect_uri", &home);
        }
        Ok(url.into())
    }

    /// `POST /-/auth/backchannel-logout`: verify `token` and revoke the
    /// sessions it names (see the module documentation). Returns how many
    /// were revoked (0 is a success: the provider must not retry).
    ///
    /// Verification, the `jti` and the revocations run as one task, awaited
    /// here: when the provider's request goes away mid-way (it disconnects,
    /// the request times out), the task still revokes what it remembered
    /// the `jti` for, so the provider's retry being `replayed` never leaves
    /// a session alive (review of MR5, HIGH).
    pub async fn backchannel_logout(
        self: &Arc<Self>,
        request_id: &RequestId,
        token: &str,
        source_ip: Option<IpAddr>,
    ) -> Result<usize, ForgeError> {
        let me = Arc::clone(self);
        let request_id_owned = request_id.clone(); // clone: the task owns its id
        let token = token.to_owned();
        tokio::spawn(async move {
            me.backchannel_detached(&request_id_owned, &token, source_ip)
                .await
        })
        .await
        .map_err(|e| internal("backchannel_task_failed").with_context("panicked", e.is_panic()))?
    }

    async fn backchannel_detached(
        &self,
        request_id: &RequestId,
        token: &str,
        source_ip: Option<IpAddr>,
    ) -> Result<usize, ForgeError> {
        let doc = self.discovered().await?;
        let kid = self.verifier.logout_kid(token)?;
        let keys = self
            .jwks
            .for_kid(
                &self.http,
                &doc.jwks_uri,
                &kid,
                self.settings.timeouts.fetch,
            )
            .await
            .map_err(as_logout_error)?;
        let now = now_secs()?;
        let expected = Expected {
            issuer: &self.settings.issuer,
            client_id: &self.settings.client_id,
            nonce: None,
            now,
            leeway: 60,
        };
        let claims = self.verifier.verify_logout_token(token, &keys, &expected)?;
        self.jtis.remember(&claims.jti, now)?;
        let revoked = self.revoke_named(request_id, &claims, source_ip).await;
        if revoked.is_err() {
            self.jtis.forget(&claims.jti);
        }
        let revoked = revoked?;
        tracing::info!(
            target: "forge_oidc",
            request_id = %request_id,
            by = if claims.sid.is_some() { "sid" } else { "sub" },
            revoked,
            "forge.auth.backchannel_logout"
        );
        Ok(revoked)
    }

    async fn revoke_named(
        &self,
        request_id: &RequestId,
        claims: &crate::jwt::LogoutClaims,
        source_ip: Option<IpAddr>,
    ) -> Result<usize, ForgeError> {
        let target = match (&claims.sid, &claims.sub) {
            (Some(sid), sub) => LogoutTarget::Sid {
                sid,
                sub: sub.as_deref(),
            },
            (None, Some(sub)) => LogoutTarget::Subject(sub),
            (None, None) => {
                return Err(logout_token("missing_claim").with_context("claim", "sid_or_sub"))
            }
        };
        let mut ctx = system(request_id);
        ctx.source_ip = source_ip;
        let mut revoked = 0;
        let mut mismatch_logged = false;
        // Batches until none is left: a revoked row is not a target any
        // more, so every round shrinks the set (review of MR5: no silent
        // leftover beyond one batch).
        loop {
            let batch =
                sessions::backchannel_targets(&self.db, &self.settings.issuer, target).await?;
            if batch.other_subject > 0 && !mismatch_logged {
                mismatch_logged = true;
                tracing::warn!(
                    target: "forge_oidc",
                    request_id = %request_id,
                    other_subject = batch.other_subject,
                    "forge.auth.backchannel_logout_subject_mismatch"
                );
            }
            if batch.ids.is_empty() {
                break;
            }
            for id in batch.ids {
                if sessions::revoke(&self.db, &ctx, id, SessionRevokeReason::Backchannel).await?
                    == RevokeOutcome::Revoked
                {
                    revoked += 1;
                }
            }
        }
        Ok(revoked)
    }
}

/// A JWKS lookup's `FG-AUTH-0012` (unknown kid, a broken key) is the
/// logout token's `FG-AUTH-0017` with the same context; `FG-AUTH-0015`
/// (the provider) stays.
fn as_logout_error(e: ForgeError) -> ForgeError {
    if e.code() != ErrorCode::IdTokenInvalid {
        return e;
    }
    e.context().iter().fold(
        ForgeError::new(ErrorCode::LogoutTokenInvalid),
        |acc, (k, v)| acc.with_context(k, v),
    )
}

/// A session authenticator that runs the refresh check (ADR-0021 §7) on
/// the sessions the inner authenticator accepted and marked
/// `refresh_due`. `forge serve` wraps the bounded database authenticator
/// with it when `auth.oidc` is configured; the provider call runs outside
/// the database permits. The inner store must defer the touch of a due
/// session (`SessionStore::deferring_touch_to_refresh_check`): this wrapper
/// records the activity after a `Continue`.
pub struct RefreshingSessions {
    inner: Arc<dyn SessionAuthenticator>,
    oidc: Arc<Oidc>,
}

impl RefreshingSessions {
    pub fn new(inner: Arc<dyn SessionAuthenticator>, oidc: Arc<Oidc>) -> Self {
        Self { inner, oidc }
    }
}

impl SessionAuthenticator for RefreshingSessions {
    fn authenticate_session<'a>(
        &'a self,
        request_id: &'a RequestId,
        presented: &'a str,
    ) -> SessionAuthFuture<'a> {
        Box::pin(async move {
            let identity = self
                .inner
                .authenticate_session(request_id, presented)
                .await?;
            if !identity.refresh_due {
                return Ok(identity);
            }
            match self.oidc.refresh_check(request_id, &identity).await? {
                RefreshVerdict::Continue => {
                    // The store deferred the activity record of a due
                    // session to here: a session the check revokes is never
                    // extended first (review of MR5).
                    let mut identity = identity;
                    if let Some(moved) = sessions::touch_session(
                        &self.oidc.db,
                        request_id,
                        identity.session_id,
                        self.oidc.settings.session_idle,
                    )
                    .await
                    {
                        identity.idle_expires_at = moved;
                    }
                    Ok(identity)
                }
                RefreshVerdict::Revoked(reason) => Err(ForgeError::new(ErrorCode::Unauthenticated)
                    .with_context("reason", "session_revoked")
                    .with_context("public_id", &identity.public_id)
                    .with_context("revoked_reason", reason.as_str())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_jti_is_refused_while_remembered_then_forgotten() {
        let cache = JtiCache::default();
        cache.remember("synthetic-jti-1", 1_000).expect("first");
        let err = cache
            .remember("synthetic-jti-1", 1_100)
            .expect_err("replay");
        assert_eq!(err.code(), ErrorCode::LogoutTokenInvalid);
        assert!(err.to_string().contains("reason=replayed"), "{err}");
        cache
            .remember("synthetic-jti-1", 1_000 + JTI_MEMORY_SECS)
            .expect("after 10 minutes");
        cache.forget("synthetic-jti-1");
        cache.remember("synthetic-jti-1", 1_700).expect("forgotten");
    }

    #[test]
    fn a_full_jti_table_drops_old_entries_and_refuses_a_flood() {
        let cache = JtiCache::default();
        for i in 0..MAX_JTIS {
            cache.remember(&format!("old-{i}"), 0).expect("fill");
        }
        // The old ones expired: the newcomer fits.
        cache
            .remember("new-0", JTI_MEMORY_SECS)
            .expect("room after expiry");
        for i in 1..MAX_JTIS {
            cache
                .remember(&format!("new-{i}"), JTI_MEMORY_SECS)
                .expect("fill fresh");
        }
        let err = cache
            .remember("one-too-many", JTI_MEMORY_SECS + 1)
            .expect_err("full of fresh ones");
        assert!(
            err.to_string().contains("reason=replay_table_full"),
            "{err}"
        );
    }

    #[test]
    fn a_jwks_refusal_becomes_a_logout_token_refusal_with_its_context() {
        let e = as_logout_error(
            ForgeError::new(ErrorCode::IdTokenInvalid).with_context("reason", "unknown_kid"),
        );
        assert_eq!(e.code(), ErrorCode::LogoutTokenInvalid);
        assert!(e.to_string().contains("reason=unknown_kid"), "{e}");
        let provider = as_logout_error(unavailable("jwks", "timeout"));
        assert_eq!(provider.code(), ErrorCode::IdentityProviderUnavailable);
    }
}
