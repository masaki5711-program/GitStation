//! The JWKS document → verification keys (ADR-0021 §9). Strict and
//! panic-free: every key is checked when the set is read, and a key that
//! fails keeps its `kid` with the reason, so a token naming it is
//! `key_invalid` (with `detail`), not `unknown_kid`.
//!
//! - `use` absent or `sig` (an `enc` key is never a signature key);
//! - `kty = RSA`: `n` and `e` base64url without padding; the modulus 2048
//!   ..= 8192 bits (ring's `RSA_PKCS1_2048_8192_SHA256`); `e` odd, ≥ 3, at
//!   most 33 bits;
//! - `kty = EC`: `crv = P-256`, `x` and `y` 32 bytes each, a point on the
//!   curve (checked with an ephemeral ECDH agreement, which validates the
//!   peer point);
//! - `alg`, when present, is kept and must equal the token's.

use std::collections::HashMap;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::Value;

/// A key's material after the checks.
#[derive(Clone, PartialEq, Eq)]
pub enum VerifyKey {
    /// RS256: big-endian modulus (no leading zero) and exponent.
    Rsa { n: Vec<u8>, e: Vec<u8> },
    /// ES256: the uncompressed point `04 || x || y`.
    P256 { point: Vec<u8> },
}

impl std::fmt::Debug for VerifyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rsa { n, .. } => write!(f, "Rsa({} bits)", n.len() * 8),
            Self::P256 { .. } => f.write_str("P256"),
        }
    }
}

impl VerifyKey {
    /// The JWK `kty` (for `key_alg_mismatch`).
    pub fn kty(&self) -> &'static str {
        match self {
            Self::Rsa { .. } => "RSA",
            Self::P256 { .. } => "EC",
        }
    }
}

/// One `kid` of the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEntry {
    /// The JWK `alg` when present.
    pub alg: Option<String>,
    /// The key, or why it cannot be used (`key_invalid`'s `detail`).
    pub key: Result<VerifyKey, &'static str>,
}

/// The keys of one JWKS document, by `kid`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeySet {
    keys: HashMap<String, KeyEntry>,
}

impl KeySet {
    /// Read a JWKS document. `Err` with a detail when the document itself
    /// is not a JWKS (`FG-AUTH-0015 invalid_json`); a broken key only marks
    /// its own `kid`. Keys without a `kid` are skipped (a token must name
    /// its key). A repeated `kid` keeps the first.
    pub fn parse(body: &[u8]) -> Result<Self, &'static str> {
        let doc: Value = serde_json::from_slice(body).map_err(|_| "json")?;
        let list = doc
            .get("keys")
            .and_then(Value::as_array)
            .ok_or("keys_missing")?;
        let mut keys = HashMap::new();
        for jwk in list {
            let Some(kid) = jwk.get("kid").and_then(Value::as_str) else {
                continue;
            };
            if kid.is_empty() || kid.len() > 256 || keys.contains_key(kid) {
                continue;
            }
            let alg = jwk.get("alg").and_then(Value::as_str).map(str::to_owned);
            keys.insert(
                kid.to_owned(),
                KeyEntry {
                    alg,
                    key: parse_key(jwk),
                },
            );
        }
        Ok(Self { keys })
    }

    pub fn get(&self, kid: &str) -> Option<&KeyEntry> {
        self.keys.get(kid)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// One JWK's material, or the `detail` of why it is unusable.
fn parse_key(jwk: &Value) -> Result<VerifyKey, &'static str> {
    if !matches!(jwk.get("use").map(Value::as_str), None | Some(Some("sig"))) {
        return Err("use");
    }
    let field = |name: &str| -> Result<Vec<u8>, &'static str> {
        let text = jwk.get(name).and_then(Value::as_str).ok_or("base64")?;
        URL_SAFE_NO_PAD.decode(text).map_err(|_| "base64")
    };
    match jwk.get("kty").and_then(Value::as_str) {
        Some("RSA") => {
            let n = strip_zeros(field("n")?);
            let e = strip_zeros(field("e")?);
            let bits = bit_len(&n);
            if !(2048..=8192).contains(&bits) {
                return Err("rsa_modulus_size");
            }
            let odd = e.last().is_some_and(|b| b & 1 == 1);
            let e_bits = bit_len(&e);
            if !odd || !(2..=33).contains(&e_bits) {
                return Err("rsa_exponent");
            }
            Ok(VerifyKey::Rsa { n, e })
        }
        Some("EC") => {
            if jwk.get("crv").and_then(Value::as_str) != Some("P-256") {
                return Err("crv");
            }
            let (x, y) = (field("x")?, field("y")?);
            if x.len() != 32 || y.len() != 32 {
                return Err("ec_coordinate_length");
            }
            let mut point = Vec::with_capacity(65);
            point.push(0x04);
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            on_curve(&point)?;
            Ok(VerifyKey::P256 { point })
        }
        _ => Err("kty"),
    }
}

fn strip_zeros(mut bytes: Vec<u8>) -> Vec<u8> {
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    bytes.drain(..zeros);
    bytes
}

/// Bits of a big-endian unsigned integer without leading zero bytes.
fn bit_len(bytes: &[u8]) -> usize {
    match bytes.first() {
        None => 0,
        Some(top) => (bytes.len() - 1) * 8 + (8 - top.leading_zeros() as usize),
    }
}

/// An ephemeral P-256 agreement with the point: ring validates the peer's
/// public key (on the curve, not the identity) before agreeing.
fn on_curve(point: &[u8]) -> Result<(), &'static str> {
    use ring::agreement::{agree_ephemeral, EphemeralPrivateKey, UnparsedPublicKey, ECDH_P256};
    let rng = ring::rand::SystemRandom::new();
    let mine = EphemeralPrivateKey::generate(&ECDH_P256, &rng).map_err(|_| "ec_point_unchecked")?;
    agree_ephemeral(mine, &UnparsedPublicKey::new(&ECDH_P256, point), |_| ())
        .map_err(|_| "ec_point")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_lengths_and_zero_stripping() {
        assert_eq!(bit_len(&[]), 0);
        assert_eq!(bit_len(&[1]), 1);
        assert_eq!(bit_len(&[0x80, 0]), 16);
        assert_eq!(strip_zeros(vec![0, 0, 3]), vec![3]);
    }

    #[test]
    fn a_document_that_is_not_a_jwks_is_refused_whole() {
        assert_eq!(KeySet::parse(b"not json"), Err("json"));
        assert_eq!(KeySet::parse(b"{}"), Err("keys_missing"));
        assert_eq!(KeySet::parse(br#"{"keys": {}}"#), Err("keys_missing"));
        let empty = KeySet::parse(br#"{"keys": [{"kty": "RSA"}]}"#).expect("parses");
        assert!(empty.is_empty(), "a key without kid is skipped");
    }
}
