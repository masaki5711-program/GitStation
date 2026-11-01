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
    // M1 phase 9 MR0: connection fairness.
    number("server.max_connections", "FORGE_SERVER_MAX_CONNECTIONS"),
    number(
        "server.max_connections_per_peer",
        "FORGE_SERVER_MAX_CONNECTIONS_PER_PEER",
    ),
    number(
        "server.header_read_timeout_ms",
        "FORGE_SERVER_HEADER_READ_TIMEOUT_MS",
    ),
    number("server.keep_alive_max_ms", "FORGE_SERVER_KEEP_ALIVE_MAX_MS"),
    number(
        "server.keep_alive_max_requests",
        "FORGE_SERVER_KEEP_ALIVE_MAX_REQUESTS",
    ),
    number(
        "server.write_idle_timeout_ms",
        "FORGE_SERVER_WRITE_IDLE_TIMEOUT_MS",
    ),
    number(
        "server.read_idle_timeout_ms",
        "FORGE_SERVER_READ_IDLE_TIMEOUT_MS",
    ),
    text("storage.root", "FORGE_STORAGE_ROOT"),
    text("database.url", "FORGE_DATABASE_URL"),
    text("database.password_file", "FORGE_DATABASE_PASSWORD_FILE"),
    text("database.migrate_url", "FORGE_DATABASE_MIGRATE_URL"),
    text(
        "database.migrate_password_file",
        "FORGE_DATABASE_MIGRATE_PASSWORD_FILE",
    ),
    text("database.maint_url", "FORGE_DATABASE_MAINT_URL"),
    text(
        "database.maint_password_file",
        "FORGE_DATABASE_MAINT_PASSWORD_FILE",
    ),
    text(
        "database.privileged_members",
        "FORGE_DATABASE_PRIVILEGED_MEMBERS",
    ),
    text("database.tls_mode", "FORGE_DATABASE_TLS_MODE"),
    text("database.tls_root_cert", "FORGE_DATABASE_TLS_ROOT_CERT"),
    boolean(
        "database.tls_allow_unverified",
        "FORGE_DATABASE_TLS_ALLOW_UNVERIFIED",
    ),
    number(
        "database.write_max_connections",
        "FORGE_DATABASE_WRITE_MAX_CONNECTIONS",
    ),
    number(
        "database.read_max_connections",
        "FORGE_DATABASE_READ_MAX_CONNECTIONS",
    ),
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
    // C8: the HMAC keys of the audit seal (ADR-0018 §4).
    text("audit.seal_keys_file", "FORGE_AUDIT_SEAL_KEYS_FILE"),
    // M1 phase 9 MR3: Smart HTTP limits (ADR-0007 amendment).
    number(
        "git.http.fetch_request_max_bytes",
        "FORGE_GIT_HTTP_FETCH_REQUEST_MAX_BYTES",
    ),
    number("git.http.idle_timeout_ms", "FORGE_GIT_HTTP_IDLE_TIMEOUT_MS"),
    number("git.http.timeout_ms", "FORGE_GIT_HTTP_TIMEOUT_MS"),
    number("git.http.body_timeout_ms", "FORGE_GIT_HTTP_BODY_TIMEOUT_MS"),
    number(
        "git.http.max_object_wants",
        "FORGE_GIT_HTTP_MAX_OBJECT_WANTS",
    ),
    number(
        "git.http.want_check_timeout_ms",
        "FORGE_GIT_HTTP_WANT_CHECK_TIMEOUT_MS",
    ),
    number("git.http.max_streams", "FORGE_GIT_HTTP_MAX_STREAMS"),
    // M1 phase 9 MR4: the push size limit (receive.maxInputSize).
    number("git.http.push_max_bytes", "FORGE_GIT_HTTP_PUSH_MAX_BYTES"),
    number(
        "git.http.max_streams_per_principal",
        "FORGE_GIT_HTTP_MAX_STREAMS_PER_PRINCIPAL",
    ),
];
