//! `[ci]`: the CI variables' keys (M4 C6, plan 14 §9 #2, ADR-0013).
//!
//! - `ci.secret_keys_file`: the AES-256-GCM keys that seal the CI
//!   variables (`<key_id> <64 hex>` per line; a secret file, absolute
//!   path; [`crate::ci_secret::CiSecretKeys`]). Without it no variable can
//!   be set and a job that asks for secrets is failed with `FG-CI-0016`
//!   (doctor `ci.secret_keys`); a file that does not read stops
//!   `forge serve` at the start.
//! - `ci.artifact_quota_bytes` (default 5 GiB, 1 MiB..=1 TiB): the most
//!   bytes of stored CI artifacts one project may hold
//!   (`FG-CI-0022` past it).

use std::path::PathBuf;

use forge_errors::ForgeError;

use super::{database, invalid};
use crate::ci_secret::{CiSecretKeys, SECRET_KEYS_KEY};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiConfig {
    /// `ci.secret_keys_file`.
    pub secret_keys_file: Option<PathBuf>,
    /// `ci.artifact_quota_bytes`.
    pub artifact_quota_bytes: u64,
}

impl Default for CiConfig {
    fn default() -> Self {
        Self {
            secret_keys_file: None,
            artifact_quota_bytes: Self::DEFAULT_ARTIFACT_QUOTA_BYTES,
        }
    }
}

/// The configuration key of the artifact quota.
pub const ARTIFACT_QUOTA_KEY: &str = "ci.artifact_quota_bytes";

impl CiConfig {
    pub const DEFAULT_ARTIFACT_QUOTA_BYTES: u64 = 5 * 1024 * 1024 * 1024;
    const MIN_ARTIFACT_QUOTA_BYTES: u64 = 1024 * 1024;
    const MAX_ARTIFACT_QUOTA_BYTES: u64 = 1024 * 1024 * 1024 * 1024;

    pub(super) fn load<'a>(
        get: impl Fn(&str) -> Option<&'a str> + Copy,
    ) -> Result<Self, ForgeError> {
        let secret_keys_file = match get(SECRET_KEYS_KEY).map(str::trim) {
            None => None,
            Some("") => return Err(invalid(SECRET_KEYS_KEY, "empty")),
            Some(p) => Some(database::password_path(SECRET_KEYS_KEY, p)?),
        };
        let artifact_quota_bytes = match get(ARTIFACT_QUOTA_KEY).map(str::trim) {
            None => Self::DEFAULT_ARTIFACT_QUOTA_BYTES,
            Some(raw) => match raw.parse::<u64>() {
                Ok(n) if n < Self::MIN_ARTIFACT_QUOTA_BYTES => {
                    return Err(invalid(ARTIFACT_QUOTA_KEY, "below_min"))
                }
                Ok(n) if n > Self::MAX_ARTIFACT_QUOTA_BYTES => {
                    return Err(invalid(ARTIFACT_QUOTA_KEY, "above_max"))
                }
                Ok(n) => n,
                Err(_) => return Err(invalid(ARTIFACT_QUOTA_KEY, "invalid")),
            },
        };
        Ok(Self {
            secret_keys_file,
            artifact_quota_bytes,
        })
    }

    /// Read `ci.secret_keys_file` now (`None` when not configured).
    pub fn secret_keys(&self) -> Result<Option<CiSecretKeys>, ForgeError> {
        self.secret_keys_file
            .as_deref()
            .map(CiSecretKeys::read)
            .transpose()
    }
}
