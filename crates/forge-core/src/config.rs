//! Typed configuration. Layers, lowest first: built-in defaults ← TOML file
//! (`--config` / `FORGE_CONFIG`) ← environment ← command-line flags (applied
//! by the caller). The environment name of a key is derived from it:
//! `database.write_max_connections` ↔ `FORGE_DATABASE_WRITE_MAX_CONNECTIONS`.
//!
//! Every problem is `FG-SYS-0002` with `config_key` and `reason`, detected at
//! load time. Unknown, renamed and empty keys fail (a typo never silently
//! becomes a default); the database password comes only from
//! `database.password_file` and is never printed (ADR-0013).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

mod database;
mod keys;
mod secret;
mod tls;

pub use database::{DatabaseConfig, PrivilegedMember, PRIVILEGED_ROLES};
pub use keys::{Key, Kind, KEYS};
pub use secret::{DatabaseUrl, Secret};
pub use tls::TlsMode;

/// Environment prefixes owned by the configuration: an unknown variable
/// under one of them is a typo. Other `FORGE_*` variables (for example the
/// ones forge passes to git hooks) are not configuration and are ignored.
const NAMESPACES: &[&str] = &["FORGE_SERVER_", "FORGE_STORAGE_", "FORGE_DATABASE_"];

/// Variables that used to be configuration: setting one fails instead of
/// being ignored (old name, new name).
const RENAMED: &[(&str, &str)] = &[
    ("FORGE_BIND", "FORGE_SERVER_BIND"),
    // Split into a write and a read pool (ADR-0018 §2).
    (
        "FORGE_DATABASE_MAX_CONNECTIONS",
        "FORGE_DATABASE_WRITE_MAX_CONNECTIONS",
    ),
];

/// The environment variable of a configuration key.
pub fn env_name(key: &str) -> String {
    format!("FORGE_{}", key.replace('.', "_").to_ascii_uppercase())
}

fn invalid(config_key: &str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", config_key)
        .with_context("reason", reason)
}

/// A snapshot of the `FORGE_*` environment variables. `Debug` shows the
/// names only: a value may be a URL or a path an operator considers private.
#[derive(Default)]
pub struct Env {
    vars: BTreeMap<String, String>,
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.vars.keys()).finish()
    }
}

impl Env {
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            vars: pairs
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        }
    }

    /// Read the process environment. A non-UTF-8 value of a configuration
    /// variable is an error, not an absent value.
    pub fn from_process() -> Result<Self, ForgeError> {
        let mut vars = BTreeMap::new();
        for (name, value) in std::env::vars_os() {
            // Configuration names are ASCII; a non-UTF-8 name is not ours.
            let Some(name) = name.to_str().filter(|n| n.starts_with("FORGE_")) else {
                continue;
            };
            match value.into_string() {
                Ok(value) => {
                    vars.insert(name.to_owned(), value);
                }
                Err(_) if is_config_env(name) => return Err(invalid(name, "not_unicode")),
                // Not configuration (see NAMESPACES): nothing reads it here.
                Err(_) => {}
            }
        }
        Ok(Self { vars })
    }
}

fn is_config_env(name: &str) -> bool {
    NAMESPACES.iter().any(|ns| name.starts_with(ns)) || RENAMED.iter().any(|(old, _)| *old == name)
}

#[derive(Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub database: DatabaseConfig,
}

impl Config {
    /// The storage root; required by commands that touch repositories.
    pub fn storage_root(&self) -> Result<&Path, ForgeError> {
        self.storage
            .root
            .as_deref()
            .ok_or_else(|| invalid("storage.root", "missing"))
    }
}

#[derive(Debug)]
pub struct ServerConfig {
    /// `host:port`, validated at load time.
    pub bind: String,
}

#[derive(Debug)]
pub struct StorageConfig {
    pub root: Option<PathBuf>,
}

/// Load the configuration from the optional file and the environment.
pub fn load(file: Option<&Path>, env: &Env) -> Result<Config, ForgeError> {
    let mut values = match file {
        Some(path) => read_file(path)?,
        None => BTreeMap::new(),
    };
    for (name, value) in &env.vars {
        if let Some((_, new)) = RENAMED.iter().find(|(old, _)| old == name) {
            return Err(invalid(name, "renamed").with_context("renamed_to", new));
        }
        match KEYS.iter().find(|k| k.env == name) {
            Some(k) => {
                values.insert(k.key, value.as_str().into());
            }
            None if is_config_env(name) => return Err(invalid(name, "unknown_env")),
            None => {}
        }
    }
    build(&values)
}

/// Flatten `[section] key = value` into `section.key → text`.
fn read_file(path: &Path) -> Result<BTreeMap<&'static str, String>, ForgeError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        invalid("config_file", "unreadable")
            .with_context("path", path.display())
            .with_source(e)
    })?;
    // The TOML error text quotes the offending line, which may hold a
    // secret: only its position is kept.
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        let (line, column) = position(&text, e.span().map_or(0, |s| s.start));
        invalid("config_file", "parse")
            .with_context("path", path.display())
            .with_context("line", line)
            .with_context("column", column)
    })?;
    let mut values = BTreeMap::new();
    for (section, body) in &table {
        let Some(body) = body.as_table() else {
            return Err(invalid(section, "unknown_key"));
        };
        for (name, value) in body {
            let full = format!("{section}.{name}");
            let Some(k) = KEYS.iter().find(|k| k.key == full) else {
                return Err(invalid(&full, "unknown_key"));
            };
            let text = match value {
                toml::Value::String(s) => s.as_str().into(),
                toml::Value::Integer(i) if k.kind == Kind::Number => i.to_string(),
                toml::Value::Boolean(b) if k.kind == Kind::Bool => b.to_string(),
                toml::Value::Integer(_)
                | toml::Value::Float(_)
                | toml::Value::Boolean(_)
                | toml::Value::Datetime(_)
                | toml::Value::Array(_)
                | toml::Value::Table(_) => return Err(invalid(k.key, "invalid")),
            };
            values.insert(k.key, text);
        }
    }
    Ok(values)
}

/// 1-based line and column of a byte offset.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let before = text.get(..offset).unwrap_or(text);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

fn build(values: &BTreeMap<&'static str, String>) -> Result<Config, ForgeError> {
    let get = |key: &str| values.get(key).map(String::as_str);
    // A set-but-empty value (an exported, unfilled template variable) is an
    // error, never "unset".
    let text = |key: &'static str| match get(key).map(str::trim) {
        Some("") => Err(invalid(key, "empty")),
        other => Ok(other),
    };

    let bind = text("server.bind")?.unwrap_or("127.0.0.1:8080");
    if !valid_bind(bind) {
        return Err(invalid("server.bind", "invalid"));
    }
    let url = |key: &'static str| text(key)?.map(|u| DatabaseUrl::parse(key, u)).transpose();
    let password = |key: &'static str| {
        text(key)?
            .map(|p| secret::read_password(key, Path::new(p)))
            .transpose()
    };
    let password_path = |key: &'static str| {
        text(key)?
            .map(|p| database::password_path(key, p))
            .transpose()
    };
    let privileged_members = match text("database.privileged_members")? {
        Some(list) => database::privileged_members("database.privileged_members", list)?,
        None => Vec::new(),
    };

    let tls_mode = match text("database.tls_mode")? {
        Some(t) => TlsMode::parse(t).ok_or_else(|| invalid("database.tls_mode", "invalid"))?,
        None => TlsMode::VerifyFull,
    };
    let tls_allow_unverified = match text("database.tls_allow_unverified")? {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(invalid("database.tls_allow_unverified", "invalid")),
    };
    let tls_root_cert = text("database.tls_root_cert")?.map(PathBuf::from);
    if tls_root_cert.as_deref().is_some_and(|p| !p.is_absolute()) {
        return Err(invalid("database.tls_root_cert", "not_absolute"));
    }

    let write_max_connections = positive(get, "database.write_max_connections", 10)?;
    let read_max_connections = positive(get, "database.read_max_connections", 10)?;
    let min_connections = unsigned(get, "database.min_connections", 0)?;
    if min_connections > write_max_connections.min(read_max_connections) {
        return Err(invalid("database.min_connections", "above_max"));
    }

    Ok(Config {
        server: ServerConfig {
            bind: bind.to_owned(),
        },
        storage: StorageConfig {
            root: text("storage.root")?.map(PathBuf::from),
        },
        database: DatabaseConfig {
            url: url("database.url")?,
            password: password("database.password_file")?,
            migrate_url: url("database.migrate_url")?,
            migrate_password_file: password_path("database.migrate_password_file")?,
            maint_url: url("database.maint_url")?,
            maint_password_file: password_path("database.maint_password_file")?,
            privileged_members,
            tls_mode,
            tls_root_cert,
            tls_allow_unverified,
            write_max_connections,
            read_max_connections,
            min_connections,
            connect_timeout: millis(get, "database.connect_timeout_ms", 5_000)?,
            acquire_timeout: millis(get, "database.acquire_timeout_ms", 3_000)?,
            statement_timeout: millis(get, "database.statement_timeout_ms", 15_000)?,
            lock_timeout: millis(get, "database.lock_timeout_ms", 5_000)?,
            idle_in_transaction_timeout: millis(
                get,
                "database.idle_in_transaction_timeout_ms",
                60_000,
            )?,
            migrate_statement_timeout: millis(
                get,
                "database.migrate_statement_timeout_ms",
                600_000,
            )?,
        },
    })
}

/// `host:port` with a non-empty host and a numeric port (`[::1]:8080` too).
fn valid_bind(bind: &str) -> bool {
    bind.rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
}

/// A non-negative integer (`u32`), or `default` when unset.
fn unsigned<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    key: &'static str,
    default: u32,
) -> Result<u32, ForgeError> {
    match get(key) {
        Some(text) => text.trim().parse().map_err(|_| invalid(key, "invalid")),
        None => Ok(default),
    }
}

fn positive<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    key: &'static str,
    default: u32,
) -> Result<u32, ForgeError> {
    match unsigned(get, key, default)? {
        0 => Err(invalid(key, "zero")),
        n => Ok(n),
    }
}

fn millis<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    key: &'static str,
    default: u32,
) -> Result<Duration, ForgeError> {
    positive(get, key, default).map(|ms| Duration::from_millis(u64::from(ms)))
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
