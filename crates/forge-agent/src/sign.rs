//! Ed25519 signatures over part digests (plan 16 §2.2, ADR-0032 §4).
//!
//! The signed message is [`SIGNATURE_DOMAIN`] ‖ the 32 digest bytes, so a
//! signature made for a part can never be replayed as a signature of
//! anything else the same key might sign later. Verification is ring's
//! `ED25519` (RFC 8032: a non-canonical `S` or a public key that does not
//! decode is refused). A key is found by `kid`; an unknown kid is
//! `FG-AGENT-0003` (the local keys are missing or stale) and a revoked one
//! is `FG-AGENT-0002` — never a fallback to "unsigned is fine".

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use forge_errors::ErrorCode;
use ring::signature::{Ed25519KeyPair, KeyPair as _, UnparsedPublicKey, ED25519};
use serde::Deserialize;

use crate::{Digest, Problem};

/// The prefix of every signed message.
pub const SIGNATURE_DOMAIN: &[u8] = b"forge-agent-part-signature-v1\0";

const MAX_KEYS: usize = 64;

/// One public key the server signs parts with (`GET /api/v1/agent-keys`,
/// kept locally by gstn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub kid: String,
    pub key: [u8; 32],
    /// A revoked key's signatures are refused (a retired key still
    /// verifies what it signed; it only signs nothing new).
    pub revoked: bool,
}

/// The keys a verifier trusts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyRing {
    pub keys: Vec<PublicKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRing {
    keys: Vec<RawKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKey {
    kid: String,
    public_key: String,
    // Shown by the server, not used to verify (a retired key still verifies).
    #[serde(default, rename = "created_at")]
    _created_at: Option<String>,
    #[serde(default, rename = "retired_at")]
    _retired_at: Option<String>,
    #[serde(default)]
    revoked_at: Option<String>,
}

fn key_problem(reason: &'static str, subject: Option<String>) -> Problem {
    Problem::new(ErrorCode::SigningKeyUnavailable, reason, subject)
}

impl KeyRing {
    /// Parse the key list JSON: `{"keys":[{"kid","public_key"(base64 of 32
    /// bytes),"created_at"?,"retired_at"?,"revoked_at"?}]}`.
    pub fn parse(bytes: &[u8]) -> Result<KeyRing, Problem> {
        let raw: RawRing = serde_json::from_slice(bytes)
            .map_err(|e| key_problem("keys_invalid", Some(format!("line {}", e.line()))))?;
        if raw.keys.len() > MAX_KEYS {
            return Err(key_problem(
                "keys_invalid",
                Some("too_many_keys".to_owned()),
            ));
        }
        let mut keys = Vec::with_capacity(raw.keys.len());
        for k in raw.keys {
            let duplicate = keys.iter().any(|p: &PublicKey| p.kid == k.kid);
            if k.kid.is_empty() || k.kid.len() > 64 || duplicate {
                return Err(key_problem("keys_invalid", Some("kid".to_owned())));
            }
            let key: [u8; 32] = STANDARD
                .decode(k.public_key.as_bytes())
                .ok()
                .and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok())
                .ok_or_else(|| key_problem("keys_invalid", Some(k.kid.clone())))?;
            keys.push(PublicKey {
                kid: k.kid,
                key,
                revoked: k.revoked_at.is_some(),
            });
        }
        Ok(KeyRing { keys })
    }

    pub fn get(&self, kid: &str) -> Option<&PublicKey> {
        self.keys.iter().find(|k| k.kid == kid)
    }
}

fn message(digest: &Digest) -> Vec<u8> {
    let mut m = Vec::with_capacity(SIGNATURE_DOMAIN.len() + 32);
    m.extend_from_slice(SIGNATURE_DOMAIN);
    m.extend_from_slice(&digest.0);
    m
}

/// Verify `signature` (base64 of 64 bytes) of `digest` by key `kid`.
pub fn verify_signature(
    ring_: &KeyRing,
    kid: &str,
    digest: &Digest,
    signature: &str,
) -> Result<(), Problem> {
    let Some(key) = ring_.get(kid) else {
        return Err(key_problem(
            "unknown_key",
            Some(kid.chars().take(64).collect()),
        ));
    };
    if key.revoked {
        return Err(Problem::about(
            ErrorCode::PartSignatureInvalid,
            "key_revoked",
            &key.kid,
        ));
    }
    let sig = STANDARD
        .decode(signature.as_bytes())
        .ok()
        .filter(|s| s.len() == 64)
        .ok_or_else(|| {
            Problem::about(
                ErrorCode::PartSignatureInvalid,
                "signature_malformed",
                &key.kid,
            )
        })?;
    UnparsedPublicKey::new(&ED25519, key.key)
        .verify(&message(digest), &sig)
        .map_err(|_| Problem::about(ErrorCode::PartSignatureInvalid, "bad_signature", &key.kid))
}

/// Sign `digest` with the Ed25519 key whose 32-byte seed is `seed`; base64
/// of the 64-byte signature. For the publisher (forge-marketplace, A5) and
/// tests; the seed never leaves the caller.
pub fn sign_digest(seed: &[u8; 32], digest: &Digest) -> Result<String, Problem> {
    let pair = Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| key_problem("signing_key_invalid", None))?;
    Ok(STANDARD.encode(pair.sign(&message(digest)).as_ref()))
}

/// The public key (base64 of 32 bytes) of the seed.
pub fn public_key_from_seed(seed: &[u8; 32]) -> Result<String, Problem> {
    let pair = Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| key_problem("signing_key_invalid", None))?;
    Ok(STANDARD.encode(pair.public_key().as_ref()))
}
