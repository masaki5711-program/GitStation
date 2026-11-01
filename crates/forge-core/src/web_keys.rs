//! The keys of `web.session_key_file` (M2 MR3, ADR-0021 §7, ADR-0013).
//!
//! The file has the format of `api.cursor_keys_file`: one `<kid> <hex>` per
//! line (kid 1..=65535, exactly 32 bytes as 64 hex digits), blank lines and
//! `#` comments ignored. The highest kid encrypts; every listed key opens
//! what it sealed (rotation: add a key, wait, remove the old one).
//!
//! A file key is never used directly. Each purpose gets its own AES-256-GCM
//! key, derived per kid with HKDF-SHA256 (RFC 5869):
//!
//! ```text
//! key(kid, purpose) = HKDF-SHA256(ikm = file key of kid, salt = none,
//!                                 info = "gitstation/web/<purpose>/v1")[..32]
//! ```
//!
//! so a ciphertext of one purpose never opens under another (the keys
//! differ, not only the associated data). The purposes are
//! [`KeyPurpose::OidcState`] (the `__Host-gstn_oidc` cookie, MR4),
//! [`KeyPurpose::RefreshToken`] and [`KeyPurpose::IdToken`] (the
//! `web_sessions.*_enc` columns, MR4 / MR5).
//!
//! Sealed bytes: `version:u8 = 1 | kid:u16 BE | nonce:[u8; 12] | ciphertext | tag:[u8; 16]`;
//! the caller's associated data (for a column: the session's `public_id`)
//! is authenticated with the header. Opening fails with an [`OpenError`]
//! (never the value); key file problems are `FG-SYS-0002
//! config_key=web.session_key_file` with `reason` and `line`.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use forge_errors::{ErrorCode, ForgeError};
use hkdf::Hkdf;
use sha2::Sha256;

/// The configuration key that names the key file (errors name it).
pub const KEYS_CONFIG_KEY: &str = "web.session_key_file";
/// The only sealed format this build writes and reads.
const VERSION: u8 = 1;
const KID_BYTES: usize = 2;
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const HEADER_BYTES: usize = 1 + KID_BYTES + NONCE_BYTES;
/// A file key and a derived key: 32 bytes.
pub const KEY_BYTES: usize = 32;

/// What a derived key is for (its HKDF `info`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPurpose {
    /// The login state cookie `__Host-gstn_oidc` (ADR-0021 §3).
    OidcState,
    /// `web_sessions.refresh_token_enc` (ADR-0021 §7).
    RefreshToken,
    /// `web_sessions.id_token_enc` (ADR-0021 §8).
    IdToken,
}

impl KeyPurpose {
    pub const ALL: &'static [KeyPurpose] = &[Self::OidcState, Self::RefreshToken, Self::IdToken];

    /// The purpose's name in the HKDF `info`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OidcState => "oidc-state",
            Self::RefreshToken => "refresh-token",
            Self::IdToken => "id-token",
        }
    }

    /// `gitstation/web/<purpose>/v1`.
    pub fn info(self) -> String {
        format!("gitstation/web/{}/v1", self.as_str())
    }
}

/// HKDF-SHA256 with no salt, 32 bytes of output. The only failure HKDF has
/// is an output longer than 255 × 32 bytes, impossible for 32; it is still
/// reported (`FG-SYS-0001 reason=hkdf_failed`), never replaced.
pub fn hkdf_sha256(ikm: &[u8], info: &[u8]) -> Result<[u8; KEY_BYTES], ForgeError> {
    let mut out = [0u8; KEY_BYTES];
    Hkdf::<Sha256>::new(None, ikm)
        .expand(info, &mut out)
        .map_err(|_| ForgeError::new(ErrorCode::Internal).with_context("reason", "hkdf_failed"))?;
    Ok(out)
}

/// One file key: its kid and the raw bytes. Never printed.
struct FileKey {
    kid: u16,
    bytes: [u8; KEY_BYTES],
}

/// The keys of `web.session_key_file`. `Debug` shows the kids only.
pub struct WebKeys {
    /// Sorted by kid, ascending; never empty.
    keys: Vec<FileKey>,
}

impl std::fmt::Debug for WebKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kids: Vec<u16> = self.keys.iter().map(|k| k.kid).collect();
        write!(f, "WebKeys(kids={kids:?}, [redacted])")
    }
}

fn key_file_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", KEYS_CONFIG_KEY)
        .with_context("reason", reason)
}

impl WebKeys {
    /// Parse the key file's text. Errors are `FG-SYS-0002
    /// config_key=web.session_key_file` with `reason` (`invalid`,
    /// `invalid_key_id`, `invalid_key`, `duplicate`, `empty`) and `line`,
    /// never the line's content.
    pub fn parse(text: &str) -> Result<Self, ForgeError> {
        let mut keys: Vec<FileKey> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = |reason| key_file_invalid(reason).with_context("line", index + 1);
            let mut parts = line.split_whitespace();
            let (Some(kid), Some(hex), None) = (parts.next(), parts.next(), parts.next()) else {
                return Err(bad("invalid"));
            };
            let kid: u16 = Some(kid)
                .filter(|k| k.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|k| k.parse().ok())
                .filter(|k| *k >= 1)
                .ok_or_else(|| bad("invalid_key_id"))?;
            let bytes: [u8; KEY_BYTES] = crate::config::decode_hex(hex)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| bad("invalid_key"))?;
            if keys.iter().any(|k| k.kid == kid) {
                return Err(bad("duplicate"));
            }
            keys.push(FileKey { kid, bytes });
        }
        if keys.is_empty() {
            return Err(key_file_invalid("empty"));
        }
        keys.sort_by_key(|k| k.kid);
        Ok(Self { keys })
    }

    /// The kids in the file, ascending.
    pub fn kids(&self) -> Vec<u16> {
        self.keys.iter().map(|k| k.kid).collect()
    }

    /// The derived keys of one purpose, one per file key.
    pub fn purpose(&self, purpose: KeyPurpose) -> Result<PurposeKeys, ForgeError> {
        let info = purpose.info();
        let mut ciphers = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            let derived = hkdf_sha256(&key.bytes, info.as_bytes())?;
            let cipher = Aes256Gcm::new_from_slice(&derived).map_err(|_| {
                ForgeError::new(ErrorCode::Internal).with_context("reason", "derived_key_invalid")
            })?;
            ciphers.push((key.kid, cipher));
        }
        Ok(PurposeKeys { purpose, ciphers })
    }
}

/// Why sealed bytes did not open. Carries no part of the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// Shorter than a header and a tag.
    Truncated,
    /// Another format version.
    Version,
    /// A kid the key file no longer lists.
    Kid,
    /// The tag does not verify: tampered, another purpose, another
    /// associated data, or another key with the same kid.
    Tag,
}

impl OpenError {
    /// The log `rule`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Truncated => "truncated",
            Self::Version => "version",
            Self::Kid => "kid",
            Self::Tag => "tag",
        }
    }
}

/// The AES-256-GCM keys of one purpose. `Debug` shows the purpose and kids.
pub struct PurposeKeys {
    purpose: KeyPurpose,
    /// Ascending by kid; the last one seals.
    ciphers: Vec<(u16, Aes256Gcm)>,
}

impl std::fmt::Debug for PurposeKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kids: Vec<u16> = self.ciphers.iter().map(|(k, _)| *k).collect();
        write!(f, "PurposeKeys({}, kids={kids:?})", self.purpose.as_str())
    }
}

fn aad_of(kid: u16, aad: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + KID_BYTES + aad.len());
    out.push(VERSION);
    out.extend_from_slice(&kid.to_be_bytes());
    out.extend_from_slice(aad);
    out
}

impl PurposeKeys {
    pub fn purpose(&self) -> KeyPurpose {
        self.purpose
    }

    /// Seal `plain` with the newest key, binding `aad`. No random bytes is
    /// `FG-SYS-0001 reason=rng_unavailable`.
    pub fn seal(&self, aad: &[u8], plain: &[u8]) -> Result<Vec<u8>, ForgeError> {
        let internal = |reason: &'static str| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", reason)
                .with_context("purpose", self.purpose.as_str())
        };
        let (kid, cipher) = self
            .ciphers
            .last()
            .ok_or_else(|| internal("web_keys_empty"))?;
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::getrandom(&mut nonce).map_err(|_| internal("rng_unavailable"))?;
        let aad = aad_of(*kid, aad);
        let sealed = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plain,
                    aad: &aad,
                },
            )
            .map_err(|_| internal("seal_failed"))?;
        let mut out = Vec::with_capacity(HEADER_BYTES + sealed.len());
        out.push(VERSION);
        out.extend_from_slice(&kid.to_be_bytes());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Open bytes sealed by [`PurposeKeys::seal`] of the same purpose with
    /// the same `aad`.
    pub fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, OpenError> {
        if sealed.len() < HEADER_BYTES + TAG_BYTES {
            return Err(OpenError::Truncated);
        }
        let (header, body) = sealed.split_at(HEADER_BYTES);
        let (version, rest) = header.split_at(1);
        if version != [VERSION] {
            return Err(OpenError::Version);
        }
        let (kid, nonce) = rest.split_at(KID_BYTES);
        let kid: [u8; KID_BYTES] = kid.try_into().map_err(|_| OpenError::Truncated)?;
        let kid = u16::from_be_bytes(kid);
        let (_, cipher) = self
            .ciphers
            .iter()
            .find(|(k, _)| *k == kid)
            .ok_or(OpenError::Kid)?;
        cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: body,
                    aad: &aad_of(kid, aad),
                },
            )
            .map_err(|_| OpenError::Tag)
    }
}
