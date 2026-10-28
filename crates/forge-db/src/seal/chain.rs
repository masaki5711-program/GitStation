//! The seal values of ADR-0018 §4 (for every hash version):
//!
//! - `rows_sha256` = SHA-256 of the row hashes concatenated in seq order;
//! - `seal_sha256` = SHA-256 of `be16(hash_version) ‖ be16(seal_epoch) ‖
//!   be64(first_seq) ‖ be64(last_seq) ‖ be64(events_high_seq) ‖ rows_sha256
//!   ‖ prev_seal_sha256`;
//! - the first seal's `prev_seal_sha256` = SHA-256 of `forge-audit-genesis`
//!   (the guard of forge.audit_seal uses the same value);
//! - `hmac` = HMAC-SHA256(key[key_id], seal_sha256). `key_id` is not in
//!   `seal_sha256`.

use forge_core::config::SealKey;
use forge_errors::{ErrorCode, ForgeError};
use hmac::{Hmac, Mac as _};
use sha2::{Digest as _, Sha256};

pub type Digest32 = [u8; 32];

/// `prev_seal_sha256` of the first seal.
pub fn genesis() -> Digest32 {
    Sha256::digest(b"forge-audit-genesis").into()
}

/// SHA-256 of the row hashes in seq order.
pub fn rows_sha256<'a>(row_hashes: impl IntoIterator<Item = &'a Digest32>) -> Digest32 {
    let mut h = Sha256::new();
    for row in row_hashes {
        h.update(row);
    }
    h.finalize().into()
}

/// The fields `seal_sha256` covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealFields {
    pub hash_version: i16,
    pub seal_epoch: i16,
    pub first_seq: i64,
    pub last_seq: i64,
    pub events_high_seq: i64,
    pub rows_sha256: Digest32,
    pub prev_seal_sha256: Digest32,
}

impl SealFields {
    pub fn seal_sha256(&self) -> Digest32 {
        let mut h = Sha256::new();
        h.update(self.hash_version.to_be_bytes());
        h.update(self.seal_epoch.to_be_bytes());
        h.update(self.first_seq.to_be_bytes());
        h.update(self.last_seq.to_be_bytes());
        h.update(self.events_high_seq.to_be_bytes());
        h.update(self.rows_sha256);
        h.update(self.prev_seal_sha256);
        h.finalize().into()
    }
}

fn mac(key: &SealKey) -> Result<Hmac<Sha256>, ForgeError> {
    // HMAC takes a key of any length; the error type exists for other MACs.
    Hmac::<Sha256>::new_from_slice(key.expose()).map_err(|_| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "seal.hmac")
            .with_context("reason", "key_rejected")
            .with_context("key_id", key.id())
    })
}

/// HMAC-SHA256 of a seal under `key`.
pub fn hmac(key: &SealKey, seal_sha256: &Digest32) -> Result<Digest32, ForgeError> {
    let mut m = mac(key)?;
    m.update(seal_sha256);
    Ok(m.finalize().into_bytes().into())
}

/// Whether `stored` is the HMAC of `seal_sha256` under `key` (constant time).
pub fn hmac_matches(
    key: &SealKey,
    seal_sha256: &Digest32,
    stored: &[u8],
) -> Result<bool, ForgeError> {
    let mut m = mac(key)?;
    m.update(seal_sha256);
    Ok(m.verify_slice(stored).is_ok())
}

/// A 32-byte column (the table CHECKs guarantee the length).
pub fn digest(column: &'static str, bytes: &[u8]) -> Result<Digest32, ForgeError> {
    bytes.try_into().map_err(|_| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "seal.read")
            .with_context("reason", "digest_length")
            .with_context("column", column)
            .with_context("length", bytes.len())
    })
}
