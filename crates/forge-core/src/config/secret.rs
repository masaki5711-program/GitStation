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
/// owner on Unix (on Windows the ACL is the operator's responsibility,
/// see the config-invalid runbook), with the trailing newline trimmed.
pub(super) fn read_password(path: &Path) -> Result<Secret, ForgeError> {
    const KEY: &str = "database.password_file";
    if !path.is_absolute() {
        return Err(invalid(KEY, "not_absolute"));
    }
    let unreadable = |e: std::io::Error| {
        invalid(KEY, "unreadable")
            .with_context("path", path.display())
            .with_source(e)
    };
    let meta = std::fs::metadata(path).map_err(unreadable)?;
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
    #[cfg(not(unix))]
    let _ = &meta; // only the existence check applies here
    let text = std::fs::read_to_string(path).map_err(unreadable)?;
    let value = text.trim_end_matches(['\r', '\n']);
    if value.is_empty() {
        return Err(invalid(KEY, "empty"));
    }
    Ok(Secret(value.to_owned()))
}
