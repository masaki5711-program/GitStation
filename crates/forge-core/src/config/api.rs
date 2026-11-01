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

use forge_errors::ForgeError;

use super::{invalid, secret};
use crate::cursor::{CursorKeys, KEYS_CONFIG_KEY};

/// Upper bound for the key file (a mistaken `/dev/zero` must not exhaust
/// memory; 64 KiB holds hundreds of keys).
const MAX_KEYS_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
pub struct ApiConfig {
    pub(super) cursor_keys_file: Option<PathBuf>,
}

impl ApiConfig {
    pub(super) fn new(cursor_keys_file: Option<PathBuf>) -> Self {
        Self { cursor_keys_file }
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
