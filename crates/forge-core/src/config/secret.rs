//! Values that must never be printed: the database password and the URL
//! that would otherwise carry it.

use std::path::Path;

use forge_errors::ForgeError;

use super::invalid;

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

/// Query parameters through which libpq accepts a password or its file.
const PASSWORD_PARAMS: &[&str] = &["password", "passfile", "sslpassword"];

/// Query parameters that would replace the host, port, database or user
/// written in the URL itself.
const TARGET_PARAMS: &[&str] = &["host", "hostaddr", "port", "dbname", "user", "service"];

/// Upper bound for the password file (a mistaken `/dev/zero` must not
/// exhaust memory).
const MAX_PASSWORD_BYTES: u64 = 4096;

/// A validated `postgres://` URL. It never carries a password: the password
/// comes only from `database.password_file` (ADR-0013).
pub struct DatabaseUrl {
    url: url::Url,
    host: String,
    port: u16,
    dbname: String,
}

impl DatabaseUrl {
    /// Parse and validate. The raw text is never echoed in the error.
    pub(super) fn parse(raw: &str) -> Result<Self, ForgeError> {
        let bad = || invalid("database.url", "invalid");
        let url = url::Url::parse(raw).map_err(|_| bad())?;
        if !matches!(url.scheme(), "postgres" | "postgresql") {
            return Err(bad());
        }
        let has_password_param = url
            .query_pairs()
            .any(|(k, _)| PASSWORD_PARAMS.iter().any(|p| k.eq_ignore_ascii_case(p)));
        if url.password().is_some() || has_password_param {
            return Err(invalid("database.url", "password_in_url"));
        }
        // The driver prefers these over the URL's own parts: the logged
        // target would no longer be the one connected to.
        let overrides_target = url
            .query_pairs()
            .any(|(k, _)| TARGET_PARAMS.iter().any(|p| k.eq_ignore_ascii_case(p)));
        if overrides_target {
            return Err(invalid("database.url", "target_in_query"));
        }
        // TLS is configured only by database.tls_mode / tls_root_cert, so a
        // URL cannot quietly downgrade it.
        let sets_tls = url
            .query_pairs()
            .any(|(k, _)| k.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("ssl")));
        if sets_tls {
            return Err(invalid("database.url", "tls_in_query"));
        }
        let host = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(bad)?
            .to_owned();
        let dbname = url.path().trim_start_matches('/');
        if dbname.is_empty() || dbname.contains('/') {
            return Err(bad());
        }
        let dbname = percent_encoding::percent_decode_str(dbname)
            .decode_utf8()
            .map_err(|_| bad())?
            .into_owned();
        let port = url.port().unwrap_or(5432);
        Ok(Self {
            url,
            host,
            port,
            dbname,
        })
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

    /// The URL for the driver (no password; that is passed separately).
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }
}

/// Logs show where, never how: host, port and database only.
impl std::fmt::Display for DatabaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}/{}", self.host, self.port, self.dbname)
    }
}

impl std::fmt::Debug for DatabaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DatabaseUrl({self})")
    }
}

/// Read `database.password_file`: an absolute path, readable only by its
/// owner on Unix (production runs on Linux; on Windows the ACL is the
/// operator's responsibility, ADR-0013), at most 4 KiB, with the trailing
/// newline trimmed. The file is opened once; the permission check and the
/// read use the same handle.
pub(super) fn read_password(path: &Path) -> Result<Secret, ForgeError> {
    use std::io::Read as _;
    const KEY: &str = "database.password_file";
    if !path.is_absolute() {
        return Err(invalid(KEY, "not_absolute"));
    }
    let unreadable = |e: std::io::Error| {
        invalid(KEY, "unreadable")
            .with_context("path", path.display())
            .with_source(e)
    };
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let meta = file.metadata().map_err(unreadable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(invalid(KEY, "insecure_permissions")
                .with_context("path", path.display())
                .with_context("mode", format!("{mode:o}")));
        }
    }
    if !meta.is_file() || meta.len() > MAX_PASSWORD_BYTES {
        return Err(invalid(KEY, "invalid").with_context("path", path.display()));
    }
    let mut text = String::new();
    // `take` also bounds a file that grows after the size check.
    file.take(MAX_PASSWORD_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(unreadable)?;
    if text.len() as u64 > MAX_PASSWORD_BYTES {
        return Err(invalid(KEY, "invalid").with_context("path", path.display()));
    }
    let value = text.trim_end_matches(['\r', '\n']);
    if value.is_empty() {
        return Err(invalid(KEY, "empty"));
    }
    Ok(Secret(value.to_owned()))
}
