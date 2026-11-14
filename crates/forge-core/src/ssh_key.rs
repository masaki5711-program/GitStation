//! SSH public keys of people (M4 C13, ADR-0033 §2): the one line a user
//! pastes (`<algorithm> <base64 blob> [comment]`, OpenSSH's
//! `authorized_keys` / `.pub` form), parsed strictly, and its SHA-256
//! fingerprint (`SHA256:<base64 without padding>`, what `ssh-keygen -l`
//! prints and what the SSH server computes from the key a client proves).
//!
//! Accepted: Ed25519 and ECDSA on P-256 / P-384 / P-521. Refused with
//! `FG-AUTH-0023 reason=algorithm_unsupported`: RSA (the server is built
//! without the `rsa` crate, RUSTSEC-2023-0071), DSA, security-key
//! (`sk-…`) and certificate (`…-cert-v01@openssh.com`) types. Pure: no I/O.

use base64::Engine as _;
use forge_errors::{ErrorCode, ForgeError};
use sha2::{Digest as _, Sha256};

/// The longest line accepted (a P-521 key is ~ 270 bytes; options or a
/// certificate never belong here).
pub const MAX_LINE_BYTES: usize = 8 * 1024;
/// The longest comment kept (a longer one is cut at a character boundary).
pub const MAX_COMMENT_CHARS: usize = 100;

/// The accepted key algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SshKeyAlgorithm {
    Ed25519,
    EcdsaP256,
    EcdsaP384,
    EcdsaP521,
}

impl SshKeyAlgorithm {
    /// Every accepted algorithm.
    pub const ALL: [SshKeyAlgorithm; 4] = [
        SshKeyAlgorithm::Ed25519,
        SshKeyAlgorithm::EcdsaP256,
        SshKeyAlgorithm::EcdsaP384,
        SshKeyAlgorithm::EcdsaP521,
    ];

    /// The SSH name (the first word of the line, the blob's first string).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ed25519 => "ssh-ed25519",
            Self::EcdsaP256 => "ecdsa-sha2-nistp256",
            Self::EcdsaP384 => "ecdsa-sha2-nistp384",
            Self::EcdsaP521 => "ecdsa-sha2-nistp521",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.as_str() == name)
    }

    /// The curve name inside an ECDSA blob; `None` for Ed25519.
    fn curve(self) -> Option<&'static str> {
        match self {
            Self::Ed25519 => None,
            Self::EcdsaP256 => Some("nistp256"),
            Self::EcdsaP384 => Some("nistp384"),
            Self::EcdsaP521 => Some("nistp521"),
        }
    }

    /// The length of the key itself: Ed25519's 32 bytes, an ECDSA
    /// uncompressed point (`0x04 || X || Y`).
    fn key_len(self) -> usize {
        match self {
            Self::Ed25519 => 32,
            Self::EcdsaP256 => 65,
            Self::EcdsaP384 => 97,
            Self::EcdsaP521 => 133,
        }
    }
}

/// A parsed public key: what is stored and shown (never a private key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshPublicKey {
    pub algorithm: SshKeyAlgorithm,
    /// The wire-format blob (RFC 4253 §6.6), base64-decoded.
    pub blob: Vec<u8>,
    /// The line's comment (trimmed, at most [`MAX_COMMENT_CHARS`]); empty
    /// when there is none.
    pub comment: String,
}

impl SshPublicKey {
    /// `SHA256:<base64 of SHA-256(blob), no padding>` — the identity of the
    /// key (unique across all users).
    pub fn fingerprint(&self) -> String {
        fingerprint_of_blob(&self.blob)
    }

    /// The canonical line `<algorithm> <base64>` (no comment).
    pub fn openssh(&self) -> String {
        format!(
            "{} {}",
            self.algorithm.as_str(),
            base64::engine::general_purpose::STANDARD.encode(&self.blob)
        )
    }
}

/// The SHA-256 fingerprint of a wire-format key blob, as `ssh-keygen -l
/// -E sha256` prints it.
pub fn fingerprint_of_blob(blob: &[u8]) -> String {
    let digest = Sha256::digest(blob);
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
    )
}

/// Whether `s` has the shape of a fingerprint this module produces.
pub fn is_fingerprint(s: &str) -> bool {
    s.strip_prefix("SHA256:").is_some_and(|b64| {
        b64.len() == 43
            && b64
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    })
}

fn invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::SshKeyInvalid).with_context("reason", reason)
}

fn malformed(detail: &'static str) -> ForgeError {
    invalid("malformed").with_context("detail", detail)
}

/// Parse one pasted public key line. Errors: `FG-AUTH-0023` with
/// `reason` = `too_long`, `malformed` (`detail`) or
/// `algorithm_unsupported` (`algorithm`, at most 64 characters).
pub fn parse_public_key(line: &str) -> Result<SshPublicKey, ForgeError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(invalid("too_long").with_context("len", line.len()));
    }
    let line = line.trim();
    if line.contains(['\n', '\r', '\0']) {
        return Err(malformed("more_than_one_line"));
    }
    let mut words = line.split_ascii_whitespace();
    let Some(name) = words.next() else {
        return Err(malformed("empty"));
    };
    let Some(algorithm) = SshKeyAlgorithm::parse(name) else {
        // A line starting with options (`no-pty,command="…" ssh-ed25519 …`)
        // or a known but refused type both land here.
        return Err(invalid("algorithm_unsupported")
            .with_context("algorithm", name.chars().take(64).collect::<String>()));
    };
    let Some(encoded) = words.next() else {
        return Err(malformed("no_key"));
    };
    let blob = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| malformed("base64"))?;
    check_blob(algorithm, &blob)?;
    let comment: String = words
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_COMMENT_CHARS)
        .collect();
    Ok(SshPublicKey {
        algorithm,
        blob,
        comment,
    })
}

/// A wire-format `string` (u32 length, bytes) at the front of `input`.
fn take_string<'a>(input: &mut &'a [u8]) -> Option<&'a [u8]> {
    let (len, rest) = input.split_first_chunk::<4>()?;
    let len = usize::try_from(u32::from_be_bytes(*len)).ok()?;
    if rest.len() < len {
        return None;
    }
    let (value, rest) = rest.split_at(len);
    *input = rest;
    Some(value)
}

/// The blob is exactly `string(algorithm) [string(curve)] string(key)`.
fn check_blob(algorithm: SshKeyAlgorithm, blob: &[u8]) -> Result<(), ForgeError> {
    let mut rest = blob;
    let name = take_string(&mut rest).ok_or_else(|| malformed("blob_truncated"))?;
    if name != algorithm.as_str().as_bytes() {
        return Err(malformed("blob_algorithm_mismatch"));
    }
    if let Some(curve) = algorithm.curve() {
        let named = take_string(&mut rest).ok_or_else(|| malformed("blob_truncated"))?;
        if named != curve.as_bytes() {
            return Err(malformed("blob_curve_mismatch"));
        }
    }
    let key = take_string(&mut rest).ok_or_else(|| malformed("blob_truncated"))?;
    if key.len() != algorithm.key_len() {
        return Err(malformed("key_length"));
    }
    if algorithm.curve().is_some() && key.first() != Some(&0x04) {
        return Err(malformed("point_not_uncompressed"));
    }
    if !rest.is_empty() {
        return Err(malformed("trailing_bytes"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "ssh_key_tests.rs"]
mod tests;
