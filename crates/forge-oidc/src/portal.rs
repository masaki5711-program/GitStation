//! The PFS portal (ADR-0036): the widget routes' token check and the
//! gitstation client's client-credentials token for the news.
//!
//! - [`PortalWidgets`] ([`forge_core::portal::PortalAuthenticator`]): the
//!   portal forwards the person's access token (`azp = portal`). It is
//!   verified like the sign-in's access token (the same JWKS and
//!   algorithms, `iss`, `exp` / `nbf` / `iat`, `typ`, `sub`) with
//!   **`portal.client_id` as the party**: a token another client was
//!   issued — the gitstation client's own included — is refused here
//!   (`FG-AUTH-0026 reason=azp`) before anything is read from the
//!   database. `authorize()` then decides the party again (defence in
//!   depth) and whether the token holds the access role on
//!   `auth.oidc.client_id`. The person is found read-only by the sign-in's
//!   link rules ([`forge_db::portal::portal_user`], with
//!   `auth.oidc.link_auth_sources`). Failures: `FG-AUTH-0026` (the token,
//!   with the reasons of `FG-AUTH-0025`), `FG-AUTH-0027` (no linked user),
//!   `FG-AUTH-0015` (the provider cannot be reached).
//! - [`ServiceTokens`] ([`forge_core::portal::ServiceTokenSource`]): the
//!   token endpoint from discovery, `grant_type=client_credentials` with
//!   `client_secret_basic`, cached until 30 s before `expires_in` ends.
//!   Failures: `FG-INT-0024` with `reason`.

use std::time::{Duration, Instant};

use forge_core::config::OidcClaimsSource;
use forge_core::portal::{
    PortalAuthFuture, PortalAuthenticator, PortalIdentity, ServiceToken, ServiceTokenFuture,
    ServiceTokenSource,
};
use forge_core::RequestId;
use forge_db::identities::{LinkAuthSources, LinkRules};
use forge_errors::{ErrorCode, ForgeError};
use serde_json::Value;

use crate::discovery::DiscoveryCache;
use crate::error::loggable;
use crate::flow::{now_secs, Oidc, OidcSettings};
use crate::http::IdpClient;
use crate::jwt::Expected;

/// The widget routes' authenticator (one per process).
pub struct PortalWidgets {
    oidc: std::sync::Arc<Oidc>,
    /// `portal.client_id`.
    portal_client: String,
}

impl std::fmt::Debug for PortalWidgets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalWidgets")
            .field("portal_client", &self.portal_client)
            .finish_non_exhaustive()
    }
}

/// A verification failure of the access token as the portal's
/// `FG-AUTH-0026`, with the same context.
fn as_portal(e: ForgeError) -> ForgeError {
    if e.code() != ErrorCode::AccessTokenInvalid {
        return e;
    }
    e.context().iter().fold(
        ForgeError::new(ErrorCode::PortalTokenInvalid),
        |err, (key, value)| err.with_context(key, value),
    )
}

impl PortalWidgets {
    pub fn new(oidc: std::sync::Arc<Oidc>, portal_client: &str) -> Self {
        Self {
            oidc,
            portal_client: portal_client.to_owned(),
        }
    }

    /// The sign-in's link rules (`Oidc::callback`), for the read-only
    /// lookup.
    fn link_rules(&self) -> LinkRules {
        match self.oidc.settings.claims_source {
            OidcClaimsSource::IdToken => LinkRules::VerifiedEmail,
            OidcClaimsSource::AccessToken => LinkRules::EmployeeNumberOnly {
                auth_sources: LinkAuthSources::from_names(&self.oidc.settings.link_auth_sources),
            },
        }
    }

    async fn authenticate(&self, token: &str) -> Result<PortalIdentity, ForgeError> {
        let oidc = &self.oidc;
        let kid = oidc.verifier.access_kid(token).map_err(as_portal)?;
        let doc = oidc.discovered().await?;
        let keys = oidc
            .jwks
            .for_kid(
                &oidc.http,
                &doc.jwks_uri,
                &kid,
                oidc.settings.timeouts.fetch,
            )
            .await?;
        let expected = Expected {
            issuer: &oidc.settings.issuer,
            // The party is the portal, whatever the token claims: another
            // client's token ends here (`reason=azp`), before the lookup.
            client_id: &self.portal_client,
            nonce: None,
            now: now_secs()?,
            leeway: 60,
        };
        let claims = oidc
            .verifier
            .verify_access_token(token, &keys, &expected)
            .map_err(as_portal)?;
        let access_role = oidc.has_access_role(&claims.claims);
        let staff_id = claims
            .claims
            .get(&oidc.settings.employee_number_claim)
            .and_then(Value::as_str);
        let user = forge_db::portal::portal_user(
            &oidc.db,
            &oidc.settings.issuer,
            &claims.subject,
            staff_id,
            oidc.settings.auto_link,
            self.link_rules(),
        )
        .await?;
        Ok(PortalIdentity {
            user_id: user.user,
            active: user.active,
            kind: user.kind,
            // Verified above; `authorize()` decides it again.
            portal_party: true,
            access_role,
            is_admin: menu_admin(
                user.is_admin,
                user.admin_from_idp,
                oidc.admin_role_held(&claims.claims),
            ),
        })
    }
}

/// The account's administrator flag as the menu may count it (ADR-0021
/// amendment 2026-11-24): a CLI-managed flag as it is; a flag the
/// identity provider's role granted only while the portal's verified
/// token still holds that role (no GitStation session re-checks it here).
fn menu_admin(is_admin: bool, admin_from_idp: bool, role_now: bool) -> bool {
    is_admin && (!admin_from_idp || role_now)
}

impl PortalAuthenticator for PortalWidgets {
    fn authenticate_portal<'a>(
        &'a self,
        _request_id: &'a RequestId,
        presented: &'a str,
    ) -> PortalAuthFuture<'a> {
        Box::pin(self.authenticate(presented))
    }
}

// ------------------------------------------------------------ service token

/// A token is renewed this long before it expires.
const RENEW_BEFORE: Duration = Duration::from_secs(30);
/// The shortest lifetime a token is cached for (Keycloak's minimum is far
/// above it).
const MIN_LIFETIME: Duration = Duration::from_secs(5);

/// `FG-INT-0024 PortalServiceTokenUnavailable` with `reason`.
fn unavailable(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::PortalServiceTokenUnavailable).with_context("reason", reason)
}

/// A provider failure (`FG-AUTH-0015` from the HTTP client) as the news'
/// `FG-INT-0024 reason=unreachable` with the transport's reason as
/// `detail`; anything else as it is.
fn transport(e: ForgeError) -> ForgeError {
    if e.code() != ErrorCode::IdentityProviderUnavailable {
        return e;
    }
    let detail = e
        .context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map_or_else(|| "unknown".to_owned(), |(_, v)| v.clone());
    unavailable("unreachable").with_context("detail", detail)
}

/// The token endpoint's answer to `client_credentials`: the token and its
/// lifetime, or the refusal.
pub(crate) fn token_reply(status: u16, body: &[u8]) -> Result<(String, Duration), ForgeError> {
    let doc: Option<Value> = serde_json::from_slice(body).ok();
    match status {
        200 => {}
        400 | 401 => {
            let oauth = doc
                .as_ref()
                .and_then(|b| b.get("error"))
                .and_then(Value::as_str)
                .map_or_else(|| "none".to_owned(), loggable);
            return Err(unavailable("client_rejected")
                .with_context("status", status)
                .with_context("oauth_error", oauth));
        }
        other => return Err(unavailable("status").with_context("status", other)),
    }
    let doc = doc.ok_or_else(|| unavailable("invalid_json").with_context("detail", "json"))?;
    let token = doc
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty() && t.len() <= 64 * 1024)
        .ok_or_else(|| unavailable("invalid_json").with_context("detail", "access_token"))?;
    let expires = doc
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|s| *s > 0)
        .ok_or_else(|| unavailable("invalid_json").with_context("detail", "expires_in"))?;
    Ok((token.to_owned(), Duration::from_secs(expires)))
}

/// How long a token of `lifetime` is used.
fn usable_for(lifetime: Duration) -> Duration {
    lifetime.saturating_sub(RENEW_BEFORE).max(MIN_LIFETIME)
}

/// The gitstation client's client-credentials tokens (one per process).
pub struct ServiceTokens {
    issuer: String,
    client_id: String,
    /// Never printed.
    client_secret: String,
    http: IdpClient,
    discovery: DiscoveryCache,
    fetch: Duration,
    token_timeout: Duration,
    cached: std::sync::Mutex<Option<(ServiceToken, Instant)>>,
    /// One fetch at a time (concurrent senders wait for it).
    fetching: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for ServiceTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceTokens")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl ServiceTokens {
    /// From the sign-in's settings (`auth.oidc.*`: the same client, secret
    /// and CA as the sign-in).
    pub fn new(settings: &OidcSettings) -> Result<Self, ForgeError> {
        Ok(Self {
            issuer: settings.issuer.clone(), // clone: the source owns its settings
            client_id: settings.client_id.clone(),
            client_secret: settings.client_secret.clone(),
            http: IdpClient::new(settings.ca_pem.as_deref(), settings.timeouts.connect)?,
            discovery: DiscoveryCache::default(),
            fetch: settings.timeouts.fetch,
            token_timeout: settings.timeouts.token,
            cached: std::sync::Mutex::new(None),
            fetching: tokio::sync::Mutex::new(()),
        })
    }

    fn cached(&self) -> Option<ServiceToken> {
        let held = self.cached.lock().ok()?;
        held.as_ref()
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(token, _)| token.clone()) // clone: the caller sends it
    }

    async fn fetch_token(&self) -> Result<ServiceToken, ForgeError> {
        let _turn = self.fetching.lock().await;
        if let Some(token) = self.cached() {
            return Ok(token);
        }
        let doc = self
            .discovery
            .get(&self.http, &self.issuer, self.fetch)
            .await
            .map_err(transport)?;
        let reply = self
            .http
            .post_form(
                "token",
                &doc.token_endpoint,
                (&self.client_id, &self.client_secret),
                &[("grant_type", "client_credentials")],
                self.token_timeout,
            )
            .await
            .map_err(transport)?;
        let (token, lifetime) = token_reply(reply.status, &reply.body)?;
        let token = ServiceToken::new(token);
        if let Ok(mut held) = self.cached.lock() {
            *held = Some((token.clone(), Instant::now() + usable_for(lifetime)));
            // clone: kept and returned
        }
        Ok(token)
    }
}

impl ServiceTokenSource for ServiceTokens {
    fn service_token(&self) -> ServiceTokenFuture<'_> {
        Box::pin(async move {
            if let Some(token) = self.cached() {
                return Ok(token);
            }
            self.fetch_token().await
        })
    }

    fn forget(&self) {
        if let Ok(mut held) = self.cached.lock() {
            *held = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_service_token_is_portal_token_unavailable() {
        let refused = token_reply(401, br#"{"error":"unauthorized_client"}"#).expect_err("401");
        assert_eq!(refused.code(), ErrorCode::PortalServiceTokenUnavailable);
        let text = refused.to_string();
        assert!(text.contains("reason=client_rejected"), "{text}");
        assert!(text.contains("oauth_error=unauthorized_client"), "{text}");
        for (status, body, want) in [
            (500, &b"oops"[..], "reason=status"),
            (200, &b"not json"[..], "detail=json"),
            (200, &br#"{"expires_in":300}"#[..], "detail=access_token"),
            (200, &br#"{"access_token":"t"}"#[..], "detail=expires_in"),
        ] {
            let err = token_reply(status, body).expect_err("refused");
            assert_eq!(err.code(), ErrorCode::PortalServiceTokenUnavailable);
            assert!(err.to_string().contains(want), "{status}: {err}");
        }
        let unreachable = transport(
            ForgeError::new(ErrorCode::IdentityProviderUnavailable)
                .with_context("call", "token")
                .with_context("reason", "timeout"),
        );
        assert_eq!(unreachable.code(), ErrorCode::PortalServiceTokenUnavailable);
        assert!(unreachable.to_string().contains("detail=timeout"));
    }

    #[test]
    fn a_token_is_kept_until_shortly_before_it_expires() {
        let (token, lifetime) =
            token_reply(200, br#"{"access_token":"eyJ.a.b","expires_in":300}"#).expect("ok");
        assert_eq!(token, "eyJ.a.b");
        assert_eq!(usable_for(lifetime), Duration::from_secs(270));
        assert_eq!(usable_for(Duration::from_secs(10)), MIN_LIFETIME);
    }

    #[test]
    fn a_verification_failure_becomes_the_portals_code() {
        let e = as_portal(
            ForgeError::new(ErrorCode::AccessTokenInvalid).with_context("reason", "expired"),
        );
        assert_eq!(e.code(), ErrorCode::PortalTokenInvalid);
        assert!(e.to_string().contains("reason=expired"));
        let other = as_portal(ForgeError::new(ErrorCode::IdentityProviderUnavailable));
        assert_eq!(other.code(), ErrorCode::IdentityProviderUnavailable);
    }
}

#[cfg(test)]
mod menu_admin_tests {
    use super::menu_admin;

    /// Review LOW: the menu's administrator link from a confirmed flag
    /// only — the CLI's as it is, the role's while the token holds it.
    #[test]
    fn an_idp_flag_needs_the_role_in_the_portal_token() {
        assert!(menu_admin(true, false, false), "a CLI administrator");
        assert!(menu_admin(true, true, true), "the role is still there");
        assert!(!menu_admin(true, true, false), "the role was removed");
        assert!(!menu_admin(false, false, true), "no flag, no link");
    }
}
