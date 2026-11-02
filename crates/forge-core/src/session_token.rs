//! The web session cookie (M2 MR3, ADR-0021 §5–§6). Pure functions only.
//!
//! The cookie `__Host-gstn_session` holds `gss_<public_id>_<secret>`: the
//! same parts as a personal access token ([`crate::token`]: 16 hex of
//! public id, 64 hex of secret from the OS RNG, only `sha256(secret)`
//! stored, compared in constant time); the prefix `gss_` lets the secret
//! scanners find it.
//!
//! The CSRF token of a session is `hex(HMAC-SHA256(secret, "gitstation-csrf-v1"))`
//! (ADR-0021 §6): computed from the cookie's secret on every request, never
//! stored, compared in constant time. It can be shown to the page
//! (`GET /api/v1/session`) without revealing the secret.
//!
//! ```compile_fail
//! use forge_core::session_token::SessionToken;
//! let token: SessionToken = todo!();
//! let _ = serde_json::to_string(&token);
//! ```

use std::fmt;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::token::{self, PublicId, TokenError, TokenSecret};

/// Literal prefix of every session cookie value.
pub const SESSION_PREFIX: &str = "gss";
/// The session cookie's name (`__Host-`: `Secure`, `Path=/`, no `Domain`).
pub const SESSION_COOKIE: &str = "__Host-gstn_session";
/// The request header that carries the CSRF token (lowercase, as `http`
/// header names are compared).
pub const CSRF_HEADER: &str = "x-gitstation-csrf";
/// The HMAC message of the CSRF token (versioned: a new scheme gets `v2`).
const CSRF_MESSAGE: &[u8] = b"gitstation-csrf-v1";

/// A session cookie value as issued once or as presented by a browser.
/// Redacted `Debug`, no `Display`, not `Serialize`.
#[derive(Clone)]
pub struct SessionToken {
    public_id: PublicId,
    secret: TokenSecret,
}

impl SessionToken {
    pub fn public_id(&self) -> &PublicId {
        &self.public_id
    }

    pub fn secret(&self) -> &TokenSecret {
        &self.secret
    }

    /// The `gss_<public_id>_<secret>` cookie value. Call it only to set the
    /// cookie.
    pub fn reveal(&self) -> String {
        format!(
            "{SESSION_PREFIX}_{}_{}",
            self.public_id.as_str(),
            token::to_hex(self.secret.as_bytes())
        )
    }

    /// This session's CSRF token ([`csrf_token`]).
    pub fn csrf_token(&self) -> Result<String, TokenError> {
        csrf_token(&self.secret)
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionToken({SESSION_PREFIX}_[redacted])")
    }
}

/// A fresh session cookie value from the OS RNG.
pub fn generate() -> Result<SessionToken, TokenError> {
    let (public_id, secret) = token::random_parts()?;
    Ok(SessionToken { public_id, secret })
}

/// Parse a presented cookie value. Never panics on any input; a personal
/// access token (`fgp_…`) is not a session.
pub fn parse(input: &str) -> Result<SessionToken, TokenError> {
    let (public_id, secret) = token::parse_prefixed(input, SESSION_PREFIX)?;
    Ok(SessionToken { public_id, secret })
}

fn csrf_mac(secret: &TokenSecret) -> Result<[u8; 32], TokenError> {
    // HMAC accepts a key of any length, so this never fails in practice;
    // a failure is still reported, never answered with a fixed value.
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).map_err(|_| TokenError::Mac)?;
    mac.update(CSRF_MESSAGE);
    Ok(mac.finalize().into_bytes().into())
}

/// `hex(HMAC-SHA256(secret, "gitstation-csrf-v1"))`, 64 lowercase hex.
pub fn csrf_token(secret: &TokenSecret) -> Result<String, TokenError> {
    csrf_mac(secret).map(|mac| token::to_hex(&mac))
}

/// Whether `presented` is this session's CSRF token, in constant time
/// (a value of another length is `false` without comparing).
pub fn csrf_matches(secret: &TokenSecret, presented: &str) -> Result<bool, TokenError> {
    let expected = csrf_token(secret)?;
    Ok(expected.as_bytes().ct_eq(presented.as_bytes()).into())
}
