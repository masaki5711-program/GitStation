//! `[portal]`: the PFS portal's home widgets and news (ADR-0036; the PFS
//! portal contract v1, `docs/portal/widgets.md`).
//!
//! | key | default | rule |
//! |---|---|---|
//! | `portal.enabled` | `false` | the widget routes accept the portal's tokens only when `true`; news is staged only when this **and** `portal.news_enabled` are `true` |
//! | `portal.registry_url` | — | required with `portal.news_enabled`; `https://host[:port]` (no path, query, user or fragment); `http` only for a loopback host |
//! | `portal.news_enabled` | `false` | stage news in the outbox and send it; needs `portal.enabled` |
//! | `portal.client_id` | `portal` | the portal's Keycloak client: the `azp` the widget routes accept |
//! | `portal.ca_file` | — | absolute path of the PEM roots that verify the registry; only these are trusted (no public roots); required for an `https` registry |
//! | `portal.timeout_ms` | 5 000 | one news POST, connect and answer; 500 ..= 30 000 |
//! | `portal.max_attempts` | 8 | runs of one news item (the job queue's backoff, 10 s doubling, at most 1 h); 1 ..= 20 |
//!
//! With `portal.enabled = false` (the default) **nothing is written to the
//! news outbox**: the business changes skip the staging step, so turning it
//! on later starts with the next event and never replays the past.
//! Every problem is `FG-SYS-0002` with `config_key` and `reason`.

use std::path::PathBuf;
use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, unsigned};

const ENABLED_KEY: &str = "portal.enabled";
const REGISTRY_KEY: &str = "portal.registry_url";
const NEWS_KEY: &str = "portal.news_enabled";
const CLIENT_KEY: &str = "portal.client_id";
const CA_KEY: &str = "portal.ca_file";
const TIMEOUT_KEY: &str = "portal.timeout_ms";
const ATTEMPTS_KEY: &str = "portal.max_attempts";

/// `[portal]` as loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalConfig {
    pub enabled: bool,
    /// `scheme://host[:port]` without a trailing slash (`Some` whenever
    /// `news_enabled`).
    pub registry_url: Option<String>,
    pub news_enabled: bool,
    pub client_id: String,
    pub ca_file: Option<PathBuf>,
    pub timeout: Duration,
    pub max_attempts: u8,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            registry_url: None,
            news_enabled: false,
            client_id: "portal".to_owned(),
            ca_file: None,
            timeout: Duration::from_millis(5_000),
            max_attempts: 8,
        }
    }
}

fn text<'a>(
    get: &impl Fn(&str) -> Option<&'a str>,
    key: &'static str,
) -> Result<Option<&'a str>, ForgeError> {
    match get(key).map(str::trim) {
        Some("") => Err(invalid(key, "empty")),
        other => Ok(other),
    }
}

fn flag<'a>(get: &impl Fn(&str) -> Option<&'a str>, key: &'static str) -> Result<bool, ForgeError> {
    match text(get, key)? {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(invalid(key, "invalid")),
    }
}

fn ranged<'a>(
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

/// A Keycloak client id: 1..=64 of `[A-Za-z0-9._-]`.
fn valid_client_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The registry's origin: `https://host[:port]` (or `http` on a loopback
/// host), nothing after it but an optional `/`. Returned without the slash.
pub(crate) fn registry_origin(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.strip_suffix('/').unwrap_or(raw);
    let (scheme, rest) = trimmed.split_once("://").ok_or("invalid")?;
    if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ']) || raw.len() > 512 {
        return Err("invalid");
    }
    let host = match rest.rsplit_once(':') {
        Some((host, port)) => {
            if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
                return Err("invalid");
            }
            port.parse::<u16>().map_err(|_| "invalid")?;
            host
        }
        _ => rest,
    };
    if host.is_empty() {
        return Err("invalid");
    }
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    match scheme {
        "https" => Ok(trimmed.to_owned()),
        "http" if loopback => Ok(trimmed.to_owned()),
        "http" => Err("https_required"),
        _ => Err("invalid"),
    }
}

impl PortalConfig {
    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let d = Self::default();
        let enabled = flag(&get, ENABLED_KEY)?;
        let news_enabled = flag(&get, NEWS_KEY)?;
        let registry_url = text(&get, REGISTRY_KEY)?
            .map(|raw| registry_origin(raw).map_err(|reason| invalid(REGISTRY_KEY, reason)))
            .transpose()?;
        let client_id = match text(&get, CLIENT_KEY)? {
            Some(id) if valid_client_id(id) => id.to_owned(),
            Some(_) => return Err(invalid(CLIENT_KEY, "invalid")),
            None => d.client_id,
        };
        let ca_file = match text(&get, CA_KEY)? {
            Some(p) if !std::path::Path::new(p).is_absolute() => {
                return Err(invalid(CA_KEY, "not_absolute"));
            }
            other => other.map(PathBuf::from),
        };
        let timeout_ms = ranged(&get, TIMEOUT_KEY, 5_000, (500, 30_000))?;
        let attempts = ranged(&get, ATTEMPTS_KEY, 8, (1, 20))?;
        if news_enabled && !enabled {
            return Err(invalid(NEWS_KEY, "needs_portal_enabled"));
        }
        if news_enabled {
            let Some(url) = &registry_url else {
                return Err(invalid(REGISTRY_KEY, "missing"));
            };
            // Only the operator's CA is trusted: no fallback to public roots.
            if url.starts_with("https://") && ca_file.is_none() {
                return Err(invalid(CA_KEY, "missing"));
            }
        }
        Ok(Self {
            enabled,
            registry_url,
            news_enabled,
            client_id,
            ca_file,
            timeout: Duration::from_millis(u64::from(timeout_ms)),
            max_attempts: u8::try_from(attempts).unwrap_or(u8::MAX),
        })
    }

    /// News is staged and sent (`portal.enabled` and `portal.news_enabled`).
    pub fn news_on(&self) -> bool {
        self.enabled && self.news_enabled
    }

    /// The content of `portal.ca_file` (`None` without one); an unreadable
    /// file is `FG-SYS-0002` (the worker's start stops).
    pub fn ca_pem(&self) -> Result<Option<Vec<u8>>, ForgeError> {
        self.ca_file
            .as_deref()
            .map(|path| {
                std::fs::read(path).map_err(|e| {
                    invalid(CA_KEY, "unreadable")
                        .with_context("path", path.display())
                        .with_source(e)
                })
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use forge_errors::ErrorCode;

    use super::*;

    fn load(pairs: &[(&str, &str)]) -> Result<PortalConfig, ForgeError> {
        let map: BTreeMap<&str, &str> = pairs.iter().copied().collect();
        PortalConfig::load(|k| map.get(k).copied())
    }

    fn reason(pairs: &[(&str, &str)]) -> String {
        let err = load(pairs).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        err.to_string()
    }

    #[test]
    fn the_portal_is_off_by_default() {
        let cfg = load(&[]).expect("defaults");
        assert_eq!(cfg, PortalConfig::default());
        assert!(!cfg.news_on());
        assert_eq!(cfg.client_id, "portal");
    }

    /// An absolute path of the host (Windows: on drive C:), so the rule
    /// under test is not `not_absolute`.
    fn ca_path() -> &'static str {
        if cfg!(windows) {
            "C:\\forge\\portal-ca.pem"
        } else {
            "/etc/forge/portal-ca.pem"
        }
    }

    #[test]
    fn news_needs_the_portal_a_registry_and_its_ca() {
        assert!(reason(&[(NEWS_KEY, "true")]).contains("needs_portal_enabled"));
        assert!(reason(&[(ENABLED_KEY, "true"), (NEWS_KEY, "true")]).contains("missing"));
        assert!(reason(&[
            (ENABLED_KEY, "true"),
            (NEWS_KEY, "true"),
            (REGISTRY_KEY, "https://portal.example.test")
        ])
        .contains("portal.ca_file"));
        let cfg = load(&[
            (ENABLED_KEY, "true"),
            (NEWS_KEY, "true"),
            (REGISTRY_KEY, "https://portal.example.test:8443/"),
            (CA_KEY, ca_path()),
        ])
        .expect("complete");
        assert!(cfg.news_on());
        assert_eq!(
            cfg.registry_url.as_deref(),
            Some("https://portal.example.test:8443")
        );
        // A loopback registry may be plain http (tests, a local portal).
        let local = load(&[
            (ENABLED_KEY, "true"),
            (NEWS_KEY, "true"),
            (REGISTRY_KEY, "http://127.0.0.1:4100"),
        ])
        .expect("loopback");
        assert!(local.ca_file.is_none());
    }

    #[test]
    fn a_registry_url_is_an_origin_only() {
        for bad in [
            "portal.example.test",
            "https://",
            "https://portal.example.test/api",
            "https://user@portal.example.test",
            "https://portal.example.test?x=1",
            "ftp://portal.example.test",
            "https://portal.example.test:99999",
        ] {
            assert!(
                reason(&[(REGISTRY_KEY, bad)]).contains("reason=invalid"),
                "{bad}"
            );
        }
        assert!(reason(&[(REGISTRY_KEY, "http://portal.example.test")]).contains("https_required"));
    }

    #[test]
    fn the_other_keys_are_checked() {
        assert!(reason(&[(ENABLED_KEY, "yes")]).contains("portal.enabled"));
        assert!(reason(&[(CLIENT_KEY, "a b")]).contains("portal.client_id"));
        assert!(reason(&[(CA_KEY, "relative.pem")]).contains("not_absolute"));
        assert!(reason(&[(TIMEOUT_KEY, "100")]).contains("below_min"));
        assert!(reason(&[(ATTEMPTS_KEY, "21")]).contains("above_max"));
        assert!(reason(&[(REGISTRY_KEY, "")]).contains("empty"));
    }
}
