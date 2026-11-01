//! Client configuration: where gstn keeps its files and which server it talks
//! to. Layers, later wins: `config.toml` → `GSTN_*` environment → flags.
//!
//! Layout (per user, never in the repository):
//! - Linux: `$XDG_CONFIG_HOME/gstn` or `~/.config/gstn`; Windows: `%APPDATA%\gstn`;
//!   `GSTN_CONFIG_DIR` overrides both.
//! - `config.toml` — settings (below), no secrets.
//! - `credentials.toml` — only the 0600 fallback of the token store (store.rs).
//! - `state.toml` — reserved for the self-update check (last check time); not
//!   written by this series.
//!
//! The `[update]` table is reserved for `gstn self-update` (plan 01 §3.11:
//! checksum + signature, never automatic): it is accepted and ignored now so
//! that a later release adds behaviour without a config migration.

use crate::error::{ClientError, Result};
use forge_errors::ErrorCode;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 5;
const MAX_TIMEOUT_SECS: u64 = 3600;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// The server's base URL, e.g. `https://git.example.internal`.
    pub host: Option<String>,
    /// An extra PEM CA bundle, added to the system trust store.
    pub ca_file: Option<PathBuf>,
    pub timeout_secs: Option<u64>,
    pub connect_timeout_secs: Option<u64>,
    /// Allow a plain `http://` host that is not loopback (tokens then travel
    /// unencrypted: off by default).
    pub allow_http: Option<bool>,
    /// Reserved for `gstn self-update` (ignored today).
    #[allow(dead_code)]
    pub update: Option<toml::Table>,
}

/// Values that override the file (flags, then environment).
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub host: Option<String>,
    pub config_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub dir: PathBuf,
    pub host: Url,
    pub ca_file: Option<PathBuf>,
    pub timeout: Duration,
    pub connect_timeout: Duration,
}

pub fn config_dir(env: &dyn Fn(&str) -> Option<String>, over: Option<&Path>) -> Result<PathBuf> {
    if let Some(d) = over {
        return Ok(d.to_path_buf());
    }
    if let Some(d) = env("GSTN_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    if cfg!(windows) {
        if let Some(a) = env("APPDATA").filter(|a| !a.is_empty()) {
            return Ok(PathBuf::from(a).join("gstn"));
        }
    } else {
        if let Some(x) = env("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
            return Ok(PathBuf::from(x).join("gstn"));
        }
        if let Some(h) = env("HOME").filter(|h| !h.is_empty()) {
            return Ok(PathBuf::from(h).join(".config").join("gstn"));
        }
    }
    Err(ClientError::local(
        ErrorCode::CliConfigInvalid,
        "config_key=GSTN_CONFIG_DIR reason=missing (no home directory found; set GSTN_CONFIG_DIR)",
    ))
}

/// Normalise a host string to an origin URL (`scheme://host[:port]`, no path).
pub fn parse_host(raw: &str, allow_http: bool) -> Result<Url> {
    let bad = |reason: &str| {
        ClientError::local(
            ErrorCode::CliConfigInvalid,
            format!("config_key=host reason={reason}"),
        )
    };
    let url = Url::parse(raw.trim()).map_err(|_| bad("invalid"))?;
    let host = url.host_str().ok_or_else(|| bad("invalid"))?;
    if !url.username().is_empty() || url.password().is_some() {
        // Credentials in the URL would end up in logs and process listings.
        return Err(bad("invalid"));
    }
    if !(url.path() == "/" || url.path().is_empty())
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(bad("invalid"));
    }
    // The host ends up in a git config value that git runs through a shell
    // (`auth git-credential --install`): only DNS-label characters or a
    // bracketed IPv6 literal are accepted.
    let host_ok = if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
    } else {
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    };
    if !host_ok {
        return Err(bad("invalid"));
    }
    let loopback = host == "localhost"
        || host
            .trim_matches(|c| c == '[' || c == ']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match url.scheme() {
        "https" => {}
        "http" if loopback || allow_http => {}
        _ => return Err(bad("insecure_scheme")),
    }
    let mut origin = url.clone();
    origin.set_path("/");
    Ok(origin)
}

impl Config {
    pub fn load(env: &dyn Fn(&str) -> Option<String>, over: &Overrides) -> Result<Self> {
        let dir = config_dir(env, over.config_dir.as_deref())?;
        let file = read_file(&dir.join("config.toml"))?;
        let host_raw = over
            .host
            .clone()
            .or_else(|| env("GSTN_HOST").filter(|h| !h.is_empty()))
            .or(file.host.clone())
            .ok_or_else(|| {
                ClientError::local(
                    ErrorCode::CliConfigInvalid,
                    format!(
                        "config_key=host reason=missing (set `host` in {}, GSTN_HOST, or --host)",
                        dir.join("config.toml").display()
                    ),
                )
            })?;
        let host = parse_host(&host_raw, file.allow_http.unwrap_or(false))?;
        let ca_file = env("GSTN_CA_FILE")
            .filter(|c| !c.is_empty())
            .map(PathBuf::from)
            .or(file.ca_file.clone());
        let timeout = secs(
            "timeout_secs",
            env("GSTN_TIMEOUT").as_deref(),
            file.timeout_secs,
            DEFAULT_TIMEOUT_SECS,
        )?;
        let connect_timeout = secs(
            "connect_timeout_secs",
            None,
            file.connect_timeout_secs,
            DEFAULT_CONNECT_TIMEOUT_SECS,
        )?;
        Ok(Self {
            dir,
            host,
            ca_file,
            timeout,
            connect_timeout,
        })
    }

    /// The key a token is stored under: the origin without a trailing slash.
    pub fn host_key(&self) -> String {
        host_key(&self.host)
    }
}

pub fn host_key(url: &Url) -> String {
    url.as_str().trim_end_matches('/').to_string()
}

fn secs(key: &str, env: Option<&str>, file: Option<u64>, default: u64) -> Result<Duration> {
    let bad = || {
        ClientError::local(
            ErrorCode::CliConfigInvalid,
            format!("config_key={key} reason=invalid (1..={MAX_TIMEOUT_SECS} seconds)"),
        )
    };
    let v = match env.filter(|e| !e.is_empty()) {
        Some(e) => e.parse::<u64>().map_err(|_| bad())?,
        None => file.unwrap_or(default),
    };
    if v == 0 || v > MAX_TIMEOUT_SECS {
        return Err(bad());
    }
    Ok(Duration::from_secs(v))
}

fn read_file(path: &Path) -> Result<FileConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| {
            // Parse errors name the line, never the text (it could be pasted secrets).
            let at = e.span().map(|s| {
                text.get(..s.start)
                    .unwrap_or_default()
                    .matches('\n')
                    .count()
                    + 1
            });
            ClientError::local(
                ErrorCode::CliConfigInvalid,
                format!(
                    "config_key=config_file reason=parse path={} line={}",
                    path.display(),
                    at.map_or_else(|| "?".to_string(), |l| l.to_string())
                ),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(_) => Err(ClientError::local(
            ErrorCode::CliConfigInvalid,
            format!(
                "config_key=config_file reason=unreadable path={}",
                path.display()
            ),
        )),
    }
}
