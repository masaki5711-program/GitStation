//! `[llm]`: the local LLM gateway (plan 19 §1, §5; ADR-0039 §5).
//!
//! | key | default | rule |
//! |---|---|---|
//! | `llm.enabled` | `false` | the worker runs the gateway only when `true` (production stays off until a GPU host is named, plan 19 §11 1) |
//! | `llm.endpoint` | — | required when enabled; the Ollama origin `http(s)://host[:port]`; the host must be loopback or in `llm.allowed_hosts` |
//! | `llm.allowed_hosts` | — | comma-separated in-house host names (or addresses) the gateway may call besides loopback |
//! | `llm.ca_file` | — | absolute PEM roots for an `https` endpoint (required with one; no public roots) |
//! | `llm.model` | — | required when enabled; `name:tag@sha256:<64 hex>` (never `latest`) |
//! | `llm.num_ctx` | 8 192 | 1 024 ..= 131 072; the same as every other user of the shared Ollama |
//! | `llm.timeout_ms` | 120 000 | one model call; 1 000 ..= 600 000 |
//! | `llm.concurrency` | 1 | requests at once; 1 ..= 2 |
//! | `llm.paused` | `false` | start paused: nothing new is queued |
//! | `llm.kanflow_busy_url` | — | Kanflow's `GET /api/internal/ai-busy`; host loopback or allowed |
//! | `llm.kanflow_token_file` | — | absolute path of Kanflow's internal token (`x-internal-token`); required with the URL |
//! | `llm.kanflow_poll_ms` | 5 000 | 1 000 ..= 60 000 |
//!
//! An endpoint (or the Kanflow URL) outside loopback and the allow list is
//! refused at load: `FG-SYS-0002 reason=llm_endpoint_not_internal`
//! (contract 8: nothing leaves the house). Every other problem is
//! `FG-SYS-0002` with `config_key` and `reason`.

use std::path::PathBuf;
use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, unsigned};
use crate::llm::ModelRef;

pub const ENABLED_KEY: &str = "llm.enabled";
pub const ENDPOINT_KEY: &str = "llm.endpoint";
pub const ALLOWED_HOSTS_KEY: &str = "llm.allowed_hosts";
pub const CA_KEY: &str = "llm.ca_file";
pub const MODEL_KEY: &str = "llm.model";
pub const NUM_CTX_KEY: &str = "llm.num_ctx";
pub const TIMEOUT_KEY: &str = "llm.timeout_ms";
pub const CONCURRENCY_KEY: &str = "llm.concurrency";
pub const PAUSED_KEY: &str = "llm.paused";
pub const KANFLOW_URL_KEY: &str = "llm.kanflow_busy_url";
pub const KANFLOW_TOKEN_KEY: &str = "llm.kanflow_token_file";
pub const KANFLOW_POLL_KEY: &str = "llm.kanflow_poll_ms";

/// `[llm]` as loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmConfig {
    pub enabled: bool,
    /// `scheme://host[:port]` without a trailing slash.
    pub endpoint: Option<String>,
    pub allowed_hosts: Vec<String>,
    pub ca_file: Option<PathBuf>,
    pub model: Option<ModelRef>,
    pub num_ctx: u32,
    pub timeout: Duration,
    pub concurrency: u8,
    pub paused: bool,
    pub kanflow_busy_url: Option<String>,
    pub kanflow_token_file: Option<PathBuf>,
    pub kanflow_poll: Duration,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            allowed_hosts: Vec::new(),
            ca_file: None,
            model: None,
            num_ctx: 8_192,
            timeout: Duration::from_millis(120_000),
            concurrency: 1,
            paused: false,
            kanflow_busy_url: None,
            kanflow_token_file: None,
            kanflow_poll: Duration::from_millis(5_000),
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

fn absolute<'a>(
    get: &impl Fn(&str) -> Option<&'a str>,
    key: &'static str,
) -> Result<Option<PathBuf>, ForgeError> {
    match text(get, key)? {
        Some(p) if !std::path::Path::new(p).is_absolute() => Err(invalid(key, "not_absolute")),
        other => Ok(other.map(PathBuf::from)),
    }
}

/// A parsed `scheme://host[:port][path]`.
struct Url<'a> {
    scheme: &'a str,
    host: &'a str,
    /// Everything from the first `/` after the authority (may be empty).
    path: &'a str,
}

fn split_url(raw: &str) -> Result<Url<'_>, &'static str> {
    if raw.len() > 512 || raw.contains([' ', '?', '#', '@', '\\']) {
        return Err("invalid");
    }
    let (scheme, rest) = raw.split_once("://").ok_or("invalid")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = if let Some(v6) = authority.strip_prefix('[') {
        let end = v6.find(']').ok_or("invalid")?;
        let after = &v6[end + 1..];
        if !(after.is_empty() || after.starts_with(':')) {
            return Err("invalid");
        }
        port_ok(after.strip_prefix(':'))?;
        &authority[..end + 2]
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => {
                port_ok(Some(port))?;
                host
            }
            None => authority,
        }
    };
    if host.is_empty() || !matches!(scheme, "http" | "https") {
        return Err("invalid");
    }
    Ok(Url { scheme, host, path })
}

fn port_ok(port: Option<&str>) -> Result<(), &'static str> {
    match port {
        None => Ok(()),
        Some(p) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            p.parse::<u16>().map(drop).map_err(|_| "invalid")
        }
        Some(_) => Err("invalid"),
    }
}

/// Loopback: `localhost`, `127.0.0.0/8`, `[::1]`.
pub fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "[::1]"
        || host.strip_prefix("127.").is_some_and(|r| {
            r.split('.').count() == 3 && r.split('.').all(|o| o.parse::<u8>().is_ok())
        })
}

fn internal(host: &str, allowed: &[String]) -> bool {
    is_loopback(host) || allowed.iter().any(|a| a.eq_ignore_ascii_case(host))
}

impl LlmConfig {
    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let d = Self::default();
        let enabled = flag(&get, ENABLED_KEY)?;
        let paused = flag(&get, PAUSED_KEY)?;
        let allowed_hosts: Vec<String> = match text(&get, ALLOWED_HOSTS_KEY)? {
            None => Vec::new(),
            Some(list) => {
                let mut hosts = Vec::new();
                for h in list.split(',').map(str::trim) {
                    let ok = !h.is_empty()
                        && h.len() <= 253
                        && h.chars().all(|c| {
                            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')
                        });
                    if !ok {
                        return Err(invalid(ALLOWED_HOSTS_KEY, "invalid"));
                    }
                    hosts.push(h.to_ascii_lowercase());
                }
                hosts
            }
        };
        let ca_file = absolute(&get, CA_KEY)?;
        let endpoint = match text(&get, ENDPOINT_KEY)? {
            None => None,
            Some(raw) => {
                let trimmed = raw.strip_suffix('/').unwrap_or(raw);
                let url = split_url(trimmed).map_err(|r| invalid(ENDPOINT_KEY, r))?;
                if !url.path.is_empty() {
                    return Err(invalid(ENDPOINT_KEY, "invalid"));
                }
                if !internal(url.host, &allowed_hosts) {
                    return Err(invalid(ENDPOINT_KEY, "llm_endpoint_not_internal"));
                }
                if url.scheme == "https" && ca_file.is_none() {
                    return Err(invalid(CA_KEY, "missing"));
                }
                Some(trimmed.to_owned())
            }
        };
        let model = text(&get, MODEL_KEY)?
            .map(|m| ModelRef::parse(m).map_err(|r| invalid(MODEL_KEY, r)))
            .transpose()?;
        let num_ctx = ranged(&get, NUM_CTX_KEY, d.num_ctx, (1_024, 131_072))?;
        let timeout_ms = ranged(&get, TIMEOUT_KEY, 120_000, (1_000, 600_000))?;
        let concurrency = ranged(&get, CONCURRENCY_KEY, 1, (1, 2))?;
        let kanflow_busy_url = match text(&get, KANFLOW_URL_KEY)? {
            None => None,
            Some(raw) => {
                let url = split_url(raw).map_err(|r| invalid(KANFLOW_URL_KEY, r))?;
                if !internal(url.host, &allowed_hosts) {
                    return Err(invalid(KANFLOW_URL_KEY, "llm_endpoint_not_internal"));
                }
                if url.scheme == "https" && ca_file.is_none() {
                    return Err(invalid(CA_KEY, "missing"));
                }
                Some(raw.to_owned())
            }
        };
        let kanflow_token_file = absolute(&get, KANFLOW_TOKEN_KEY)?;
        if kanflow_busy_url.is_some() != kanflow_token_file.is_some() {
            return Err(invalid(KANFLOW_TOKEN_KEY, "needs_both_url_and_token"));
        }
        let poll_ms = ranged(&get, KANFLOW_POLL_KEY, 5_000, (1_000, 60_000))?;
        if enabled {
            if endpoint.is_none() {
                return Err(invalid(ENDPOINT_KEY, "missing"));
            }
            if model.is_none() {
                return Err(invalid(MODEL_KEY, "missing"));
            }
        }
        Ok(Self {
            enabled,
            endpoint,
            allowed_hosts,
            ca_file,
            model,
            num_ctx,
            timeout: Duration::from_millis(u64::from(timeout_ms)),
            concurrency: u8::try_from(concurrency).unwrap_or(1),
            paused,
            kanflow_busy_url,
            kanflow_token_file,
            kanflow_poll: Duration::from_millis(u64::from(poll_ms)),
        })
    }

    /// The content of `llm.ca_file` (`None` without one); unreadable is
    /// `FG-SYS-0002` (the worker's start stops).
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

    /// Kanflow's internal token (`None` without `llm.kanflow_token_file`).
    pub fn kanflow_token(&self) -> Result<Option<super::Secret>, ForgeError> {
        self.kanflow_token_file
            .as_deref()
            .map(|path| {
                super::read_secret_text(KANFLOW_TOKEN_KEY, path, 4096)
                    .map(|t| super::Secret::new(t.trim().to_owned()))
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use forge_errors::ErrorCode;

    use super::*;

    fn load(pairs: &[(&str, &str)]) -> Result<LlmConfig, ForgeError> {
        let map: BTreeMap<&str, &str> = pairs.iter().copied().collect();
        LlmConfig::load(|k| map.get(k).copied())
    }

    fn reason(pairs: &[(&str, &str)]) -> String {
        let err = load(pairs).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        err.to_string()
    }

    fn model() -> String {
        format!("qwen3:27b@sha256:{}", "a".repeat(64))
    }

    fn abs(name: &str) -> String {
        if cfg!(windows) {
            format!("C:\\forge\\{name}")
        } else {
            format!("/etc/forge/{name}")
        }
    }

    #[test]
    fn the_gateway_is_off_by_default() {
        let cfg = load(&[]).expect("defaults");
        assert_eq!(cfg, LlmConfig::default());
        assert!(!cfg.enabled);
    }

    #[test]
    fn endpoint_outside_allow_list_refused_at_start() {
        let m = model();
        for outside in [
            "https://api.openai.com",
            "http://ollama.example.test:11434",
            "http://10.0.0.5:11434",
        ] {
            let text = reason(&[
                (ENABLED_KEY, "true"),
                (ENDPOINT_KEY, outside),
                (MODEL_KEY, &m),
            ]);
            assert!(
                text.contains("llm_endpoint_not_internal"),
                "{outside}: {text}"
            );
            assert!(text.contains("config_key=llm.endpoint"), "{text}");
        }
        // Loopback and listed in-house hosts are allowed.
        let local = load(&[
            (ENABLED_KEY, "true"),
            (ENDPOINT_KEY, "http://127.0.0.1:11434/"),
            (MODEL_KEY, &m),
        ])
        .expect("loopback");
        assert_eq!(local.endpoint.as_deref(), Some("http://127.0.0.1:11434"));
        load(&[
            (ENABLED_KEY, "true"),
            (ENDPOINT_KEY, "http://gpu01.factory.internal:11434"),
            (ALLOWED_HOSTS_KEY, "gpu01.factory.internal, 10.0.0.5"),
            (MODEL_KEY, &m),
        ])
        .expect("allowed");
        // The Kanflow gate obeys the same rule.
        let text = reason(&[
            (
                KANFLOW_URL_KEY,
                "http://kanflow.example.test/api/internal/ai-busy",
            ),
            (KANFLOW_TOKEN_KEY, &abs("kanflow.token")),
        ]);
        assert!(text.contains("llm_endpoint_not_internal"), "{text}");
    }

    #[test]
    fn enabled_needs_an_endpoint_and_a_pinned_model() {
        assert!(reason(&[(ENABLED_KEY, "true")]).contains("llm.endpoint"));
        assert!(reason(&[
            (ENABLED_KEY, "true"),
            (ENDPOINT_KEY, "http://localhost:11434")
        ])
        .contains("llm.model"));
        assert!(reason(&[(MODEL_KEY, "qwen3:latest")]).contains("digest_missing"));
        assert!(reason(&[(ENDPOINT_KEY, "https://localhost:11434")]).contains("llm.ca_file"));
        assert!(reason(&[(ENDPOINT_KEY, "http://localhost:11434/api")]).contains("invalid"));
        assert!(reason(&[(CONCURRENCY_KEY, "3")]).contains("above_max"));
        assert!(reason(&[(NUM_CTX_KEY, "100")]).contains("below_min"));
        assert!(reason(&[(
            KANFLOW_URL_KEY,
            "http://127.0.0.1:3000/api/internal/ai-busy"
        )])
        .contains("needs_both_url_and_token"));
        assert!(reason(&[(KANFLOW_TOKEN_KEY, "token.txt")]).contains("not_absolute"));
        assert!(reason(&[(ALLOWED_HOSTS_KEY, "a b")]).contains("invalid"));
        let cfg = load(&[
            (ENABLED_KEY, "true"),
            (ENDPOINT_KEY, "http://[::1]:11434"),
            (MODEL_KEY, &model()),
            (CONCURRENCY_KEY, "2"),
            (PAUSED_KEY, "true"),
            (
                KANFLOW_URL_KEY,
                "http://127.0.0.1:3000/api/internal/ai-busy",
            ),
            (KANFLOW_TOKEN_KEY, &abs("kanflow.token")),
        ])
        .expect("complete");
        assert_eq!(cfg.concurrency, 2);
        assert!(cfg.paused);
        assert_eq!(cfg.model.map(|m| m.name), Some("qwen3:27b".to_owned()));
    }
}
