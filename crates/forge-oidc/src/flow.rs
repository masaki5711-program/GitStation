//! The sign-in itself (ADR-0021 §3): [`Oidc::start`] and
//! [`Oidc::complete`]. See the crate documentation for the steps.

use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forge_auth::{authorize, Action, AdmissionAccount, Principal, Resource};
use forge_core::config::{OidcConfig, OidcSigningAlg};
use forge_core::session_token::SessionToken;
use forge_core::web_keys::{KeyPurpose, PurposeKeys, WebKeys};
use forge_core::{RequestId, SessionRevokeReason, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::identities::{self, LinkPolicy, LoginAccount, LoginIdentity, LoginOutcome};
use forge_db::sessions::{self, NewSession, OidcSession};
use forge_db::Db;
use forge_errors::ForgeError;
use serde_json::Value;

use crate::discovery::{self, Discovery, DiscoveryCache};
use crate::error::{loggable, state, unavailable};
use crate::http::IdpClient;
use crate::jwks::{JwksCache, JwksTiming};
use crate::jwt::{Expected, IdClaims, JwtVerifier};
use crate::state::{safe_next, LoginState};
use crate::{CALLBACK_PATH, STATE_MAX_AGE_SECS};

/// The bounds of every call to the identity provider (ADR-0021 §11). The
/// defaults are the ADR's; tests shorten them.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Connecting (every call): 2 s.
    pub connect: Duration,
    /// Discovery and JWKS, send and body: 5 s.
    pub fetch: Duration,
    /// The token endpoint at the callback: 5 s.
    pub token: Duration,
    /// The JWKS cache.
    pub jwks: JwksTiming,
    /// The waits between the start's discovery attempts (1 s, 2 s).
    pub prefetch_backoff: [Duration; 2],
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(2),
            fetch: Duration::from_secs(5),
            token: Duration::from_secs(5),
            jwks: JwksTiming::default(),
            prefetch_backoff: [Duration::from_secs(1), Duration::from_secs(2)],
        }
    }
}

/// Everything the sign-in needs from the configuration.
pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    /// Never printed.
    pub client_secret: String,
    /// The content of `auth.oidc.ca_file`.
    pub ca_pem: Option<Vec<u8>>,
    pub employee_number_claim: String,
    pub access_role: String,
    pub signing_algs: Vec<OidcSigningAlg>,
    pub auto_link: bool,
    /// `web.base_url`'s origin (`redirect_uri` = it + `/-/auth/callback`).
    pub base_origin: String,
    /// `web.session_idle_secs` / `web.session_absolute_secs`.
    pub session_idle: Duration,
    pub session_absolute: Duration,
    pub timeouts: Timeouts,
}

impl std::fmt::Debug for OidcSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcSettings")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[redacted]")
            .field("ca_pem", &self.ca_pem.is_some())
            .field("signing_algs", &self.signing_algs)
            .field("auto_link", &self.auto_link)
            .field("base_origin", &self.base_origin)
            .finish_non_exhaustive()
    }
}

impl OidcSettings {
    /// From `auth.oidc.*` (reading `auth.oidc.ca_file` now: a missing or
    /// unreadable file stops the start, `FG-SYS-0002`) and `web.*`.
    pub fn from_config(
        cfg: &OidcConfig,
        base_origin: &str,
        session_idle: Duration,
        session_absolute: Duration,
    ) -> Result<Self, ForgeError> {
        let ca_pem = cfg
            .ca_file
            .as_deref()
            .map(|path| {
                std::fs::read(path).map_err(|e| {
                    ForgeError::new(forge_errors::ErrorCode::ConfigInvalid)
                        .with_context("config_key", "auth.oidc.ca_file")
                        .with_context("reason", "unreadable")
                        .with_context("path", path.display())
                        .with_source(e)
                })
            })
            .transpose()?;
        Ok(Self {
            issuer: cfg.issuer.clone(), // clone: the settings own their copy
            client_id: cfg.client_id.clone(),
            client_secret: cfg.client_secret.expose().to_owned(),
            ca_pem,
            employee_number_claim: cfg.employee_number_claim.clone(),
            access_role: cfg.access_role.clone(),
            signing_algs: cfg.signing_algs.clone(),
            auto_link: cfg.auto_link,
            base_origin: base_origin.to_owned(),
            session_idle,
            session_absolute,
            timeouts: Timeouts::default(),
        })
    }
}

/// What `GET /-/auth/login` answers: 302 to `location`, setting the state
/// cookie to `state_cookie`.
#[derive(Debug)]
pub struct LoginStart {
    pub location: String,
    pub state_cookie: String,
    /// The requested `next` was refused (log `reason=next_rejected`).
    pub next_rejected: bool,
}

/// The callback request's parts.
#[derive(Debug, Clone, Copy)]
pub struct CallbackInput<'a> {
    pub request_id: &'a RequestId,
    pub code: Option<&'a str>,
    pub state: Option<&'a str>,
    /// The provider's `error=` (only the code is used).
    pub error: Option<&'a str>,
    pub state_cookie: Option<&'a str>,
    /// The browser's current `__Host-gstn_session`, if any (revoked).
    pub session_cookie: Option<&'a str>,
    pub source_ip: Option<IpAddr>,
    pub user_agent: Option<&'a str>,
}

/// A finished sign-in: set `token` as the session cookie (Max-Age =
/// `max_age`) and 302 to `next`.
#[derive(Debug)]
pub struct LoginDone {
    pub token: SessionToken,
    pub max_age: Duration,
    pub next: String,
    pub user: UserId,
    pub public_id: String,
    /// `existing`, `linked` or `created`.
    pub outcome: &'static str,
}

/// The sign-in service (one per process).
pub struct Oidc {
    settings: OidcSettings,
    redirect_uri: String,
    http: IdpClient,
    discovery: DiscoveryCache,
    jwks: JwksCache,
    verifier: JwtVerifier,
    state_keys: PurposeKeys,
    refresh_keys: PurposeKeys,
    id_keys: PurposeKeys,
    db: Db,
}

impl std::fmt::Debug for Oidc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Oidc")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// Seconds since the epoch.
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Oidc {
    /// Build the service: the HTTP client, the JWT verifier (installs the
    /// provider) and the three purpose keys of `web.session_key_file`.
    pub fn new(settings: OidcSettings, keys: &WebKeys, db: Db) -> Result<Self, ForgeError> {
        let http = IdpClient::new(settings.ca_pem.as_deref(), settings.timeouts.connect)?;
        let verifier = JwtVerifier::new(&settings.signing_algs)?;
        let redirect_uri = format!("{}{CALLBACK_PATH}", settings.base_origin);
        Ok(Self {
            redirect_uri,
            http,
            discovery: DiscoveryCache::default(),
            jwks: JwksCache::new(settings.timeouts.jwks),
            verifier,
            state_keys: keys.purpose(KeyPurpose::OidcState)?,
            refresh_keys: keys.purpose(KeyPurpose::RefreshToken)?,
            id_keys: keys.purpose(KeyPurpose::IdToken)?,
            db,
            settings,
        })
    }

    /// The configured issuer.
    pub fn issuer(&self) -> &str {
        &self.settings.issuer
    }

    /// The `redirect_uri` (`web.base_url` + `/-/auth/callback`).
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// At start (`forge serve`, in the background): fetch the discovery
    /// document, three attempts with waits of 1 s and 2 s. A failure is
    /// logged (warn, FG-AUTH-0015) and the server keeps running: the first
    /// sign-in tries once more (ADR-0021 §11).
    pub async fn prefetch(&self) {
        let waits = self.settings.timeouts.prefetch_backoff;
        for attempt in 0..=waits.len() {
            match self
                .discovery
                .get(
                    &self.http,
                    &self.settings.issuer,
                    self.settings.timeouts.fetch,
                )
                .await
            {
                Ok(_) => return,
                Err(e) => {
                    let context: Vec<String> = e
                        .context()
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    tracing::warn!(
                        target: "forge_oidc",
                        error_code = e.code().id(),
                        attempt = attempt + 1,
                        context = %context.join(" "),
                        "forge.auth.discovery_failed"
                    );
                }
            }
            if let Some(wait) = waits.get(attempt) {
                tokio::time::sleep(*wait).await;
            }
        }
    }

    /// One fresh check of the provider for `forge doctor`: discovery and
    /// the JWKS (never cached).
    pub async fn check(&self) -> Result<(Discovery, usize), ForgeError> {
        let doc = discovery::fetch(
            &self.http,
            &self.settings.issuer,
            self.settings.timeouts.fetch,
        )
        .await?;
        let keys =
            crate::jwks::fetch(&self.http, &doc.jwks_uri, self.settings.timeouts.fetch).await?;
        Ok((doc, keys.len()))
    }

    async fn discovered(&self) -> Result<std::sync::Arc<Discovery>, ForgeError> {
        self.discovery
            .get(
                &self.http,
                &self.settings.issuer,
                self.settings.timeouts.fetch,
            )
            .await
    }

    /// `GET /-/auth/login?next=`: a new login state and where to send the
    /// browser (ADR-0021 §3 1).
    pub async fn start(&self, next: Option<&str>) -> Result<LoginStart, ForgeError> {
        let doc = self.discovered().await?;
        let (next, next_rejected) = safe_next(next);
        let login = LoginState::generate(next, now_secs())?;
        let location = with_query(
            &doc.authorization_endpoint,
            &[
                ("response_type", "code"),
                ("client_id", &self.settings.client_id),
                ("redirect_uri", &self.redirect_uri),
                ("scope", "openid"),
                ("response_mode", "query"),
                ("state", &login.state),
                ("nonce", &login.nonce),
                ("code_challenge", &login.challenge()),
                ("code_challenge_method", "S256"),
            ],
        )?;
        Ok(LoginStart {
            location,
            state_cookie: login.seal(&self.state_keys)?,
            next_rejected,
        })
    }

    /// `GET /-/auth/callback` (ADR-0021 §3 2): see the crate documentation.
    pub async fn complete(&self, input: CallbackInput<'_>) -> Result<LoginDone, ForgeError> {
        let login = self.check_state(&input)?;
        let code = input.code.ok_or_else(|| state("query_missing"))?;
        let doc = self.discovered().await?;
        let tokens = self.redeem(&doc, code, &login.verifier).await?;
        let claims = self.verify(&doc, &tokens.id_token, &login.nonce).await?;
        let access_role = self.has_access_role(&claims.claims);
        let identity = self.identity(&claims);
        let mut ctx = ChangeContext::new(Actor::Anonymous, input.request_id.clone());
        ctx.source_ip = input.source_ip;
        ctx.user_agent = input.user_agent.map(|u| u.chars().take(512).collect());
        let outcome = identities::login_decided(
            &self.db,
            &ctx,
            &identity,
            LinkPolicy {
                auto_link: self.settings.auto_link,
            },
            |account| {
                let account = match account {
                    LoginAccount::New => AdmissionAccount::New,
                    LoginAccount::Active => AdmissionAccount::Active,
                    LoginAccount::Disabled => AdmissionAccount::Disabled,
                    LoginAccount::NotHuman => AdmissionAccount::NotHuman,
                };
                authorize(
                    &Principal::anonymous(),
                    Action::SessionCreate,
                    &Resource::Admission {
                        access_role,
                        account,
                    },
                )
            },
        )
        .await?;
        let user = outcome.user();
        if matches!(outcome, LoginOutcome::Existing { .. }) {
            if let Err(e) =
                identities::record_login(&self.db, &self.settings.issuer, &claims.subject).await
            {
                // Unaudited bookkeeping: logged once here and the sign-in goes on.
                forge_core::telemetry::log_failure(&e, input.request_id);
            }
        }
        ctx.actor = Actor::User(user);
        if let Some(previous) = input.session_cookie {
            sessions::revoke_presented(&self.db, &ctx, previous, SessionRevokeReason::Logout)
                .await?;
        }
        let token = sessions::new_token()?;
        let aad = token.public_id().as_str().as_bytes();
        let refresh_token_enc = tokens
            .refresh_token
            .as_deref()
            .map(|t| self.refresh_keys.seal(aad, t.as_bytes()))
            .transpose()?;
        let id_token_enc = Some(self.id_keys.seal(aad, tokens.id_token.as_bytes())?);
        let new = NewSession {
            user,
            idle: self.settings.session_idle,
            absolute: self.settings.session_absolute,
            oidc: Some(OidcSession {
                issuer: self.settings.issuer.clone(), // clone: forge-db owns its request
                subject: claims.subject.clone(),
                sid: claims.sid.clone(),
                refresh_token_enc,
                id_token_enc,
            }),
        };
        let issued = sessions::issue_with_token(&self.db, &ctx, &new, token).await?;
        let public_id = issued.token.public_id().as_str().to_owned();
        tracing::info!(
            target: "forge_oidc",
            request_id = %input.request_id,
            user_id = user.get(),
            outcome = outcome.as_str(),
            public_id = %public_id,
            "forge.auth.login_succeeded"
        );
        Ok(LoginDone {
            token: issued.token,
            max_age: self.settings.session_absolute,
            next: login.next,
            user,
            public_id,
            outcome: outcome.as_str(),
        })
    }

    /// The state cookie and the query's `state` / `error`.
    fn check_state(&self, input: &CallbackInput<'_>) -> Result<LoginState, ForgeError> {
        let cookie = input
            .state_cookie
            .filter(|c| !c.is_empty())
            .ok_or_else(|| state("state_cookie_missing"))?;
        let login = LoginState::open(cookie, &self.state_keys, now_secs(), STATE_MAX_AGE_SECS)?;
        if let Some(error) = input.error {
            return Err(state("idp_error").with_context("idp_error", loggable(error)));
        }
        let presented = input.state.ok_or_else(|| state("query_missing"))?;
        if !login.matches(presented) {
            return Err(state("state_mismatch"));
        }
        Ok(login)
    }

    /// The token endpoint: the ID token (required) and the refresh token.
    async fn redeem(
        &self,
        doc: &Discovery,
        code: &str,
        verifier: &str,
    ) -> Result<Tokens, ForgeError> {
        let reply = self
            .http
            .post_form(
                "token",
                &doc.token_endpoint,
                (&self.settings.client_id, &self.settings.client_secret),
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", &self.redirect_uri),
                    ("code_verifier", verifier),
                ],
                self.settings.timeouts.token,
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
                return Err(state("code_rejected")
                    .with_context("status", reply.status)
                    .with_context("oauth_error", oauth));
            }
            other => return Err(unavailable("token", "status").with_context("status", other)),
        }
        let body = body
            .ok_or_else(|| unavailable("token", "invalid_json").with_context("detail", "json"))?;
        let id_token = body
            .get("id_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| unavailable("token", "invalid_json").with_context("detail", "id_token"))?
            .to_owned();
        let refresh_token = body
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        Ok(Tokens {
            id_token,
            refresh_token,
        })
    }

    async fn verify(
        &self,
        doc: &Discovery,
        token: &str,
        nonce: &str,
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
            nonce: Some(nonce),
            now: now_secs(),
            leeway: 60,
        };
        self.verifier.verify_id_token(token, &keys, &expected)
    }

    /// `resource_access.<client_id>.roles` holds `auth.oidc.access_role`.
    fn has_access_role(&self, claims: &Value) -> bool {
        claims
            .get("resource_access")
            .and_then(|r| r.get(&self.settings.client_id))
            .and_then(|c| c.get("roles"))
            .and_then(Value::as_array)
            .is_some_and(|roles| {
                roles
                    .iter()
                    .any(|r| r.as_str() == Some(self.settings.access_role.as_str()))
            })
    }

    fn identity<'a>(&'a self, claims: &'a IdClaims) -> LoginIdentity<'a> {
        let text = |name: &str| claims.claims.get(name).and_then(Value::as_str);
        LoginIdentity {
            issuer: &self.settings.issuer,
            subject: &claims.subject,
            employee_number: text(&self.settings.employee_number_claim),
            email: text("email"),
            email_verified: claims.claims.get("email_verified").and_then(Value::as_bool)
                == Some(true),
            preferred_username: text("preferred_username"),
            name: text("name"),
        }
    }
}

/// `endpoint` with `pairs` appended to its query (a sync helper: the
/// query serializer is not `Send`).
fn with_query(endpoint: &str, pairs: &[(&str, &str)]) -> Result<String, ForgeError> {
    let mut url = url::Url::parse(endpoint).map_err(|_| {
        unavailable("discovery", "invalid_json").with_context("detail", "authorization_endpoint")
    })?;
    url.query_pairs_mut().extend_pairs(pairs);
    Ok(url.into())
}

struct Tokens {
    id_token: String,
    refresh_token: Option<String>,
}
