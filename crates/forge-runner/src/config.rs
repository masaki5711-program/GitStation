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
//! image_registries = ["registry.internal/"] # a job's image must start with one
//! # ca_file = "/etc/forge-runner/internal-ca.pem"  # the API client's and git's CA
//! # git = "git"
//! # podman = "podman"
//! # memory = "4g"
//! # pids_limit = 4096
//! # tmpfs_size = "1g"
//! # file_size_limit = "4g"   # --ulimit=fsize
//! # nofile_limit = 4096      # --ulimit=nofile
//! # work_dir_max_bytes = 21474836480  # checked when the job ends (FG-CI-0012)
//!
//! # Only on the runner tagged `gpu-eval` (plan 17 P17-1, `crate::gpu`):
//! # [gpu_eval]
//! # devices = ["nvidia.com/gpu=all"]     # CDI names only
//! # [gpu_eval.model_store]
//! # path = "/srv/forge-models"           # not the runner's, not writable
//! # target = "/models"                   # in the service container
//! # manifest_sha256 = "<64 hex>"         # sha256 of <path>/forge-models.sha256
//! ```
//!
//! Every problem is `FG-SYS-0002 ConfigInvalid` with `config_key` and
//! `reason` (never the token).

use std::path::{Path, PathBuf};

use forge_errors::{ErrorCode, ForgeError};
use serde::Deserialize;
use url::Url;

use crate::gpu::{GpuEval, ModelStore, MAX_DEVICES};
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
    image_registries: Vec<String>,
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
    #[serde(default)]
    file_size_limit: Option<String>,
    #[serde(default)]
    nofile_limit: Option<u32>,
    #[serde(default)]
    work_dir_max_bytes: Option<u64>,
    #[serde(default)]
    gpu_eval: Option<RawGpuEval>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGpuEval {
    #[serde(default)]
    devices: Vec<String>,
    #[serde(default)]
    model_store: Option<RawModelStore>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModelStore {
    path: PathBuf,
    target: String,
    manifest_sha256: String,
}

/// A checked configuration.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub server: Url,
    pub token_file: Option<PathBuf>,
    pub work_dir: PathBuf,
    pub ca_file: Option<PathBuf>,
    /// The registry prefixes a job's image must start with (each ends at
    /// a `/`).
    pub image_registries: Vec<String>,
    pub git: PathBuf,
    pub podman: PathBuf,
    pub limits: Limits,
    /// The job's work directory's bound, checked when its container ends
    /// (FG-CI-0012); the filesystem quota is the bound while it runs.
    pub work_dir_max_bytes: Option<u64>,
    /// Plan 17 P17-1: the GPU and the model store a service of a
    /// `gpu-eval` job may get (`None`: this runner has no GPU; a service
    /// that asks fails with FG-CI-0029).
    pub gpu_eval: Option<GpuEval>,
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

/// A `:` or `,` in the work directory (after a Windows drive prefix, on
/// development machines only): it would split Podman's `--volume=` value
/// into another target or option. Not UTF-8 is refused too.
fn has_mount_separator(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return true;
    };
    let text = if cfg!(windows) {
        match text.as_bytes() {
            [d, b':', ..] if d.is_ascii_alphabetic() => &text[2..],
            _ => text,
        }
    } else {
        text
    };
    text.contains([':', ','])
}

/// `registry.internal/ci/`: a lower-case registry path (the characters
/// of `podman::valid_image`'s name) that starts with a letter or digit,
/// ends at a `/` (so `registry.internal` never matches
/// `registry.internal.evil/`) and has no empty or dot-only component.
fn valid_registry_prefix(prefix: &str) -> bool {
    let Some(path) = prefix.strip_suffix('/') else {
        return false;
    };
    prefix.len() <= 255
        && prefix
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-/:".contains(&b))
        && path
            .split('/')
            .all(|c| !c.is_empty() && c.bytes().any(|b| b != b'.'))
}

/// `[gpu_eval]`: CDI names only; the store an absolute host path outside
/// the work directory without a mount separator, a plain container path,
/// and a sha256.
fn gpu_eval(raw: RawGpuEval, work_dir: &Path) -> Result<GpuEval, ForgeError> {
    if raw.devices.is_empty() {
        return Err(invalid("gpu_eval.devices", "missing"));
    }
    if raw.devices.len() > MAX_DEVICES {
        return Err(invalid("gpu_eval.devices", "too_many"));
    }
    if !raw.devices.iter().all(|d| crate::gpu::valid_cdi_device(d)) {
        return Err(invalid("gpu_eval.devices", "not_a_cdi_device"));
    }
    let model_store = match raw.model_store {
        None => None,
        Some(m) => {
            if !m.path.is_absolute() {
                return Err(invalid("gpu_eval.model_store.path", "not_absolute"));
            }
            if has_mount_separator(&m.path) {
                return Err(invalid("gpu_eval.model_store.path", "mount_separator"));
            }
            if m.path.starts_with(work_dir) || work_dir.starts_with(&m.path) {
                return Err(invalid("gpu_eval.model_store.path", "overlaps_work_dir"));
            }
            if !crate::gpu::valid_target(&m.target) {
                return Err(invalid(
                    "gpu_eval.model_store.target",
                    "not_a_container_path",
                ));
            }
            if m.manifest_sha256.len() != 64
                || !m
                    .manifest_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(invalid(
                    "gpu_eval.model_store.manifest_sha256",
                    "not_a_sha256",
                ));
            }
            Some(ModelStore::new(m.path, m.target, m.manifest_sha256))
        }
    };
    Ok(GpuEval {
        devices: raw.devices,
        model_store,
    })
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
        if has_mount_separator(&raw.work_dir) {
            return Err(invalid("work_dir", "mount_separator"));
        }
        let limits = Limits {
            memory: raw.memory.unwrap_or_else(|| Limits::default().memory),
            pids: raw.pids_limit.unwrap_or(Limits::default().pids),
            tmpfs: raw.tmpfs_size.unwrap_or_else(|| Limits::default().tmpfs),
            file_size: raw
                .file_size_limit
                .unwrap_or_else(|| Limits::default().file_size),
            nofile: raw.nofile_limit.unwrap_or(Limits::default().nofile),
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
        if !valid_size(&limits.file_size) || crate::podman::size_bytes(&limits.file_size).is_none()
        {
            return Err(invalid("file_size_limit", "not_a_size"));
        }
        if limits.nofile == 0 {
            return Err(invalid("nofile_limit", "zero"));
        }
        if raw.image_registries.is_empty() {
            return Err(invalid("image_registries", "missing"));
        }
        if !raw
            .image_registries
            .iter()
            .all(|p| valid_registry_prefix(p))
        {
            return Err(invalid("image_registries", "not_a_prefix"));
        }
        let gpu_eval = match raw.gpu_eval {
            None => None,
            Some(g) => Some(gpu_eval(g, &raw.work_dir)?),
        };
        Ok(Self {
            server,
            token_file: raw.token_file,
            work_dir: raw.work_dir,
            ca_file: raw.ca_file,
            image_registries: raw.image_registries,
            git: raw.git.unwrap_or_else(|| PathBuf::from("git")),
            podman: raw.podman.unwrap_or_else(|| PathBuf::from("podman")),
            limits,
            work_dir_max_bytes: raw.work_dir_max_bytes,
            gpu_eval,
        })
    }

    /// Read and check the file at `path`.
    pub fn load(path: &Path) -> Result<Self, ForgeError> {
        let text = std::fs::read_to_string(path).map_err(|_| invalid("config", "unreadable"))?;
        let mut cfg = Self::parse(&text)?;
        // Plan 17 P17-1 (review): the model store's path resolved once —
        // what is verified and mounted — and the directories above it
        // checked now (FG-CI-0030 otherwise).
        if let Some(gpu) = cfg.gpu_eval.as_mut() {
            if let Some(store) = gpu.model_store.as_mut() {
                *store = store.canonical()?;
                store.check_ancestors()?;
            }
        }
        Ok(cfg)
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

    /// The registries line every valid test configuration carries.
    const REG: &str = "image_registries = [\"registry.internal/\"]\n";

    /// M4 C4 review: the allowed image registries are required, and each
    /// is a registry path prefix ending at a `/`.
    #[test]
    fn image_registries_are_required_prefixes() {
        let work = if cfg!(windows) { "C:/r" } else { "/r" };
        let base = format!("server = \"https://x\"\nwork_dir = \"{work}\"\n");
        for (line, want) in [
            ("", "missing"),
            ("image_registries = []\n", "missing"),
            (
                "image_registries = [\"registry.internal\"]\n",
                "not_a_prefix",
            ),
            ("image_registries = [\"/\"]\n", "not_a_prefix"),
            (
                "image_registries = [\"Registry.internal/\"]\n",
                "not_a_prefix",
            ),
            ("image_registries = [\"r/../x/\"]\n", "not_a_prefix"),
        ] {
            let text = format!("{line}{base}");
            let err = RunnerConfig::parse(&text).expect_err(&text);
            assert_eq!(err.code(), ErrorCode::ConfigInvalid);
            assert_eq!(reason(&err), want, "{text}");
        }
        let cfg = RunnerConfig::parse(&format!(
            "image_registries = [\"registry.internal/ci/\", \"mirror.internal:5000/\"]\n{base}"
        ))
        .expect("valid");
        assert_eq!(
            cfg.image_registries,
            ["registry.internal/ci/", "mirror.internal:5000/"]
        );
    }

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
            "{REG}server = \"https://git.example.test\"\nwork_dir = \"{work}\"\n"
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
            (
                format!(
                    "{REG}server = \"https://x\"\nwork_dir = \"{work}\"\nfile_size_limit = \"4t\""
                ),
                "not_a_size",
            ),
            (
                format!("{REG}server = \"https://x\"\nwork_dir = \"{work}\"\nnofile_limit = 0"),
                "zero",
            ),
            ("server = \"https://x\"\nwork = 1".to_owned(), "unparsable"),
        ] {
            let err = RunnerConfig::parse(&text).expect_err(&text);
            assert_eq!(err.code(), ErrorCode::ConfigInvalid);
            assert_eq!(reason(&err), want, "{text}");
        }
    }

    /// M4 C4 review: the work directory becomes part of Podman's
    /// `--volume=<path>:/workspace:rw,Z`; a `:` or `,` in it would add a
    /// mount target or option.
    #[test]
    fn a_work_dir_that_could_change_the_mount_is_refused() {
        let work = if cfg!(windows) { "C:/r" } else { "/r" };
        for bad in [":/etc:rw", "a,z", "x:y"] {
            let text = format!("server = \"https://x\"\nwork_dir = \"{work}/{bad}\"");
            let err = RunnerConfig::parse(&text).expect_err(&text);
            assert_eq!(err.code(), ErrorCode::ConfigInvalid);
            assert_eq!(reason(&err), "mount_separator", "{text}");
        }
        RunnerConfig::parse(&format!(
            "{REG}server = \"https://x\"\nwork_dir = \"{work}/forge-runner_1.d\""
        ))
        .expect("a plain path");
    }

    /// Plan 17 P17-1: `[gpu_eval]` names CDI devices and a model store the
    /// configuration checked; a runner without it has no GPU.
    #[test]
    fn the_gpu_runner_configuration_is_checked() {
        let (work, store) = if cfg!(windows) {
            ("C:/r", "C:/models")
        } else {
            ("/var/lib/forge-runner", "/srv/forge-models")
        };
        let base = format!("{REG}server = \"https://x\"\nwork_dir = \"{work}\"\n");
        assert!(RunnerConfig::parse(&base)
            .expect("plain")
            .gpu_eval
            .is_none());
        let hex = "b".repeat(64);
        let good = format!(
            "{base}[gpu_eval]\ndevices = [\"nvidia.com/gpu=all\"]\n[gpu_eval.model_store]\npath = \"{store}\"\ntarget = \"/models\"\nmanifest_sha256 = \"{hex}\"\n"
        );
        let gpu = RunnerConfig::parse(&good)
            .expect("gpu")
            .gpu_eval
            .expect("gpu");
        assert_eq!(gpu.devices, ["nvidia.com/gpu=all"]);
        let ms = gpu.model_store.expect("store");
        assert_eq!(ms.target, "/models");
        assert!(ms.refuse_own_files && ms.check_ancestors);
        let path = format!("path = \"{store}\"");
        for (text, key, want) in [
            (
                format!("{base}[gpu_eval]\ndevices = []\n"),
                "gpu_eval.devices",
                "missing",
            ),
            (
                format!("{base}[gpu_eval]\ndevices = [\"/dev/nvidia0\"]\n"),
                "gpu_eval.devices",
                "not_a_cdi_device",
            ),
            (
                good.replace(&path, "path = \"models\""),
                "gpu_eval.model_store.path",
                "not_absolute",
            ),
            (
                good.replace(&path, &format!("path = \"{work}/m\"")),
                "gpu_eval.model_store.path",
                "overlaps_work_dir",
            ),
            (
                good.replace(&path, &format!("path = \"{store}/a,rw\"")),
                "gpu_eval.model_store.path",
                "mount_separator",
            ),
            (
                good.replace("target = \"/models\"", "target = \"/workspace\""),
                "gpu_eval.model_store.target",
                "not_a_container_path",
            ),
            (
                good.replace(&hex, "latest"),
                "gpu_eval.model_store.manifest_sha256",
                "not_a_sha256",
            ),
        ] {
            let err = RunnerConfig::parse(&text).expect_err(&text);
            assert_eq!(err.code(), ErrorCode::ConfigInvalid);
            assert_eq!(reason(&err), want, "{text}");
            assert!(
                err.context()
                    .iter()
                    .any(|(k, v)| *k == "config_key" && v == key),
                "{text}"
            );
        }
        // An unknown key (a host path for the job, say) is refused.
        let err = RunnerConfig::parse(&format!(
            "{base}[gpu_eval]\ndevices = [\"nvidia.com/gpu=all\"]\njob_volumes = [\"/srv\"]\n"
        ))
        .expect_err("unknown");
        assert_eq!(reason(&err), "unparsable");
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
            "{REG}server = \"http://git.example.test\"\nwork_dir = \"{work}\"\nallow_insecure_http = true"
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
            "{REG}server = \"https://x\"\nwork_dir = \"{work}\"\ntoken_file = \"{}\"",
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
