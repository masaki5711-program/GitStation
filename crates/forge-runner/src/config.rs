//! The runner's configuration (`/etc/forge-runner/config.toml`, runbook
//! `ci-runner`) and its token. The runner registers nothing itself: an
//! administrator registers it (`forge admin runner register`) and puts the
//! token `fgr_…` it printed once into the token file (mode 0600, ADR-0013)
//! or the environment variable `FORGE_RUNNER_TOKEN`.
//!
//! ```toml
//! server = "https://git.example.internal"   # forge (https)
//! # allow_insecure_http = false             # http only with this set to true
//! token_file = "/etc/forge-runner/token"    # or FORGE_RUNNER_TOKEN
//! work_dir = "/var/lib/forge-runner"        # the job's directories
//! # ca_file = "/etc/forge-runner/internal-ca.pem"  # the API client's and git's CA
//! # git = "git"
//! # podman = "podman"
//! # memory = "4g"
//! # pids_limit = 4096
//! # tmpfs_size = "1g"
//! ```
//!
//! Every problem is `FG-SYS-0002 ConfigInvalid` with `config_key` and
//! `reason` (never the token).

use std::path::{Path, PathBuf};

use forge_errors::{ErrorCode, ForgeError};
use serde::Deserialize;
use url::Url;

use crate::podman::Limits;

/// The environment variable that may carry the runner's token.
pub const TOKEN_ENV: &str = "FORGE_RUNNER_TOKEN";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    server: String,
    #[serde(default)]
    token_file: Option<PathBuf>,
    work_dir: PathBuf,
    #[serde(default)]
    ca_file: Option<PathBuf>,
    #[serde(default)]
    allow_insecure_http: bool,
    #[serde(default)]
    git: Option<PathBuf>,
    #[serde(default)]
    podman: Option<PathBuf>,
    #[serde(default)]
    memory: Option<String>,
    #[serde(default)]
    pids_limit: Option<u32>,
    #[serde(default)]
    tmpfs_size: Option<String>,
}

/// A checked configuration.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub server: Url,
    pub token_file: Option<PathBuf>,
    pub work_dir: PathBuf,
    pub ca_file: Option<PathBuf>,
    pub git: PathBuf,
    pub podman: PathBuf,
    pub limits: Limits,
}

fn invalid(key: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", key)
        .with_context("reason", reason)
}

/// `4g`, `512m`: digits and one unit letter.
fn valid_size(raw: &str) -> bool {
    let Some((digits, unit)) = raw.split_at_checked(raw.len().saturating_sub(1)) else {
        return false;
    };
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && matches!(unit, "k" | "m" | "g")
}

impl RunnerConfig {
    /// Parse and check the TOML text.
    pub fn parse(text: &str) -> Result<Self, ForgeError> {
        let raw: Raw = toml::from_str(text).map_err(|_| invalid("config", "unparsable"))?;
        let server = Url::parse(&raw.server).map_err(|_| invalid("server", "not_a_url"))?;
        if !matches!(server.scheme(), "https" | "http")
            || server.host_str().is_none()
            || !server.username().is_empty()
            || server.password().is_some()
            || server.query().is_some()
        {
            return Err(invalid("server", "not_an_http_origin"));
        }
        // The runner token and every job's token cross this connection.
        if server.scheme() == "http" && !raw.allow_insecure_http {
            return Err(invalid("server", "insecure_http"));
        }
        if !raw.work_dir.is_absolute() {
            return Err(invalid("work_dir", "not_absolute"));
        }
        let limits = Limits {
            memory: raw.memory.unwrap_or_else(|| Limits::default().memory),
            pids: raw.pids_limit.unwrap_or(Limits::default().pids),
            tmpfs: raw.tmpfs_size.unwrap_or_else(|| Limits::default().tmpfs),
        };
        if !valid_size(&limits.memory) {
            return Err(invalid("memory", "not_a_size"));
        }
        if !valid_size(&limits.tmpfs) {
            return Err(invalid("tmpfs_size", "not_a_size"));
        }
        if limits.pids == 0 {
            return Err(invalid("pids_limit", "zero"));
        }
        Ok(Self {
            server,
            token_file: raw.token_file,
            work_dir: raw.work_dir,
            ca_file: raw.ca_file,
            git: raw.git.unwrap_or_else(|| PathBuf::from("git")),
            podman: raw.podman.unwrap_or_else(|| PathBuf::from("podman")),
            limits,
        })
    }

    /// Read and check the file at `path`.
    pub fn load(path: &Path) -> Result<Self, ForgeError> {
        let text = std::fs::read_to_string(path).map_err(|_| invalid("config", "unreadable"))?;
        Self::parse(&text)
    }

    /// The runner's token: `FORGE_RUNNER_TOKEN` (`env`), else the token file.
    /// It must be a runner token (`fgr_…`).
    pub fn token(
        &self,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<forge_client::store::Token, ForgeError> {
        let raw = match env(TOKEN_ENV).filter(|t| !t.trim().is_empty()) {
            Some(t) => zeroize::Zeroizing::new(t),
            None => {
                let file = self
                    .token_file
                    .as_ref()
                    .ok_or_else(|| invalid("token_file", "missing"))?;
                zeroize::Zeroizing::new(
                    std::fs::read_to_string(file)
                        .map_err(|_| invalid("token_file", "unreadable"))?,
                )
            }
        };
        if !raw.trim().starts_with("fgr_") {
            return Err(invalid("token_file", "not_a_runner_token"));
        }
        forge_client::store::Token::parse(&raw).map_err(|_| invalid("token_file", "malformed"))
    }

    /// The job's fetch URL: `<server>/api/v1/runner/jobs/<job>/repo.git`.
    pub fn fetch_url(&self, job: i64) -> Result<String, ForgeError> {
        self.server
            .join(&format!("/api/v1/runner/jobs/{job}/repo.git"))
            .map(String::from)
            .map_err(|_| invalid("server", "not_joinable"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use super::*;

    fn reason(err: &ForgeError) -> String {
        err.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn a_minimal_configuration_takes_the_defaults() {
        let work = if cfg!(windows) {
            "C:/r"
        } else {
            "/var/lib/forge-runner"
        };
        let cfg = RunnerConfig::parse(&format!(
            "server = \"https://git.example.test\"\nwork_dir = \"{work}\"\n"
        ));
        let cfg = cfg.expect("valid");
        assert_eq!(cfg.git, PathBuf::from("git"));
        assert_eq!(cfg.limits, Limits::default());
        assert_eq!(
            cfg.fetch_url(7).expect("url"),
            "https://git.example.test/api/v1/runner/jobs/7/repo.git"
        );
    }

    #[test]
    fn bad_values_are_config_invalid() {
        let work = if cfg!(windows) { "C:/r" } else { "/r" };
        for (text, want) in [
            (
                "server = \"ftp://x\"\nwork_dir = \"/r\"".to_owned(),
                "not_an_http_origin",
            ),
            (
                format!("server = \"https://u:p@x\"\nwork_dir = \"{work}\""),
                "not_an_http_origin",
            ),
            (
                "server = \"https://x\"\nwork_dir = \"rel\"".to_owned(),
                "not_absolute",
            ),
            (
                format!("server = \"https://x\"\nwork_dir = \"{work}\"\nmemory = \"4g; rm\""),
                "not_a_size",
            ),
            ("server = \"https://x\"\nwork = 1".to_owned(), "unparsable"),
        ] {
            let err = RunnerConfig::parse(&text).expect_err(&text);
            assert_eq!(err.code(), ErrorCode::ConfigInvalid);
            assert_eq!(reason(&err), want, "{text}");
        }
    }

    /// M4 C4 review: plain http only when asked for explicitly.
    #[test]
    fn http_needs_an_explicit_opt_in() {
        let work = if cfg!(windows) { "C:/r" } else { "/r" };
        let err = RunnerConfig::parse(&format!(
            "server = \"http://git.example.test\"\nwork_dir = \"{work}\""
        ))
        .expect_err("http");
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert_eq!(reason(&err), "insecure_http");
        let cfg = RunnerConfig::parse(&format!(
            "server = \"http://git.example.test\"\nwork_dir = \"{work}\"\nallow_insecure_http = true"
        ))
        .expect("opted in");
        assert_eq!(cfg.server.scheme(), "http");
    }

    #[test]
    fn the_token_comes_from_the_environment_or_the_file_and_is_a_runner_token() {
        let dir = tempfile::tempdir().expect("dir");
        let file = dir.path().join("token");
        std::fs::write(&file, "fgr_0011_secret\n").expect("write");
        let work = dir.path().to_string_lossy().replace('\\', "/");
        let cfg = RunnerConfig::parse(&format!(
            "server = \"https://x\"\nwork_dir = \"{work}\"\ntoken_file = \"{}\"",
            file.to_string_lossy().replace('\\', "/")
        ))
        .expect("valid");
        let none = |_: &str| None;
        assert_eq!(cfg.token(&none).expect("file").expose(), "fgr_0011_secret");
        let env = |_: &str| Some("fgr_22_env".to_owned());
        assert_eq!(cfg.token(&env).expect("env").expose(), "fgr_22_env");
        let job = |_: &str| Some("fgj_1_x".to_owned());
        assert_eq!(
            reason(&cfg.token(&job).expect_err("a job token")),
            "not_a_runner_token"
        );
    }
}
