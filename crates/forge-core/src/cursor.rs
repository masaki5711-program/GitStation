//! Opaque keyset cursors for API lists (M1 phase 8b, ADR-0015 / ADR-0016).
//!
//! A list hands out `next_cursor` so a client can continue after the last
//! row it saw. The position must not reveal anything the caller may not
//! see (the id of a scanned but invisible row, how many rows were skipped),
//! and a client must not be able to forge one. So a cursor is the list's
//! own payload, encrypted and authenticated with AES-256-GCM:
//!
//! ```text
//! base64url_nopad( version:u8 = 2 | kid:u16 BE | nonce:[u8; 12] | ciphertext | tag:[u8; 16] )
//! plaintext = issued_at:u64 BE (Unix seconds) | payload
//! aad = "forge-cursor" | version | kid | purpose
//! ```
//!
//! - The issue time is inside the sealed plaintext, so it is authenticated
//!   like the payload; [`CursorKeys::decode`] refuses a cursor older than
//!   the caller's `max_age` (`rule=expired`). A time in the future (a
//!   clock stepped back) counts as age zero.
//!
//! - `purpose` binds the cursor to one list and one caller (e.g.
//!   `projects.list:user=42`): a cursor of another list or another caller
//!   fails the tag like a tampered one.
//! - The key comes from `api.cursor_keys_file` (ADR-0013: a secret file
//!   outside the database, one `<kid> <hex>` line per key). The highest
//!   `kid` encrypts; every listed key decrypts, so a rotation adds a key,
//!   waits for the old cursors to age out, then removes the old one.
//! - Verification is the AEAD tag check (constant time). Every failure is
//!   `FG-WEB-0006 reason=cursor_invalid` with a `rule` (`too_long`,
//!   `encoding`, `truncated`, `version`, `kid`, `tag`, `expired`) and
//!   **never the cursor's value**.
//!
//! The nonce is 96 random bits from the operating system per cursor; with
//! one key the collision bound (2^-32 after 2^32 cursors) is far beyond an
//! instance's lifetime, and rotation resets it.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use forge_errors::{ErrorCode, ForgeError};
use std::time::Duration;

/// The only format version this build writes and reads.
const VERSION: u8 = 2;
const DOMAIN: &[u8] = b"forge-cursor";
const KID_BYTES: usize = 2;
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const ISSUED_BYTES: usize = 8;
const HEADER_BYTES: usize = 1 + KID_BYTES + NONCE_BYTES;
/// An AES-256 key.
pub const KEY_BYTES: usize = 32;
/// The largest payload a list may put in a cursor.
pub const MAX_PAYLOAD_BYTES: usize = 512;
/// The longest cursor text accepted (longer is `rule=too_long`, refused
/// before decoding).
pub const MAX_CURSOR_CHARS: usize =
    (HEADER_BYTES + ISSUED_BYTES + MAX_PAYLOAD_BYTES + TAG_BYTES).div_ceil(3) * 4;

/// The configuration key that names the key file (errors name it).
pub const KEYS_CONFIG_KEY: &str = "api.cursor_keys_file";

/// One cursor key. Never printed.
pub struct CursorKey {
    kid: u16,
    cipher: Aes256Gcm,
}

impl CursorKey {
    pub fn kid(&self) -> u16 {
        self.kid
    }
}

impl std::fmt::Debug for CursorKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CursorKey({}, [redacted])", self.kid)
    }
}

/// The keys of `api.cursor_keys_file`: the newest (highest `kid`) encrypts,
/// all of them decrypt.
#[derive(Debug)]
pub struct CursorKeys {
    newest: CursorKey,
    older: Vec<CursorKey>,
}

fn key_file_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", KEYS_CONFIG_KEY)
        .with_context("reason", reason)
}

/// `FG-WEB-0006 reason=cursor_invalid rule=<rule>` (never the value).
fn cursor_invalid(rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "cursor_invalid")
        .with_context("rule", rule)
}

impl CursorKeys {
    /// Parse the key file's text: one `<kid> <hex>` per line (kid
    /// 1..=65535, exactly 32 bytes of key as 64 hex digits); blank lines and
    /// lines starting with `#` are ignored. Errors are `FG-SYS-0002
    /// config_key=api.cursor_keys_file` with `reason` and `line`, never the
    /// line's content.
    pub fn parse(text: &str) -> Result<Self, ForgeError> {
        let mut keys: Vec<CursorKey> = Vec::new();
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
            // Digits only: `parse` alone would take "+1".
            let kid: u16 = Some(kid)
                .filter(|k| k.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|k| k.parse().ok())
                .filter(|k| *k >= 1)
                .ok_or_else(|| bad("invalid_key_id"))?;
            let bytes = crate::config::decode_hex(hex)
                .filter(|b| b.len() == KEY_BYTES)
                .ok_or_else(|| bad("invalid_key"))?;
            if keys.iter().any(|k| k.kid == kid) {
                return Err(bad("duplicate"));
            }
            let cipher = Aes256Gcm::new_from_slice(&bytes).map_err(|_| bad("invalid_key"))?;
            keys.push(CursorKey { kid, cipher });
        }
        keys.sort_by_key(|k| k.kid);
        let newest = keys.pop().ok_or_else(|| key_file_invalid("empty"))?;
        Ok(Self {
            newest,
            older: keys,
        })
    }

    /// The key that encrypts new cursors.
    pub fn encrypting(&self) -> &CursorKey {
        &self.newest
    }

    fn get(&self, kid: u16) -> Option<&CursorKey> {
        std::iter::once(&self.newest)
            .chain(&self.older)
            .find(|k| k.kid == kid)
    }

    /// Seal `payload` for `purpose` with the newest key. A payload above
    /// [`MAX_PAYLOAD_BYTES`] is a programming error (`FG-SYS-0001
    /// reason=cursor_payload_too_large`); no random bytes is `FG-SYS-0001
    /// reason=rng_unavailable`.
    pub fn encode(&self, purpose: &str, payload: &[u8]) -> Result<String, ForgeError> {
        self.encode_at(purpose, payload, unix_now()?)
    }

    /// [`CursorKeys::encode`] with an explicit issue time (Unix seconds).
    pub fn encode_at(
        &self,
        purpose: &str,
        payload: &[u8],
        issued_at: u64,
    ) -> Result<String, ForgeError> {
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "cursor_payload_too_large")
                .with_context("bytes", payload.len()));
        }
        let key = &self.newest;
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::getrandom(&mut nonce).map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "rng_unavailable")
        })?;
        let aad = aad(key.kid, purpose);
        let mut plain = Vec::with_capacity(ISSUED_BYTES + payload.len());
        plain.extend_from_slice(&issued_at.to_be_bytes());
        plain.extend_from_slice(payload);
        let sealed = key
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &plain,
                    aad: &aad,
                },
            )
            .map_err(|_| {
                ForgeError::new(ErrorCode::Internal).with_context("reason", "cursor_encrypt_failed")
            })?;
        let mut bytes = Vec::with_capacity(HEADER_BYTES + sealed.len());
        bytes.push(VERSION);
        bytes.extend_from_slice(&key.kid.to_be_bytes());
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&sealed);
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Open a cursor minted by [`CursorKeys::encode`] for the same
    /// `purpose`, no older than `max_age`. Every failure is `FG-WEB-0006
    /// reason=cursor_invalid` with a `rule`; the value is never logged.
    pub fn decode(
        &self,
        purpose: &str,
        cursor: &str,
        max_age: Duration,
    ) -> Result<Vec<u8>, ForgeError> {
        self.decode_at(purpose, cursor, max_age, unix_now()?)
    }

    /// [`CursorKeys::decode`] at an explicit time (Unix seconds).
    pub fn decode_at(
        &self,
        purpose: &str,
        cursor: &str,
        max_age: Duration,
        now: u64,
    ) -> Result<Vec<u8>, ForgeError> {
        if cursor.len() > MAX_CURSOR_CHARS {
            return Err(cursor_invalid("too_long"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(cursor)
            .map_err(|_| cursor_invalid("encoding"))?;
        if bytes.len() < HEADER_BYTES + TAG_BYTES {
            return Err(cursor_invalid("truncated"));
        }
        let (header, sealed) = bytes.split_at(HEADER_BYTES);
        let (version, rest) = header.split_at(1);
        if version != [VERSION] {
            return Err(cursor_invalid("version"));
        }
        let (kid, nonce) = rest.split_at(KID_BYTES);
        let kid: [u8; KID_BYTES] = kid.try_into().map_err(|_| cursor_invalid("truncated"))?;
        let kid = u16::from_be_bytes(kid);
        let key = self.get(kid).ok_or_else(|| cursor_invalid("kid"))?;
        let aad = aad(kid, purpose);
        let plain = key
            .cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: &aad,
                },
            )
            .map_err(|_| cursor_invalid("tag"))?;
        let (issued, payload) = plain
            .split_first_chunk::<ISSUED_BYTES>()
            .ok_or_else(|| cursor_invalid("truncated"))?;
        let issued_at = u64::from_be_bytes(*issued);
        if now.saturating_sub(issued_at) > max_age.as_secs() {
            return Err(cursor_invalid("expired"));
        }
        Ok(payload.to_vec())
    }
}

/// The wall clock in Unix seconds (`FG-SYS-0001 reason=clock_unavailable`
/// when it is before the epoch).
fn unix_now() -> Result<u64, ForgeError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "clock_unavailable")
        })
}

/// The associated data: the domain, the header and the purpose.
fn aad(kid: u16, purpose: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(DOMAIN.len() + 1 + KID_BYTES + purpose.len());
    aad.extend_from_slice(DOMAIN);
    aad.push(VERSION);
    aad.extend_from_slice(&kid.to_be_bytes());
    aad.extend_from_slice(purpose.as_bytes());
    aad
}
