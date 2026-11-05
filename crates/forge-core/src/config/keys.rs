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
    number(
        "server.body_read_timeout_ms",
        "FORGE_SERVER_BODY_READ_TIMEOUT_MS",
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
    // M1 phase 8b MR6: the AES-256-GCM keys of the opaque list cursors.
    text("api.cursor_keys_file", "FORGE_API_CURSOR_KEYS_FILE"),
    number("api.list_max_concurrent", "FORGE_API_LIST_MAX_CONCURRENT"),
    number(
        "api.list_max_concurrent_per_principal",
        "FORGE_API_LIST_MAX_CONCURRENT_PER_PRINCIPAL",
    ),
    number("api.cursor_max_age_s", "FORGE_API_CURSOR_MAX_AGE_S"),
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
    // M3 X8: the largest blob a push may bring (pre-receive push checks).
    number(
        "git.http.push_max_blob_bytes",
        "FORGE_GIT_HTTP_PUSH_MAX_BLOB_BYTES",
    ),
    number(
        "git.http.max_streams_per_principal",
        "FORGE_GIT_HTTP_MAX_STREAMS_PER_PRINCIPAL",
    ),
    // D5 MR3: how long a deleted project can be restored.
    number(
        "hosting.deletion_grace_days",
        "FORGE_HOSTING_DELETION_GRACE_DAYS",
    ),
    // D5 MR4: the period of the worker's purge task.
    number(
        "hosting.purge_interval_secs",
        "FORGE_HOSTING_PURGE_INTERVAL_SECS",
    ),
    // M3 R2: the mergeability fan-out cap (ADR-0024 §3).
    number(
        "review.mergeability_fanout_max",
        "FORGE_REVIEW_MERGEABILITY_FANOUT_MAX",
    ),
    // The retention of check runs (ADR-0024 O3 note).
    number(
        "review.check_run_retention_days",
        "FORGE_REVIEW_CHECK_RUN_RETENTION_DAYS",
    ),
    // M3 I1c: the feed's seal-sink comparison (ADR-0018 §5; false only in M3).
    boolean(
        "feed.require_sink_compare",
        "FORGE_FEED_REQUIRE_SINK_COMPARE",
    ),
    // M3 O3: the repository maintenance (ADR-0024 "実装で決めたこと（O3）").
    number(
        "maintenance.interval_days",
        "FORGE_MAINTENANCE_INTERVAL_DAYS",
    ),
    text("maintenance.window_start", "FORGE_MAINTENANCE_WINDOW_START"),
    text("maintenance.window_end", "FORGE_MAINTENANCE_WINDOW_END"),
    text(
        "maintenance.window_time_zone",
        "FORGE_MAINTENANCE_WINDOW_TIME_ZONE",
    ),
    number(
        "maintenance.prune_grace_hours",
        "FORGE_MAINTENANCE_PRUNE_GRACE_HOURS",
    ),
    number(
        "maintenance.step_timeout_secs",
        "FORGE_MAINTENANCE_STEP_TIMEOUT_SECS",
    ),
    number(
        "maintenance.fsck_interval_days",
        "FORGE_MAINTENANCE_FSCK_INTERVAL_DAYS",
    ),
    number("maintenance.batch_max", "FORGE_MAINTENANCE_BATCH_MAX"),
    number("maintenance.refs_warn", "FORGE_MAINTENANCE_REFS_WARN"),
    // M3 N1: e-mail notifications (ADR-0016 Amendment N1).
    boolean("notify.enabled", "FORGE_NOTIFY_ENABLED"),
    text("notify.smtp_host", "FORGE_NOTIFY_SMTP_HOST"),
    number("notify.smtp_port", "FORGE_NOTIFY_SMTP_PORT"),
    text("notify.smtp_tls", "FORGE_NOTIFY_SMTP_TLS"),
    text("notify.smtp_ca_file", "FORGE_NOTIFY_SMTP_CA_FILE"),
    text("notify.smtp_username", "FORGE_NOTIFY_SMTP_USERNAME"),
    text(
        "notify.smtp_password_file",
        "FORGE_NOTIFY_SMTP_PASSWORD_FILE",
    ),
    text("notify.from_address", "FORGE_NOTIFY_FROM_ADDRESS"),
    text("notify.from_name", "FORGE_NOTIFY_FROM_NAME"),
    text("notify.default_language", "FORGE_NOTIFY_DEFAULT_LANGUAGE"),
    number(
        "notify.connect_timeout_ms",
        "FORGE_NOTIFY_CONNECT_TIMEOUT_MS",
    ),
    number("notify.send_timeout_ms", "FORGE_NOTIFY_SEND_TIMEOUT_MS"),
    number("notify.max_attempts", "FORGE_NOTIFY_MAX_ATTEMPTS"),
    // M2 MR3: web sessions (ADR-0021 §5, §7, §12).
    text("web.base_url", "FORGE_WEB_BASE_URL"),
    text("web.session_key_file", "FORGE_WEB_SESSION_KEY_FILE"),
    number("web.session_idle_secs", "FORGE_WEB_SESSION_IDLE_SECS"),
    number(
        "web.session_absolute_secs",
        "FORGE_WEB_SESSION_ABSOLUTE_SECS",
    ),
    // M2 MR6: the period of the worker's session purge.
    number(
        "web.session_purge_interval_secs",
        "FORGE_WEB_SESSION_PURGE_INTERVAL_SECS",
    ),
    // M2 MR4: the OIDC sign-in (ADR-0021 §12).
    text("auth.oidc.issuer", "FORGE_AUTH_OIDC_ISSUER"),
    text("auth.oidc.client_id", "FORGE_AUTH_OIDC_CLIENT_ID"),
    text(
        "auth.oidc.client_secret_file",
        "FORGE_AUTH_OIDC_CLIENT_SECRET_FILE",
    ),
    text("auth.oidc.ca_file", "FORGE_AUTH_OIDC_CA_FILE"),
    text(
        "auth.oidc.employee_number_claim",
        "FORGE_AUTH_OIDC_EMPLOYEE_NUMBER_CLAIM",
    ),
    text("auth.oidc.access_role", "FORGE_AUTH_OIDC_ACCESS_ROLE"),
    text("auth.oidc.signing_algs", "FORGE_AUTH_OIDC_SIGNING_ALGS"),
    boolean("auth.oidc.auto_link", "FORGE_AUTH_OIDC_AUTO_LINK"),
    // M3 I2: webhooks (ADR-0016 §8).
    text("webhook.allowed_targets", "FORGE_WEBHOOK_ALLOWED_TARGETS"),
    boolean("webhook.allow_http", "FORGE_WEBHOOK_ALLOW_HTTP"),
    text("webhook.secret_keys_file", "FORGE_WEBHOOK_SECRET_KEYS_FILE"),
    text("webhook.ca_file", "FORGE_WEBHOOK_CA_FILE"),
    number(
        "webhook.delivery_retention_days",
        "FORGE_WEBHOOK_DELIVERY_RETENTION_DAYS",
    ),
];
