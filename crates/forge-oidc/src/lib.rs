//! forge-oidc: the browser sign-in through the OIDC identity provider
//! (Keycloak; M2 MR4, ADR-0021). A feature crate: `forge-web / forge-cli →
//! forge-oidc → forge-auth / forge-db → forge-core` (never another feature
//! crate).
//!
//! - [`Oidc::start`] (`GET /-/auth/login`): `state`, `nonce` and the PKCE
//!   verifier (OS CSPRNG) sealed with `next` into the `__Host-gstn_oidc`
//!   cookie ([`state`], key purpose `oidc-state`), and the authorization
//!   URL (code flow, S256, `response_mode=query`, `scope=openid`).
//! - [`Oidc::complete`] (`GET /-/auth/callback`): the state cookie, the
//!   token endpoint (`client_secret_basic`, 5 s), the ID token
//!   ([`jwt`]: forge's own ring `CryptoProvider` under `jsonwebtoken`,
//!   RS256 / ES256 only, the kty/alg pairing, `iss` / `aud` / `azp` /
//!   `exp` / `iat` / `nonce`, 60 s skew), the admission
//!   (`authorize(anonymous, SessionCreate, Admission)` inside forge-db's
//!   link-or-create change, ADR-0021 §4), the previous session revoked, and
//!   a new session whose IdP tokens are sealed with its public id.
//! - The identity provider is reached only through [`http`]: no
//!   environment proxy, no redirects, the configured CA only, 2 s connect,
//!   5 s per call, 1 MiB per body, the issuer's origin only
//!   ([`discovery`]); the JWKS is cached 10 minutes and refetched for an
//!   unknown `kid` at most once a minute ([`jwks`]).
//! - [`operator_link_identity`]: `forge admin user link-identity` (actor
//!   `system` only; ADR-0019 §8's exception list, ADR-0021 §4 2a).
//!
//! Failures: `FG-AUTH-0011` (state), `0012` (ID token), `0013`
//! (admission), `0014` (link), `0015` (identity provider), with the log
//! `reason`; never a token, a claim value or the IdP's response body.

mod discovery;
mod error;
mod flow;
mod http;
pub mod jwks;
pub mod jwt;
mod keys;
mod operator;
pub mod state;

#[cfg(feature = "test-support")]
pub mod test_support;

pub use discovery::Discovery;
pub use flow::{
    check_provider, CallbackInput, LoginDone, LoginStart, Oidc, OidcSettings, Timeouts,
};
pub use keys::{KeyEntry, KeySet, VerifyKey};
pub use operator::operator_link_identity;

/// The state cookie (ADR-0021 §3): HttpOnly, Secure, SameSite=Lax,
/// `Path=/`, no Domain, 10 minutes.
pub const STATE_COOKIE: &str = "__Host-gstn_oidc";
/// How long a started sign-in may take (the state cookie's `Max-Age`).
pub const STATE_MAX_AGE_SECS: u64 = 600;
/// The callback path below `web.base_url` (the fixed `redirect_uri`).
pub const CALLBACK_PATH: &str = "/-/auth/callback";
