//! Typed configuration. Layers, lowest first: built-in defaults ← TOML file
//! (`--config` / `FORGE_CONFIG`) ← environment ← command-line flags (applied
//! by the caller). The environment name of a key is derived from it:
//! `database.max_connections` ↔ `FORGE_DATABASE_MAX_CONNECTIONS`.
//!
//! Every problem is `FG-SYS-0002` with `config_key` and `reason`, detected at
//! load time. Unknown keys fail (a typo never silently becomes a default);
//! secrets are passed as files (`database.password_file`) and never appear in
//! `Debug` / `Display` output.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

/// A configuration key and its environment variable.
#[derive(Debug, Clone, Copy)]
pub struct Key {
    pub key: &'static str,
    pub env: &'static str,
}

const fn key(key: &'static str, env: &'static str) -> Key {
    Key { key, env }
}

/// Every configuration key. `env` equals `env_name(key)` (tested).
pub const KEYS: &[Key] = &[
    key("server.bind", "FORGE_SERVER_BIND"),
    key("storage.root", "FORGE_STORAGE_ROOT"),
    key("database.url", "FORGE_DATABASE_URL"),
    key("database.password_file", "FORGE_DATABASE_PASSWORD_FILE"),
    key("database.max_connections", "FORGE_DATABASE_MAX_CONNECTIONS"),
    key("database.min_connections", "FORGE_DATABASE_MIN_CONNECTIONS"),
    key(
        "database.connect_timeout_ms",
        "FORGE_DATABASE_CONNECT_TIMEOUT_MS",
    ),
    key(
        "database.acquire_timeout_ms",
        "FORGE_DATABASE_ACQUIRE_TIMEOUT_MS",
    ),
    key(
        "database.statement_timeout_ms",
        "FORGE_DATABASE_STATEMENT_TIMEOUT_MS",
    ),
    key("database.lock_timeout_ms", "FORGE_DATABASE_LOCK_TIMEOUT_MS"),
    key(
        "database.idle_in_transaction_timeout_ms",
        "FORGE_DATABASE_IDLE_IN_TRANSACTION_TIMEOUT_MS",
    ),
    key(
        "database.migrate_statement_timeout_ms",
        "FORGE_DATABASE_MIGRATE_STATEMENT_TIMEOUT_MS",
    ),
];

/// Environment prefixes owned by the configuration: an unknown variable
/// under one of them is a typo. Other `FORGE_*` variables (for example the
/// ones forge passes to git hooks) are not configuration and are ignored.
const NAMESPACES: &[&str] = &["FORGE_SERVER_", "FORGE_STORAGE_", "FORGE_DATABASE_"];

/// The environment variable of a configuration key.
pub fn env_name(key: &str) -> String {
    format!("FORGE_{}", key.replace('.', "_").to_ascii_uppercase())
}

fn invalid(config_key: &str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", config_key)
        .with_context("reason", reason)
}

/// A snapshot of the `FORGE_*` environment variables.
#[derive(Debug, Default)]
pub struct Env {
    vars: BTreeMap<String, String>,
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
    NAMESPACES.iter().any(|ns| name.starts_with(ns))
}

/// A secret value: never printed.
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// A validated `postgres://` URL without its password (the password, if the
/// URL carried one, is moved into `DatabaseConfig::password`).
pub struct DatabaseUrl {
    url: url::Url,
    host: String,
    port: u16,
    dbname: String,
}

impl DatabaseUrl {
    /// Parse and split off the password. The raw text is never echoed in the
    /// error: it may contain the password.
    fn parse(raw: &str) -> Result<(Self, Option<Secret>), ForgeError> {
        let bad = || invalid("database.url", "invalid");
        let mut url = url::Url::parse(raw).map_err(|_| bad())?;
        if !matches!(url.scheme(), "postgres" | "postgresql") {
            return Err(bad());
        }
        let host = url.host_str().filter(|h| !h.is_empty()).ok_or_else(bad)?;
        let host = host.to_owned();
        let dbname = url.path().trim_start_matches('/');
        if dbname.is_empty() || dbname.contains('/') {
            return Err(bad());
        }
        let dbname = percent_encoding::percent_decode_str(dbname)
            .decode_utf8()
            .map_err(|_| bad())?
            .into_owned();
        let password = match url.password() {
            Some(p) => Some(Secret(
                percent_encoding::percent_decode_str(p)
                    .decode_utf8()
                    .map_err(|_| bad())?
                    .into_owned(),
            )),
            None => None,
        };
        url.set_password(None).map_err(|()| bad())?;
        let port = url.port().unwrap_or(5432);
        Ok((
            Self {
                url,
                host,
                port,
                dbname,
            },
            password,
        ))
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn dbname(&self) -> &str {
        &self.dbname
    }

    /// The URL for the driver; the password is passed separately.
    pub fn without_password(&self) -> String {
        self.url.to_string()
    }
}

impl std::fmt::Display for DatabaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.url.as_str())
    }
}

impl std::fmt::Debug for DatabaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DatabaseUrl({})", self.url)
    }
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
    pub bind: String,
}

#[derive(Debug)]
pub struct StorageConfig {
    pub root: Option<PathBuf>,
}

#[derive(Debug)]
pub struct DatabaseConfig {
    url: Option<DatabaseUrl>,
    pub password: Option<Secret>,
    pub max_connections: u32,
    pub min_connections: u32,
    pub connect_timeout: Duration,
    pub acquire_timeout: Duration,
    pub statement_timeout: Duration,
    pub lock_timeout: Duration,
    pub idle_in_transaction_timeout: Duration,
    /// `None` = no statement timeout while migrating (`lock_timeout` still applies).
    pub migrate_statement_timeout: Option<Duration>,
}

impl DatabaseConfig {
    /// The database URL; required by commands that use the database.
    pub fn url(&self) -> Result<&DatabaseUrl, ForgeError> {
        self.url
            .as_ref()
            .ok_or_else(|| invalid("database.url", "missing"))
    }
}

/// Load the configuration from the optional file and the environment.
pub fn load(file: Option<&Path>, env: &Env) -> Result<Config, ForgeError> {
    let mut values = match file {
        Some(path) => read_file(path)?,
        None => BTreeMap::new(),
    };
    for (name, value) in &env.vars {
        match KEYS.iter().find(|k| k.env == name) {
            Some(k) => {
                values.insert(k.key, value.clone()); // clone: env stays a snapshot
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
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        invalid("config_file", "parse")
            .with_context("path", path.display())
            .with_source(e)
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
                toml::Value::String(s) => s.clone(), // clone: the table is dropped after
                toml::Value::Integer(i) => i.to_string(),
                toml::Value::Float(_)
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

fn build(values: &BTreeMap<&'static str, String>) -> Result<Config, ForgeError> {
    let get = |key: &str| values.get(key).map(String::as_str);

    let (url, url_password) = match get("database.url") {
        Some(raw) => {
            let (url, password) = DatabaseUrl::parse(raw)?;
            (Some(url), password)
        }
        None => (None, None),
    };
    let password = match (get("database.password_file"), url_password) {
        (Some(_), Some(_)) => return Err(invalid("database.password_file", "password_twice")),
        (Some(path), None) => Some(read_password(Path::new(path))?),
        (None, inline) => inline,
    };

    let max_connections = positive(get, "database.max_connections", 10)?;
    let min_connections = number(get, "database.min_connections", 0)?;
    if min_connections > max_connections {
        return Err(invalid("database.min_connections", "above_max"));
    }
    let migrate_ms = number(get, "database.migrate_statement_timeout_ms", 0)?;

    Ok(Config {
        server: ServerConfig {
            bind: get("server.bind").unwrap_or("127.0.0.1:8080").to_owned(),
        },
        storage: StorageConfig {
            root: get("storage.root").map(PathBuf::from),
        },
        database: DatabaseConfig {
            url,
            password,
            max_connections,
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
            migrate_statement_timeout: (migrate_ms > 0)
                .then(|| Duration::from_millis(u64::from(migrate_ms))),
        },
    })
}

fn read_password(path: &Path) -> Result<Secret, ForgeError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        invalid("database.password_file", "unreadable")
            .with_context("path", path.display())
            .with_source(e)
    })?;
    let value = text.trim_end_matches(['\r', '\n']);
    if value.is_empty() {
        return Err(invalid("database.password_file", "empty"));
    }
    Ok(Secret(value.to_owned()))
}

/// A non-negative integer (`u32`), or `default` when unset.
fn number<'a>(
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
    match number(get, key, default)? {
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
