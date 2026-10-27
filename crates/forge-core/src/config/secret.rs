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

/// Query parameters through which libpq accepts a password or its file
/// (reported as `password_in_url`, so the operator knows where it goes).
const PASSWORD_PARAMS: &[&str] = &["password", "passfile", "sslpassword"];

/// Upper bound for the password file (a mistaken `/dev/zero` must not
/// exhaust memory).
const MAX_PASSWORD_BYTES: u64 = 4096;

/// A validated `postgres://user@host[:port]/dbname` URL. It carries no
/// password (only `database.password_file`, ADR-0013) and no query: TLS,
/// timeouts and the application name come from the configuration, and the
/// driver would log an unknown parameter's value verbatim.
pub struct DatabaseUrl {
    host: String,
    port: u16,
    user: String,
    dbname: String,
}

impl DatabaseUrl {
    /// Parse and validate. The raw text is never echoed in the error.
    /// `key` names the configuration key in the error.
    pub(super) fn parse(key: &'static str, raw: &str) -> Result<Self, ForgeError> {
        let bad = || invalid(key, "invalid");
        let url = url::Url::parse(raw).map_err(|_| bad())?;
        if !matches!(url.scheme(), "postgres" | "postgresql") {
            return Err(bad());
        }
        let has_password_param = url
            .query_pairs()
            .any(|(k, _)| PASSWORD_PARAMS.iter().any(|p| k.eq_ignore_ascii_case(p)));
        if url.password().is_some() || has_password_param {
            return Err(invalid(key, "password_in_url"));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(invalid(key, "query_in_url"));
        }
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .map(std::borrow::Cow::into_owned)
                .map_err(|_| bad())
        };
        let host = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or_else(bad)?
            .to_owned();
        // The user is required: otherwise the driver falls back to PGUSER or
        // the operating-system account.
        let user = decode(url.username())?;
        if user.is_empty() {
            return Err(bad());
        }
        let dbname = url.path().trim_start_matches('/');
        if dbname.is_empty() || dbname.contains('/') {
            return Err(bad());
        }
        let dbname = decode(dbname)?;
        let port = url.port().unwrap_or(5432);
        Ok(Self {
            host,
            port,
            user,
            dbname,
        })
    }

    pub fn host(&self) -> &str {
        // IPv6 literals keep their brackets in the URL; the driver wants none.
        self.host.trim_matches(['[', ']'])
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn dbname(&self) -> &str {
        &self.dbname
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

/// Read a password file (`database.password_file` and the other
/// `*_password_file` keys, named by `key`): an absolute path, readable only by its
/// owner on Unix (production runs on Linux; on Windows the ACL is the
/// operator's responsibility, ADR-0013), at most 4 KiB, with the trailing
/// newline trimmed. The file is opened once; the permission check and the
/// read use the same handle.
pub(super) fn read_password(key: &'static str, path: &Path) -> Result<Secret, ForgeError> {
    use std::io::Read as _;
    if !path.is_absolute() {
        return Err(invalid(key, "not_absolute"));
    }
    let unreadable = |e: std::io::Error| {
        invalid(key, "unreadable")
            .with_context("path", path.display())
            .with_source(e)
    };
    // Opening a FIFO blocks until a writer appears: refuse anything but a
    // regular file before opening; the handle's metadata is checked again
    // below (the path may have changed in between).
    if !std::fs::metadata(path).map_err(unreadable)?.is_file() {
        return Err(invalid(key, "invalid").with_context("path", path.display()));
    }
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let meta = file.metadata().map_err(unreadable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(invalid(key, "insecure_permissions")
                .with_context("path", path.display())
                .with_context("mode", format!("{mode:o}")));
        }
    }
    if !meta.is_file() || meta.len() > MAX_PASSWORD_BYTES {
        return Err(invalid(key, "invalid").with_context("path", path.display()));
    }
    let mut text = String::new();
    // `take` also bounds a file that grows after the size check.
    file.take(MAX_PASSWORD_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(unreadable)?;
    if text.len() as u64 > MAX_PASSWORD_BYTES {
        return Err(invalid(key, "invalid").with_context("path", path.display()));
    }
    let value = text.trim_end_matches(['\r', '\n']);
    if value.is_empty() {
        return Err(invalid(key, "empty"));
    }
    Ok(Secret(value.to_owned()))
}
