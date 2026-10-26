//! Every configuration key, its environment variable and value type.

/// The value type of a key: a TOML integer is accepted only for `Number`,
/// a TOML boolean only for `Bool` (environment values are always text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Number,
    Bool,
}

/// A configuration key, its environment variable, and its value type.
#[derive(Debug, Clone, Copy)]
pub struct Key {
    pub key: &'static str,
    pub env: &'static str,
    pub kind: Kind,
}

const fn text(key: &'static str, env: &'static str) -> Key {
    Key {
        key,
        env,
        kind: Kind::Text,
    }
}

const fn number(key: &'static str, env: &'static str) -> Key {
    Key {
        key,
        env,
        kind: Kind::Number,
    }
}

const fn boolean(key: &'static str, env: &'static str) -> Key {
    Key {
        key,
        env,
        kind: Kind::Bool,
    }
}

/// Every configuration key. `env` equals `env_name(key)` (tested).
pub const KEYS: &[Key] = &[
    text("server.bind", "FORGE_SERVER_BIND"),
    text("storage.root", "FORGE_STORAGE_ROOT"),
    text("database.url", "FORGE_DATABASE_URL"),
    text("database.password_file", "FORGE_DATABASE_PASSWORD_FILE"),
    text("database.tls_mode", "FORGE_DATABASE_TLS_MODE"),
    text("database.tls_root_cert", "FORGE_DATABASE_TLS_ROOT_CERT"),
    boolean(
        "database.tls_allow_unverified",
        "FORGE_DATABASE_TLS_ALLOW_UNVERIFIED",
    ),
    number("database.max_connections", "FORGE_DATABASE_MAX_CONNECTIONS"),
    number("database.min_connections", "FORGE_DATABASE_MIN_CONNECTIONS"),
    number(
        "database.connect_timeout_ms",
        "FORGE_DATABASE_CONNECT_TIMEOUT_MS",
    ),
    number(
        "database.acquire_timeout_ms",
        "FORGE_DATABASE_ACQUIRE_TIMEOUT_MS",
    ),
    number(
        "database.statement_timeout_ms",
        "FORGE_DATABASE_STATEMENT_TIMEOUT_MS",
    ),
    number("database.lock_timeout_ms", "FORGE_DATABASE_LOCK_TIMEOUT_MS"),
    number(
        "database.idle_in_transaction_timeout_ms",
        "FORGE_DATABASE_IDLE_IN_TRANSACTION_TIMEOUT_MS",
    ),
    number(
        "database.migrate_statement_timeout_ms",
        "FORGE_DATABASE_MIGRATE_STATEMENT_TIMEOUT_MS",
    ),
];
