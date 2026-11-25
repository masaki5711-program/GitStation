//! forge edge distribution (plan 21 E1, ADR-0041), pure: reads bytes and
//! values, returns values. No file, network, clock, DB or async runtime —
//! the caller (`gstn edge release verify`) reads the files. The 2026-11-25
//! re-scope (ADR-0041 Amendment, plan 21 E1b) retired the waves, the halt
//! decision and the install windows: devices and rollouts belong to the
//! user's own configuration tool, not to GitStation.
//!
//! - [`Manifest`] / [`parse_release`]: the signed release file
//!   (`{manifest, digest, key_id, signature}`) and its manifest's rules.
//! - [`manifest_digest`]: SHA-256 over a canonical, length-prefixed
//!   encoding — independent of the order payloads and blobs are listed in.
//! - [`verify_release`] / [`sign_manifest`]: Ed25519 over the digest in the
//!   edge manifest domain ([`EDGE_MANIFEST_DOMAIN`]), keys by `kid` from a
//!   [`KeyRing`] (the forge-agent signer, generalized with a domain, so an
//!   agent part signature never verifies as an edge manifest).
//! - [`lint_release_definition`]: a repository's `edge-release.toml`.
//!
//! Every finding is a [`Problem`]: a registered `FG-EDGE-*` code, a stable
//! `reason` and an optional subject (a field, a path, a kid — never file
//! content).

mod digest;
mod manifest;
mod release_def;
mod sign;

pub use digest::manifest_digest;
pub use forge_agent::{Digest, KeyRing, PublicKey};
pub use manifest::{
    parse_release, Arch, Blob, Manifest, Payload, PayloadKind, Release, MAX_BLOBS_PER_PAYLOAD,
    MAX_BLOB_SIZE, MAX_RELEASE_BYTES,
};
pub use release_def::{
    is_secret_like_path, lint_release_definition, PayloadDefinition, ReleaseDefinition,
    MAX_DEFINITION_BYTES,
};
pub use sign::{
    parse_edge_keys, sign_manifest, verify_release, VerifiedRelease, EDGE_MANIFEST_DOMAIN,
};

use forge_errors::ErrorCode;

/// One finding: what is wrong, why (a stable snake_case reason), and about
/// what (a field, a path, a kid — never file content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub code: ErrorCode,
    pub reason: &'static str,
    pub subject: Option<String>,
}

impl Problem {
    pub(crate) fn new(code: ErrorCode, reason: &'static str, subject: Option<String>) -> Self {
        Self {
            code,
            reason,
            subject,
        }
    }

    pub(crate) fn about(code: ErrorCode, reason: &'static str, subject: &str) -> Self {
        Self::new(code, reason, Some(subject.chars().take(256).collect()))
    }

    /// How serious the finding is when several are reported together:
    /// tampering before keys before form (a lower rank is more serious).
    pub fn rank(&self) -> usize {
        const ORDER: [ErrorCode; 4] = [
            ErrorCode::EdgeSignatureInvalid,
            ErrorCode::EdgeSigningKeyUnavailable,
            ErrorCode::EdgeManifestInvalid,
            ErrorCode::EdgeReleaseDefinitionInvalid,
        ];
        ORDER
            .iter()
            .position(|c| *c == self.code)
            .unwrap_or(ORDER.len())
    }
}

/// The most serious of `problems` ([`Problem::rank`]).
pub fn worst(problems: &[Problem]) -> Option<&Problem> {
    problems.iter().min_by_key(|p| p.rank())
}

/// 32 bytes from 64 lower-case hex digits; anything else is `None`.
pub(crate) fn unhex32(s: &str) -> Option<[u8; 32]> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = nibble(*bytes.get(2 * i)?)?;
        let lo = nibble(*bytes.get(2 * i + 1)?)?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests;
