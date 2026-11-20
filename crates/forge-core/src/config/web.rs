//! `[web]`: the browser-facing settings (M2 MR3, ADR-0021 §5, §7, §12).
//!
//! - `web.base_url` — the URL people open GitStation at: `https://host[:port]`,
//!   or `http://localhost` / `http://127.0.0.1` (a secure context) for
//!   development. No path, query, fragment or user info. Its origin is what
//!   a state-changing browser request's `Origin` must equal (CSRF, ADR-0021
//!   §6); later also the OIDC `redirect_uri` and absolute `Location`s.
//!   `forge serve` requires it ([`WebConfig::require_base_url`]).
//! - `web.session_key_file` — the secret file of the per-purpose keys
//!   ([`crate::web_keys`], ADR-0013 format `<kid> <hex>`). Optional until
//!   the OIDC login needs it (MR4); when set, `forge serve` reads it before
//!   it binds, so a missing or broken file stops the start (`FG-SYS-0002`).
//! - `web.session_idle_secs` (default 14 400 = 4 h) and
//!   `web.session_absolute_secs` (default 43 200 = 12 h): a session ends
//!   after this long without activity, and at the latest this long after
//!   sign-in. Each at least 60 s, idle ≤ absolute ≤ 86 400 (24 h).
//!
//! - `web.session_purge_interval_secs` (default 3 600, at least 60, at most
//!   86 400; M2 MR6): the period of the `forge worker` task that deletes
//!   sessions 30 days past their revocation or absolute expiry.
//!
//! - `web.allow_insecure_http` (default `false`): accept an `http`
//!   `web.base_url` on a host other than the loopback — **a plain-HTTP trial
//!   server only** (the PFS trial box, `ops/gitlab-trial/`). A browser keeps
//!   no `Secure` / `__Host-` cookie from such an origin, so with such a base
//!   URL `forge serve` translates the session and sign-in cookies to
//!   port-qualified names (`gstn_<port>_session`) without `Secure` at the edge
//!   (`forge_web::plain_http_cookies`); [`BaseUrl::is_plain_http`] is the
//!   one decision. Cookies, tokens and git passwords then cross the network
//!   in clear text: never set it in production (`forge doctor` warns).
//!
//! Every problem is `FG-SYS-0002` with `config_key` and `reason`
//! (`invalid`, `scheme`, `insecure_http` (an `http` base URL off the
//! loopback without `web.allow_insecure_http = true`), `has_path`,
//! `has_query`, `has_user_info`, `not_absolute`, `below_min`, `above_max`,
//! `above_absolute`, `missing`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, secret, unsigned};
use crate::web_keys::{WebKeys, KEYS_CONFIG_KEY};

const BASE_URL_KEY: &str = "web.base_url";
const IDLE_KEY: &str = "web.session_idle_secs";
const ABSOLUTE_KEY: &str = "web.session_absolute_secs";
const PURGE_INTERVAL_KEY: &str = "web.session_purge_interval_secs";
const ALLOW_INSECURE_HTTP_KEY: &str = "web.allow_insecure_http";
/// Upper bound for the key file (as `api.cursor_keys_file`).
const MAX_KEYS_BYTES: u64 = 64 * 1024;

/// `web.base_url`, validated: its origin (`scheme://host[:port]`, the
/// default port left out, as browsers send `Origin`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl {
    origin: String,
    /// The port (the scheme's default when the URL names none).
    port: u16,
    /// `http` on a host other than the loopback (allowed only with
    /// `web.allow_insecure_http`): not a secure context for a browser.
    plain_http: bool,
}

impl BaseUrl {
    /// Parse and validate a base URL (see the module documentation); an
    /// `http` URL off the loopback is refused (`insecure_http`).
    pub fn parse(raw: &str) -> Result<Self, ForgeError> {
        Self::parse_with(raw, false)
    }

    /// As [`BaseUrl::parse`]; `allow_insecure_http` is
    /// `web.allow_insecure_http` (a plain-HTTP trial server).
    pub fn parse_with(raw: &str, allow_insecure_http: bool) -> Result<Self, ForgeError> {
        let bad = |reason| invalid(BASE_URL_KEY, reason);
        let url = url::Url::parse(raw.trim()).map_err(|_| bad("invalid"))?;
        let host = url.host_str().ok_or_else(|| bad("invalid"))?;
        let plain_http = match url.scheme() {
            "https" => false,
            "http" if matches!(host, "localhost" | "127.0.0.1" | "[::1]") => false,
            "http" if allow_insecure_http => true,
            "http" => return Err(bad("insecure_http")),
            _ => return Err(bad("scheme")),
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err(bad("has_user_info"));
        }
        if url.path() != "/" {
            return Err(bad("has_path"));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(bad("has_query"));
        }
        Ok(Self {
            origin: url.origin().ascii_serialization(),
            port: url.port_or_known_default().unwrap_or(80),
            plain_http,
        })
    }

    /// `scheme://host[:port]`, exactly as a browser's `Origin` header.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The port people open GitStation at (443 / 80 when the URL names
    /// none); the plain-HTTP cookie names carry it.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether people open GitStation over plain `http` on a host other
    /// than the loopback (only with `web.allow_insecure_http = true`): the
    /// browser is not in a secure context and keeps no `Secure` cookie, so
    /// `forge serve` must issue the plain-HTTP cookie names.
    pub fn is_plain_http(&self) -> bool {
        self.plain_http
    }
}

#[derive(Debug)]
pub struct WebConfig {
    base_url: Option<BaseUrl>,
    session_key_file: Option<PathBuf>,
    /// `web.session_idle_secs`.
    pub session_idle: Duration,
    /// `web.session_absolute_secs`.
    pub session_absolute: Duration,
    /// `web.session_purge_interval_secs` (M2 MR6).
    pub session_purge_interval: Duration,
}

impl WebConfig {
    pub const DEFAULT_SESSION_IDLE_SECS: u32 = 4 * 3600;
    pub const DEFAULT_SESSION_ABSOLUTE_SECS: u32 = 12 * 3600;
    pub const MIN_SESSION_SECS: u32 = 60;
    pub const MAX_SESSION_ABSOLUTE_SECS: u32 = 24 * 3600;
    pub const DEFAULT_SESSION_PURGE_INTERVAL_SECS: u32 = 3600;
    pub const MIN_SESSION_PURGE_INTERVAL_SECS: u32 = 60;
    pub const MAX_SESSION_PURGE_INTERVAL_SECS: u32 = 86_400;

    pub(super) fn load<'a>(
        get: impl Fn(&str) -> Option<&'a str> + Copy,
        base_url: Option<&str>,
        session_key_file: Option<PathBuf>,
    ) -> Result<Self, ForgeError> {
        let allow_insecure_http = match get(ALLOW_INSECURE_HTTP_KEY).map(str::trim) {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(invalid(ALLOW_INSECURE_HTTP_KEY, "invalid")),
        };
        let base_url = base_url
            .map(|raw| BaseUrl::parse_with(raw, allow_insecure_http))
            .transpose()?;
        let idle = unsigned(get, IDLE_KEY, Self::DEFAULT_SESSION_IDLE_SECS)?;
        let absolute = unsigned(get, ABSOLUTE_KEY, Self::DEFAULT_SESSION_ABSOLUTE_SECS)?;
        for (key, value) in [(IDLE_KEY, idle), (ABSOLUTE_KEY, absolute)] {
            if value < Self::MIN_SESSION_SECS {
                return Err(invalid(key, "below_min"));
            }
        }
        if absolute > Self::MAX_SESSION_ABSOLUTE_SECS {
            return Err(invalid(ABSOLUTE_KEY, "above_max"));
        }
        if idle > absolute {
            return Err(invalid(IDLE_KEY, "above_absolute"));
        }
        let purge = unsigned(
            get,
            PURGE_INTERVAL_KEY,
            Self::DEFAULT_SESSION_PURGE_INTERVAL_SECS,
        )?;
        if purge < Self::MIN_SESSION_PURGE_INTERVAL_SECS {
            return Err(invalid(PURGE_INTERVAL_KEY, "below_min"));
        }
        if purge > Self::MAX_SESSION_PURGE_INTERVAL_SECS {
            return Err(invalid(PURGE_INTERVAL_KEY, "above_max"));
        }
        Ok(Self {
            base_url,
            session_key_file,
            session_idle: Duration::from_secs(u64::from(idle)),
            session_absolute: Duration::from_secs(u64::from(absolute)),
            session_purge_interval: Duration::from_secs(u64::from(purge)),
        })
    }

    /// `web.base_url`, if configured.
    pub fn base_url(&self) -> Option<&BaseUrl> {
        self.base_url.as_ref()
    }

    /// `web.base_url`, which the caller needs (`forge serve`): not
    /// configured is `FG-SYS-0002 config_key=web.base_url reason=missing`.
    pub fn require_base_url(&self) -> Result<&BaseUrl, ForgeError> {
        self.base_url
            .as_ref()
            .ok_or_else(|| invalid(BASE_URL_KEY, "missing"))
    }

    /// Whether `web.session_key_file` is configured.
    pub fn has_session_keys(&self) -> bool {
        self.session_key_file.is_some()
    }

    /// Read `web.session_key_file` now (`None` when it is not configured).
    pub fn session_keys(&self) -> Result<Option<WebKeys>, ForgeError> {
        self.session_key_file
            .as_deref()
            .map(read_session_keys)
            .transpose()
    }
}

fn read_session_keys(path: &Path) -> Result<WebKeys, ForgeError> {
    let text = secret::read_secret_text(KEYS_CONFIG_KEY, path, MAX_KEYS_BYTES)?;
    WebKeys::parse(&text).map_err(|e| e.with_context("path", path.display()))
}
