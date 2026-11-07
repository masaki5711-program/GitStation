//! `[mirror]`: the push mirror to GitLab (M3 O2, ADR-0024 Amendment O2,
//! runbook gitlab-mirror).
//!
//! | key | default | rule |
//! |---|---|---|
//! | `mirror.credentials_dir` | — | absolute path of the directory holding the credential files (`<name>`, one line `<username>:<token>`, mode 0600). Without it no mirror can be set and `forge worker` runs no mirror lane (`forge.worker.mirror_lane_off`) |
//! | `mirror.allowed_hosts` | empty | `host` / `host:port` (443 when no port) separated by commas or white space; empty: no mirror can be set |
//! | `mirror.allow_http` | `false` | accept `http` destinations (tests only; doctor warns) |
//! | `mirror.ca_file` | — | absolute path of the PEM bundle that verifies GitLab (`http.sslCAInfo`); the system store otherwise |
//! | `mirror.push_timeout_secs` | 300 | 30 ..= 1500, one push |
//! | `mirror.lag_alert_secs` | 900 | 60 ..= 86 400, FG-INT-0014 |
//!
//! Every problem is `FG-SYS-0002` with `config_key` and `reason`. A
//! credential file is read when it is used (setting a mirror checks it, each
//! push reads it again), so a rotated token takes effect at the next push.

use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use super::secret::read_secret_text;
use super::{database, invalid, unsigned};
use crate::mirror::{CredentialName, MirrorCredential, MirrorPolicy, MAX_CREDENTIAL_BYTES};

pub const CREDENTIALS_DIR_KEY: &str = "mirror.credentials_dir";
pub const ALLOWED_HOSTS_KEY: &str = crate::mirror::ALLOWED_HOSTS_KEY;
pub const ALLOW_HTTP_KEY: &str = "mirror.allow_http";
pub const CA_FILE_KEY: &str = "mirror.ca_file";
pub const PUSH_TIMEOUT_KEY: &str = "mirror.push_timeout_secs";
pub const LAG_ALERT_KEY: &str = "mirror.lag_alert_secs";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorConfig {
    pub credentials_dir: Option<PathBuf>,
    /// `mirror.allowed_hosts` and `mirror.allow_http`.
    pub policy: MirrorPolicy,
    pub ca_file: Option<PathBuf>,
    pub push_timeout: Duration,
    pub lag_alert: Duration,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            credentials_dir: None,
            policy: MirrorPolicy::default(),
            ca_file: None,
            push_timeout: Duration::from_secs(Self::DEFAULT_PUSH_TIMEOUT_SECS.into()),
            lag_alert: Duration::from_secs(Self::DEFAULT_LAG_ALERT_SECS.into()),
        }
    }
}

/// Why a credential could not be used (the route answers FG-INT-0021, the
/// job FG-INT-0013 `reason=credential_unreadable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialProblem {
    /// `mirror.credentials_dir` is not set.
    NotConfigured,
    /// No regular file of that name.
    Missing,
    /// Readable by group or others.
    Insecure,
    /// Unreadable or not one `<username>:<token>` line.
    Invalid,
}

impl CredentialProblem {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Missing => "credential_missing",
            Self::Insecure => "credential_insecure",
            Self::Invalid => "credential_invalid",
        }
    }
}

impl MirrorConfig {
    pub const DEFAULT_PUSH_TIMEOUT_SECS: u32 = 300;
    pub const MIN_PUSH_TIMEOUT_SECS: u32 = 30;
    pub const MAX_PUSH_TIMEOUT_SECS: u32 = 1500;
    pub const DEFAULT_LAG_ALERT_SECS: u32 = 900;
    pub const MIN_LAG_ALERT_SECS: u32 = 60;
    pub const MAX_LAG_ALERT_SECS: u32 = 86_400;

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
        let policy = MirrorPolicy::parse(get(ALLOWED_HOSTS_KEY).unwrap_or(""), allow_http)?;
        let path = |key: &'static str| {
            text(key)?
                .map(|p| database::password_path(key, p))
                .transpose()
        };
        let bounded = |key: &'static str, default: u32, min: u32, max: u32| match unsigned(
            get, key, default,
        )? {
            n if n < min => Err(invalid(key, "below_min")),
            n if n > max => Err(invalid(key, "above_max")),
            n => Ok(Duration::from_secs(n.into())),
        };
        Ok(Self {
            credentials_dir: path(CREDENTIALS_DIR_KEY)?,
            policy,
            ca_file: path(CA_FILE_KEY)?,
            push_timeout: bounded(
                PUSH_TIMEOUT_KEY,
                Self::DEFAULT_PUSH_TIMEOUT_SECS,
                Self::MIN_PUSH_TIMEOUT_SECS,
                Self::MAX_PUSH_TIMEOUT_SECS,
            )?,
            lag_alert: bounded(
                LAG_ALERT_KEY,
                Self::DEFAULT_LAG_ALERT_SECS,
                Self::MIN_LAG_ALERT_SECS,
                Self::MAX_LAG_ALERT_SECS,
            )?,
        })
    }

    /// The path of credential `name` (`None` without `credentials_dir`).
    pub fn credential_path(&self, name: &CredentialName) -> Option<PathBuf> {
        self.credentials_dir
            .as_deref()
            .map(|d| d.join(name.as_str()))
    }

    /// Read credential `name` now: the file is opened once, refused unless
    /// it is a regular file of at most 4 KiB readable only by its owner
    /// (Unix), and parsed as one `<username>:<token>` line. The problem is
    /// classified; the content never enters an error.
    pub fn credential(
        &self,
        name: &CredentialName,
    ) -> Result<MirrorCredential, (CredentialProblem, ForgeError)> {
        let Some(path) = self.credential_path(name) else {
            return Err((
                CredentialProblem::NotConfigured,
                invalid(CREDENTIALS_DIR_KEY, "missing"),
            ));
        };
        let text = read_secret_text(CREDENTIALS_DIR_KEY, &path, MAX_CREDENTIAL_BYTES as u64)
            .map_err(|e| (classify(&e, &path), e))?;
        MirrorCredential::parse(&text).map_err(|e| (CredentialProblem::Invalid, e))
    }
}

fn classify(e: &ForgeError, path: &Path) -> CredentialProblem {
    let reason = e
        .context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.as_str());
    match reason {
        Some("insecure_permissions") => CredentialProblem::Insecure,
        Some("unreadable") if !path.exists() => CredentialProblem::Missing,
        Some("invalid") if !path.is_file() => CredentialProblem::Missing,
        _ if e.code() == ErrorCode::ConfigInvalid => CredentialProblem::Invalid,
        _ => CredentialProblem::Invalid,
    }
}

#[cfg(test)]
#[path = "mirror_tests.rs"]
mod tests;
