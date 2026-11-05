//! `[webhook]`: where webhooks may deliver and how their secrets are kept
//! (M3 I2, ADR-0016 §8, ADR-0013).
//!
//! - `webhook.allowed_targets` (default empty): the allowlist, entries
//!   `host:port`, `a.b.c.d[/len]:port` or `[v6[/len]]:port` separated by
//!   commas or white space (`crate::webhook::TargetPolicy`). Empty means no
//!   webhook can be created or delivered until the operator lists the
//!   internal receivers (kanflow, …). Loopback, link-local, metadata and
//!   the other always-refused ranges stay refused whatever it lists.
//! - `webhook.allow_http` (default `false`): accept `http` URLs
//!   (development only; `forge doctor` warns when it is on).
//! - `webhook.secret_keys_file`: the AES-256-GCM keys that seal the
//!   webhooks' secrets (`<key_id> <64 hex>` per line; a secret file,
//!   absolute path). Without it webhooks cannot be created and `forge
//!   worker` does not deliver (doctor `webhook.secret_keys`).
//! - `webhook.ca_file`: the PEM bundle of the CAs an `https` receiver's
//!   certificate is verified against (absolute path; no public roots).
//!   Without it an `https` delivery fails TLS (`reason=tls`).
//! - `webhook.delivery_retention_days` (default 30, 1..=365): how long the
//!   delivery history is kept.

use std::path::{Path, PathBuf};

use forge_errors::ForgeError;

use super::{database, invalid, unsigned};
use crate::webhook::{TargetPolicy, WebhookSecretKeys};

pub const ALLOWED_TARGETS_KEY: &str = crate::webhook::ALLOWED_TARGETS_KEY;
pub const ALLOW_HTTP_KEY: &str = "webhook.allow_http";
pub const SECRET_KEYS_KEY: &str = crate::webhook::SECRET_KEYS_KEY;
pub const CA_FILE_KEY: &str = "webhook.ca_file";
pub const RETENTION_KEY: &str = "webhook.delivery_retention_days";

/// The most bytes of `webhook.ca_file` read.
const MAX_CA_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookConfig {
    /// `webhook.allowed_targets` and `webhook.allow_http`.
    pub policy: TargetPolicy,
    pub secret_keys_file: Option<PathBuf>,
    pub ca_file: Option<PathBuf>,
    /// `webhook.delivery_retention_days`.
    pub delivery_retention_days: u32,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            policy: TargetPolicy::default(),
            secret_keys_file: None,
            ca_file: None,
            delivery_retention_days: Self::DEFAULT_RETENTION_DAYS,
        }
    }
}

impl WebhookConfig {
    pub const DEFAULT_RETENTION_DAYS: u32 = 30;
    pub const MAX_RETENTION_DAYS: u32 = 365;

    pub(super) fn load<'a>(
        get: impl Fn(&str) -> Option<&'a str> + Copy,
    ) -> Result<Self, ForgeError> {
        let text = |key: &'static str| match get(key).map(str::trim) {
            Some("") => Err(invalid(key, "empty")),
            other => Ok(other),
        };
        let allow_http = match text(ALLOW_HTTP_KEY)? {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(invalid(ALLOW_HTTP_KEY, "invalid")),
        };
        // An empty list is the default, so an empty value is accepted here.
        let targets = get(ALLOWED_TARGETS_KEY).unwrap_or("");
        let policy = TargetPolicy::parse(targets, allow_http)?;
        let path = |key: &'static str| {
            text(key)?
                .map(|p| database::password_path(key, p))
                .transpose()
        };
        let delivery_retention_days =
            match unsigned(get, RETENTION_KEY, Self::DEFAULT_RETENTION_DAYS)? {
                0 => return Err(invalid(RETENTION_KEY, "zero")),
                n if n > Self::MAX_RETENTION_DAYS => {
                    return Err(invalid(RETENTION_KEY, "above_max"))
                }
                n => n,
            };
        Ok(Self {
            policy,
            secret_keys_file: path(SECRET_KEYS_KEY)?,
            ca_file: path(CA_FILE_KEY)?,
            delivery_retention_days,
        })
    }

    /// Read `webhook.secret_keys_file` now (`None` when not configured).
    pub fn secret_keys(&self) -> Result<Option<WebhookSecretKeys>, ForgeError> {
        self.secret_keys_file
            .as_deref()
            .map(WebhookSecretKeys::read)
            .transpose()
    }

    /// Read `webhook.ca_file` now (`None` when not configured): at most
    /// 1 MiB, `FG-SYS-0002 config_key=webhook.ca_file reason=unreadable`
    /// otherwise.
    pub fn ca_pem(&self) -> Result<Option<Vec<u8>>, ForgeError> {
        self.ca_file.as_deref().map(read_ca).transpose()
    }
}

fn read_ca(path: &Path) -> Result<Vec<u8>, ForgeError> {
    use std::io::Read as _;
    let unreadable = |e: std::io::Error| {
        invalid(CA_FILE_KEY, "unreadable")
            .with_context("path", path.display())
            .with_source(e)
    };
    if !std::fs::metadata(path).map_err(unreadable)?.is_file() {
        return Err(invalid(CA_FILE_KEY, "invalid").with_context("path", path.display()));
    }
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let mut bytes = Vec::new();
    file.take(MAX_CA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > MAX_CA_BYTES {
        return Err(invalid(CA_FILE_KEY, "too_large").with_context("path", path.display()));
    }
    Ok(bytes)
}
