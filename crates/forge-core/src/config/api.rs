//! `[api]`: the keys of the opaque list cursors (M1 phase 8b, ADR-0013).
//!
//! `api.cursor_keys_file` names a secret file outside the database, one key
//! per line: `<kid> <hex>` — kid 1..=65535, exactly 32 bytes (AES-256) as 64
//! hex digits; blank lines and `#` comments are ignored. The highest kid
//! encrypts new cursors, every listed key still opens the cursors it
//! sealed (rotation: add a key, let old cursors age out, remove the old
//! one). The format and the crypto are [`crate::cursor`]. The path is
//! checked at load time; the server reads the file when a list route first
//! needs it ([`ApiConfig::cursor_keys`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, secret};
use crate::cursor::{CursorKeys, KEYS_CONFIG_KEY};

/// Upper bound for the key file (a mistaken `/dev/zero` must not exhaust
/// memory; 64 KiB holds hundreds of keys).
const MAX_KEYS_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
pub struct ApiConfig {
    pub(super) cursor_keys_file: Option<PathBuf>,
    /// `api.list_max_concurrent`: list requests running at once, in total
    /// (default 16). Over it: `FG-WEB-0008 reason=list_concurrency`.
    pub list_max_concurrent: u32,
    /// `api.list_max_concurrent_per_principal`: list requests running at
    /// once for one signed-in user (default 2, at most the total).
    pub list_max_concurrent_per_principal: u32,
    /// `api.cursor_max_age_s`: how long a cursor stays valid (default 24 h);
    /// an older one is `FG-WEB-0006 cursor_invalid rule=expired`.
    pub cursor_max_age: Duration,
}

impl ApiConfig {
    pub const DEFAULT_LIST_MAX_CONCURRENT: u32 = 16;
    pub const DEFAULT_LIST_MAX_CONCURRENT_PER_PRINCIPAL: u32 = 2;
    pub const DEFAULT_CURSOR_MAX_AGE_S: u32 = 86_400;

    pub(super) fn new(
        cursor_keys_file: Option<PathBuf>,
        list_max_concurrent: u32,
        list_max_concurrent_per_principal: u32,
        cursor_max_age: Duration,
    ) -> Result<Self, ForgeError> {
        if list_max_concurrent_per_principal > list_max_concurrent {
            return Err(invalid(
                "api.list_max_concurrent_per_principal",
                "above_max",
            ));
        }
        Ok(Self {
            cursor_keys_file,
            list_max_concurrent,
            list_max_concurrent_per_principal,
            cursor_max_age,
        })
    }

    /// Whether `api.cursor_keys_file` is configured.
    pub fn has_cursor_keys(&self) -> bool {
        self.cursor_keys_file.is_some()
    }

    /// Read `api.cursor_keys_file` now (`None` when it is not configured).
    pub fn cursor_keys(&self) -> Result<Option<CursorKeys>, ForgeError> {
        self.cursor_keys_file
            .as_deref()
            .map(read_cursor_keys)
            .transpose()
    }

    /// Read `api.cursor_keys_file`, which the caller needs: not configured
    /// is `FG-SYS-0002 config_key=api.cursor_keys_file reason=missing`.
    pub fn require_cursor_keys(&self) -> Result<CursorKeys, ForgeError> {
        self.cursor_keys()?
            .ok_or_else(|| invalid(KEYS_CONFIG_KEY, "missing"))
    }
}

fn read_cursor_keys(path: &Path) -> Result<CursorKeys, ForgeError> {
    let text = secret::read_secret_text(KEYS_CONFIG_KEY, path, MAX_KEYS_BYTES)?;
    CursorKeys::parse(&text).map_err(|e| e.with_context("path", path.display()))
}
