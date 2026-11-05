//! `[notify]`: e-mail notifications (M3 N1, plan 11 §5 N1 and §10 Q6,
//! ADR-0016 Amendment N1).
//!
//! The internal SMTP relay's details were not known when this was written
//! (Q6), so every one of them is a key, and nothing is sent until
//! `notify.enabled = true` with a relay named.
//!
//! | key | default | rule |
//! |---|---|---|
//! | `notify.enabled` | `false` | the subscriber writes notifications and the worker sends them only when `true` |
//! | `notify.smtp_host` | — | required when enabled; a host name or an IP address |
//! | `notify.smtp_port` | 587 (`starttls`), 465 (`tls`), 25 (`none`) | 1 ..= 65535 |
//! | `notify.smtp_tls` | `starttls` | `none` / `starttls` / `tls` (implicit) |
//! | `notify.smtp_ca_file` | — | absolute path of the PEM roots that verify the relay; only these are trusted (no public roots); needs `starttls` / `tls` |
//! | `notify.smtp_username` + `notify.smtp_password_file` | — | both or neither (AUTH PLAIN); never with `none` (no password in the clear) |
//! | `notify.from_address` | — | required when enabled; a plain `local@domain` address |
//! | `notify.from_name` | `GitStation` | at most 64 characters, no control characters |
//! | `notify.default_language` | `ja` | `ja` / `en` / `id`, for people who chose none |
//! | `notify.connect_timeout_ms` | 5 000 | 100 ..= 60 000 |
//! | `notify.send_timeout_ms` | 30 000 | the whole conversation; `connect_timeout_ms` ..= 300 000 |
//! | `notify.max_attempts` | 5 | 1 ..= 25 tries per notification (the job queue's backoff, 10 s doubling) |
//!
//! Every problem is `FG-SYS-0002` with `config_key` and `reason`. The
//! password file is read at load (a bad one stops the start) and again when
//! the worker builds its sender, so a rotation takes effect at a restart.

use std::path::PathBuf;
use std::time::Duration;

use forge_errors::{ForgeError, Lang};

use super::secret::read_password;
use super::{invalid, unsigned, Secret};

/// How the connection to the relay is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpTls {
    /// Plain text (an internal relay on a trusted segment; no AUTH allowed).
    None,
    /// Plain connect, then STARTTLS, required (never downgraded).
    StartTls,
    /// TLS from the first byte (SMTPS).
    Tls,
}

/// The relay and the sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub tls: SmtpTls,
    pub ca_file: Option<PathBuf>,
    pub username: Option<String>,
    pub password_file: Option<PathBuf>,
    pub from_address: String,
    pub from_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyConfig {
    pub enabled: bool,
    /// `Some` whenever `enabled`.
    pub smtp: Option<SmtpConfig>,
    pub default_language: Lang,
    pub connect_timeout: Duration,
    pub send_timeout: Duration,
    pub max_attempts: u8,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            smtp: None,
            default_language: Lang::Ja,
            connect_timeout: Duration::from_millis(5_000),
            send_timeout: Duration::from_millis(30_000),
            max_attempts: 5,
        }
    }
}

const ENABLED_KEY: &str = "notify.enabled";
const HOST_KEY: &str = "notify.smtp_host";
const PORT_KEY: &str = "notify.smtp_port";
const TLS_KEY: &str = "notify.smtp_tls";
const CA_KEY: &str = "notify.smtp_ca_file";
const USER_KEY: &str = "notify.smtp_username";
const PASSWORD_KEY: &str = "notify.smtp_password_file";
const FROM_KEY: &str = "notify.from_address";
const FROM_NAME_KEY: &str = "notify.from_name";
const LANGUAGE_KEY: &str = "notify.default_language";
const CONNECT_KEY: &str = "notify.connect_timeout_ms";
const SEND_KEY: &str = "notify.send_timeout_ms";
const ATTEMPTS_KEY: &str = "notify.max_attempts";

/// A non-empty trimmed value; a set-but-empty one is an error, never "unset".
fn text<'a>(
    get: &impl Fn(&str) -> Option<&'a str>,
    key: &'static str,
) -> Result<Option<&'a str>, ForgeError> {
    match get(key).map(str::trim) {
        Some("") => Err(invalid(key, "empty")),
        other => Ok(other),
    }
}

fn ranged_u32<'a>(
    get: &impl Fn(&str) -> Option<&'a str>,
    key: &'static str,
    default: u32,
    (min, max): (u32, u32),
) -> Result<u32, ForgeError> {
    let n = unsigned(get, key, default)?;
    if n < min {
        return Err(invalid(key, "below_min"));
    }
    if n > max {
        return Err(invalid(key, "above_max"));
    }
    Ok(n)
}

/// A host name (letters, digits, dots, hyphens) or an IP address.
fn valid_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    host.len() <= 253
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// A plain `local@domain` address: ASCII, one `@`, a dotted domain, nothing
/// that could start a new header or command line.
pub fn valid_mailbox(address: &str) -> bool {
    const ATEXT: &str = "!#$%&'*+/=?^_`{|}~-";
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    let local_ok = (1..=64).contains(&local.len())
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || ATEXT.contains(c));
    let domain_ok = address.len() <= 254 && domain.contains('.') && valid_host(domain);
    local_ok && domain_ok && domain.parse::<std::net::IpAddr>().is_err()
}

impl NotifyConfig {
    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let d = Self::default();
        let enabled = match text(&get, ENABLED_KEY)? {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(invalid(ENABLED_KEY, "invalid")),
        };
        let default_language = match text(&get, LANGUAGE_KEY)? {
            Some(tag) => Lang::parse(tag).ok_or_else(|| invalid(LANGUAGE_KEY, "invalid"))?,
            None => d.default_language,
        };
        let connect_ms = ranged_u32(&get, CONNECT_KEY, 5_000, (100, 60_000))?;
        let send_ms = ranged_u32(&get, SEND_KEY, 30_000, (1, 300_000))?;
        if send_ms < connect_ms {
            return Err(invalid(SEND_KEY, "below_connect_timeout"));
        }
        let max_attempts = ranged_u32(&get, ATTEMPTS_KEY, 5, (1, 25))?;
        let smtp = Self::load_smtp(&get, enabled)?;
        Ok(Self {
            enabled,
            smtp,
            default_language,
            connect_timeout: Duration::from_millis(u64::from(connect_ms)),
            send_timeout: Duration::from_millis(u64::from(send_ms)),
            max_attempts: u8::try_from(max_attempts).unwrap_or(u8::MAX),
        })
    }

    /// The relay keys: their shape is checked always, their presence only
    /// when enabled (then the result is `Some`).
    fn load_smtp<'a>(
        get: &impl Fn(&str) -> Option<&'a str>,
        enabled: bool,
    ) -> Result<Option<SmtpConfig>, ForgeError> {
        let tls = match text(get, TLS_KEY)? {
            None | Some("starttls") => SmtpTls::StartTls,
            Some("tls") => SmtpTls::Tls,
            Some("none") => SmtpTls::None,
            Some(_) => return Err(invalid(TLS_KEY, "invalid")),
        };
        let default_port = match tls {
            SmtpTls::StartTls => 587,
            SmtpTls::Tls => 465,
            SmtpTls::None => 25,
        };
        let port = ranged_u32(get, PORT_KEY, default_port, (1, 65_535))?;
        let host = match text(get, HOST_KEY)? {
            Some(h) if valid_host(h) => Some(h.to_owned()),
            Some(_) => return Err(invalid(HOST_KEY, "invalid")),
            None => None,
        };
        let from_address = match text(get, FROM_KEY)? {
            Some(a) if valid_mailbox(a) => Some(a.to_owned()),
            Some(_) => return Err(invalid(FROM_KEY, "invalid")),
            None => None,
        };
        let from_name = match text(get, FROM_NAME_KEY)? {
            Some(n) if n.chars().count() <= 64 && !n.chars().any(char::is_control) => n,
            Some(_) => return Err(invalid(FROM_NAME_KEY, "invalid")),
            None => "GitStation",
        };
        let ca_file = match text(get, CA_KEY)? {
            Some(p) if !std::path::Path::new(p).is_absolute() => {
                return Err(invalid(CA_KEY, "not_absolute"));
            }
            Some(_) if tls == SmtpTls::None => return Err(invalid(CA_KEY, "needs_tls")),
            other => other.map(PathBuf::from),
        };
        let username = match text(get, USER_KEY)? {
            Some(u) if u.len() <= 255 && !u.chars().any(char::is_control) => Some(u.to_owned()),
            Some(_) => return Err(invalid(USER_KEY, "invalid")),
            None => None,
        };
        let password_file = match text(get, PASSWORD_KEY)? {
            Some(p) if !std::path::Path::new(p).is_absolute() => {
                return Err(invalid(PASSWORD_KEY, "not_absolute"));
            }
            other => other.map(PathBuf::from),
        };
        match (&username, &password_file) {
            (Some(_), None) => return Err(invalid(PASSWORD_KEY, "missing")),
            (None, Some(_)) => return Err(invalid(USER_KEY, "missing")),
            (Some(_), Some(_)) if tls == SmtpTls::None => {
                return Err(invalid(USER_KEY, "credentials_without_tls"));
            }
            (Some(_), Some(_)) | (None, None) => {}
        }
        if !enabled {
            return Ok(None);
        }
        let host = host.ok_or_else(|| invalid(HOST_KEY, "missing"))?;
        let from_address = from_address.ok_or_else(|| invalid(FROM_KEY, "missing"))?;
        if let Some(path) = &password_file {
            // A bad file stops the start, not the first mail.
            read_password(PASSWORD_KEY, path)?;
        }
        Ok(Some(SmtpConfig {
            host,
            port: u16::try_from(port).unwrap_or(u16::MAX),
            tls,
            ca_file,
            username,
            password_file,
            from_address,
            from_name: from_name.to_owned(),
        }))
    }

    /// The relay password (`None` without AUTH), read now.
    pub fn smtp_password(&self) -> Result<Option<Secret>, ForgeError> {
        self.smtp
            .as_ref()
            .and_then(|smtp| smtp.password_file.as_deref())
            .map(|path| read_password(PASSWORD_KEY, path))
            .transpose()
    }
}

#[cfg(test)]
#[path = "notify_tests.rs"]
mod tests;
