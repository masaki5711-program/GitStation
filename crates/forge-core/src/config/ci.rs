//! `[ci]`: the CI variables' keys (M4 C6, plan 14 §9 #2, ADR-0013).
//!
//! - `ci.secret_keys_file`: the AES-256-GCM keys that seal the CI
//!   variables (`<key_id> <64 hex>` per line; a secret file, absolute
//!   path; [`crate::ci_secret::CiSecretKeys`]). Without it no variable can
//!   be set and a job that asks for secrets is failed with `FG-CI-0016`
//!   (doctor `ci.secret_keys`); a file that does not read stops
//!   `forge serve` at the start.

use std::path::PathBuf;

use forge_errors::ForgeError;

use super::{database, invalid};
use crate::ci_secret::{CiSecretKeys, SECRET_KEYS_KEY};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CiConfig {
    /// `ci.secret_keys_file`.
    pub secret_keys_file: Option<PathBuf>,
}

impl CiConfig {
    pub(super) fn load<'a>(
        get: impl Fn(&str) -> Option<&'a str> + Copy,
    ) -> Result<Self, ForgeError> {
        let secret_keys_file = match get(SECRET_KEYS_KEY).map(str::trim) {
            None => None,
            Some("") => return Err(invalid(SECRET_KEYS_KEY, "empty")),
            Some(p) => Some(database::password_path(SECRET_KEYS_KEY, p)?),
        };
        Ok(Self { secret_keys_file })
    }

    /// Read `ci.secret_keys_file` now (`None` when not configured).
    pub fn secret_keys(&self) -> Result<Option<CiSecretKeys>, ForgeError> {
        self.secret_keys_file
            .as_deref()
            .map(CiSecretKeys::read)
            .transpose()
    }
}
