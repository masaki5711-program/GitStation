//! Personal access token primitives (step D3, ADR-0019). Pure functions only:
//! no database, no clock. A token looks like `fgp_<public_id>_<secret>` where
//! `public_id` is 16 lowercase hex characters (8 random bytes, safe to log and
//! to look a row up by) and `secret` is 64 lowercase hex characters (256 bits
//! from the OS RNG). Only `sha256(secret)` is ever stored; it is compared in
//! constant time.
//!
//! The secret never reaches a log line: `TokenSecret` and `PlainToken` print
//! as `[redacted]` and `TokenSecret` is deliberately not `Serialize`:
//!
//! ```compile_fail
//! use forge_core::token::TokenSecret;
//! let secret = TokenSecret::from_bytes([0u8; 32]);
//! let _ = serde_json::to_string(&secret);
//! ```

use std::fmt;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Literal prefix of every token.
pub const TOKEN_PREFIX: &str = "fgp";
/// Length of the public id in hex characters.
pub const PUBLIC_ID_HEX_LEN: usize = 16;
/// Length of the secret in bytes (256 bits).
pub const SECRET_LEN: usize = 32;
/// Length of the public id in bytes.
const PUBLIC_ID_LEN: usize = PUBLIC_ID_HEX_LEN / 2;

/// Why a token string was not accepted. Carries no part of the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// The string is not `fgp_<16 hex>_<64 hex>` (lowercase).
    Malformed,
    /// The operating system could not provide random bytes.
    Rng,
}

/// The non-secret half of a token: identifies the row, safe to log.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublicId(String);

impl PublicId {
    /// The 16 lowercase hex characters.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The 256-bit secret half. Not `Serialize`, not `Display`, redacted `Debug`,
/// and no `==` (not constant-time): compare a secret only with [`verify`].
#[derive(Clone)]
pub struct TokenSecret([u8; SECRET_LEN]);

impl TokenSecret {
    pub fn from_bytes(bytes: [u8; SECRET_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }
}

impl fmt::Debug for TokenSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenSecret([redacted])")
    }
}

/// A full token as handed to its owner once, or as presented by a client.
#[derive(Clone)]
pub struct PlainToken {
    public_id: PublicId,
    secret: TokenSecret,
}

impl PlainToken {
    pub fn public_id(&self) -> &PublicId {
        &self.public_id
    }

    pub fn secret(&self) -> &TokenSecret {
        &self.secret
    }

    /// The `fgp_<public_id>_<secret>` string. The only way to see the secret
    /// in text form; call it only to show the token to its owner once.
    pub fn reveal(&self) -> String {
        format!(
            "{TOKEN_PREFIX}_{}_{}",
            self.public_id.0,
            to_hex(&self.secret.0)
        )
    }
}

impl fmt::Debug for PlainToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PlainToken({TOKEN_PREFIX}_[redacted])")
    }
}

/// A fresh token from the OS RNG.
pub fn generate() -> Result<PlainToken, TokenError> {
    let mut id = [0u8; PUBLIC_ID_LEN];
    let mut secret = [0u8; SECRET_LEN];
    getrandom::getrandom(&mut id).map_err(|_| TokenError::Rng)?;
    getrandom::getrandom(&mut secret).map_err(|_| TokenError::Rng)?;
    Ok(PlainToken {
        public_id: PublicId(to_hex(&id)),
        secret: TokenSecret(secret),
    })
}

/// Parse a presented token string. Never panics on any input.
pub fn parse(input: &str) -> Result<PlainToken, TokenError> {
    let rest = input
        .strip_prefix(TOKEN_PREFIX)
        .and_then(|r| r.strip_prefix('_'))
        .ok_or(TokenError::Malformed)?;
    let (id, secret) = rest.split_once('_').ok_or(TokenError::Malformed)?;
    if id.len() != PUBLIC_ID_HEX_LEN || !id.bytes().all(|b| nibble(b).is_some()) {
        return Err(TokenError::Malformed);
    }
    let secret = from_hex::<SECRET_LEN>(secret).ok_or(TokenError::Malformed)?;
    Ok(PlainToken {
        public_id: PublicId(id.to_owned()),
        secret: TokenSecret(secret),
    })
}

/// SHA-256 of the secret bytes: what the database stores.
pub fn sha256(secret: &TokenSecret) -> [u8; 32] {
    Sha256::digest(secret.0).into()
}

/// Whether `presented` hashes to `stored_hash`, in constant time.
pub fn verify(stored_hash: &[u8; 32], presented: &TokenSecret) -> bool {
    sha256(presented).ct_eq(stored_hash).into()
}

fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| [DIGITS[usize::from(b >> 4)], DIGITS[usize::from(b & 0x0f)]])
        .map(char::from)
        .collect()
}

/// Lowercase hex of exactly `N` bytes; `None` for any other input.
fn from_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (slot, pair) in out.iter_mut().zip(s.as_bytes().chunks_exact(2)) {
        let [hi, lo] = pair else { return None };
        *slot = (nibble(*hi)? << 4) | nibble(*lo)?;
    }
    Some(out)
}
