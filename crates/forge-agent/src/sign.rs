//! Ed25519 signatures over digests (plan 16 §2.2, ADR-0032 §4; generalized
//! by plan 21 E1, ADR-0041 §3).
//!
//! The signed message is a [`SignatureDomain`] ‖ the 32 digest bytes, so a
//! signature made in one domain (an agent part) can never be replayed as a
//! signature in another (an edge release manifest), even if both were made
//! with the same key. [`verify_signature`] / [`sign_digest`] keep the agent
//! part domain [`SIGNATURE_DOMAIN`]; [`verify_signature_in`] /
//! [`sign_digest_in`] take the domain. Verification is ring's
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

/// The prefix of every agent part signature's message (unchanged since A1,
/// so every existing part signature still verifies).
pub const SIGNATURE_DOMAIN: &[u8] = b"forge-agent-part-signature-v1\0";

/// What a signature is for: the prefix of the signed message. Each signed
/// kind of thing has its own constant, ending in a NUL so no domain's
/// prefix runs into another's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureDomain(&'static [u8]);

impl SignatureDomain {
    /// Agent parts (plan 16, ADR-0032 §4): [`SIGNATURE_DOMAIN`].
    pub const AGENT_PART: SignatureDomain = SignatureDomain(SIGNATURE_DOMAIN);

    /// A domain for another signed kind (its crate owns the constant, e.g.
    /// forge-edge's manifest domain). `prefix` must be a fixed literal that
    /// names the kind and a version and ends in `\0`.
    pub const fn new(prefix: &'static [u8]) -> SignatureDomain {
        SignatureDomain(prefix)
    }

    pub fn as_bytes(&self) -> &'static [u8] {
        self.0
    }
}

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

fn message(domain: SignatureDomain, digest: &Digest) -> Vec<u8> {
    let mut m = Vec::with_capacity(domain.0.len() + 32);
    m.extend_from_slice(domain.0);
    m.extend_from_slice(&digest.0);
    m
}

/// Verify `signature` (base64 of 64 bytes) of an agent part `digest` by key
/// `kid` ([`SignatureDomain::AGENT_PART`]).
pub fn verify_signature(
    ring_: &KeyRing,
    kid: &str,
    digest: &Digest,
    signature: &str,
) -> Result<(), Problem> {
    verify_signature_in(SignatureDomain::AGENT_PART, ring_, kid, digest, signature)
}

/// Verify `signature` (base64 of 64 bytes) of `digest` in `domain` by key
/// `kid`. The problems carry the agent codes (`FG-AGENT-0002` /
/// `FG-AGENT-0003`) and a stable `reason`; a crate verifying another domain
/// maps them to its own codes by `reason`.
pub fn verify_signature_in(
    domain: SignatureDomain,
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
        .verify(&message(domain, digest), &sig)
        .map_err(|_| Problem::about(ErrorCode::PartSignatureInvalid, "bad_signature", &key.kid))
}

/// Sign an agent part `digest` with the Ed25519 key whose 32-byte seed is
/// `seed`; base64 of the 64-byte signature. For the publisher
/// (forge-marketplace, A5) and tests; the seed never leaves the caller.
// The signed kind's own wrapper of the domain signer (clippy.toml).
#[allow(clippy::disallowed_methods)]
pub fn sign_digest(seed: &[u8; 32], digest: &Digest) -> Result<String, Problem> {
    sign_digest_in(SignatureDomain::AGENT_PART, seed, digest)
}

/// Sign `digest` in `domain` (base64 of the 64-byte signature). Each caller
/// is restricted by `clippy.toml` (`disallowed-methods`) to its one
/// signing step.
pub fn sign_digest_in(
    domain: SignatureDomain,
    seed: &[u8; 32],
    digest: &Digest,
) -> Result<String, Problem> {
    let pair = Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| key_problem("signing_key_invalid", None))?;
    Ok(STANDARD.encode(pair.sign(&message(domain, digest)).as_ref()))
}

/// The public key (base64 of 32 bytes) of the seed.
pub fn public_key_from_seed(seed: &[u8; 32]) -> Result<String, Problem> {
    let pair = Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| key_problem("signing_key_invalid", None))?;
    Ok(STANDARD.encode(pair.public_key().as_ref()))
}
