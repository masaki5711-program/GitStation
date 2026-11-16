//! `[ci]`: the CI variables' keys (M4 C6, plan 14 §9 #2, ADR-0013).
//!
//! - `ci.secret_keys_file`: the AES-256-GCM keys that seal the CI
//!   variables (`<key_id> <64 hex>` per line; a secret file, absolute
//!   path; [`crate::ci_secret::CiSecretKeys`]). Without it no variable can
//!   be set and a job that asks for secrets is failed with `FG-CI-0016`
//!   (doctor `ci.secret_keys`); a file that does not read stops
//!   `forge serve` at the start.
//! - `ci.artifact_quota_bytes` (default 5 GiB, 1 MiB..=1 TiB): the most
//!   bytes of stored CI artifacts one project may hold
//!   (`FG-CI-0022` past it).
//! - `ci.gpu_eval_projects` (plan 17 P17-1; default none): the project ids
//!   (comma separated) whose jobs may ask for the `gpu-eval` runner. A
//!   pipeline of another project with such a job is `invalid`
//!   (`gpu_eval_not_allowed`), and the claim never hands one out.
//! - `ci.gpu_eval_max_running` (default 1, 1..=16): the `gpu-eval` jobs of
//!   one project running at once (the claim waits past it).

use std::path::PathBuf;

use forge_errors::ForgeError;

use super::{database, invalid};
use crate::ci_secret::{CiSecretKeys, SECRET_KEYS_KEY};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiConfig {
    /// `ci.secret_keys_file`.
    pub secret_keys_file: Option<PathBuf>,
    /// `ci.artifact_quota_bytes`.
    pub artifact_quota_bytes: u64,
    /// `ci.gpu_eval_projects` / `ci.gpu_eval_max_running`.
    pub gpu_eval: GpuEvalPolicy,
}

/// Plan 17 P17-1: the projects whose jobs may ask for the `gpu-eval`
/// runner, and how many of one project's run at once. The default allows
/// none (the GPU runner is opt-in per project).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuEvalPolicy {
    /// Project ids, sorted, without duplicates.
    pub projects: Vec<i64>,
    /// At least 1.
    pub max_running_per_project: u32,
}

impl Default for GpuEvalPolicy {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            max_running_per_project: 1,
        }
    }
}

impl GpuEvalPolicy {
    /// The most projects listed.
    pub const MAX_PROJECTS: usize = 256;
    /// The largest `ci.gpu_eval_max_running`.
    pub const MAX_RUNNING: u32 = 16;

    /// May `project`'s jobs ask for the `gpu-eval` runner.
    pub fn allows(&self, project: i64) -> bool {
        self.projects.binary_search(&project).is_ok()
    }

    /// `12, 40,7` → `[7, 12, 40]`: positive ids, at most
    /// [`Self::MAX_PROJECTS`], each once.
    fn parse_projects(raw: &str) -> Result<Vec<i64>, &'static str> {
        let mut out = Vec::new();
        for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part.parse::<i64>() {
                Ok(id) if id > 0 => out.push(id),
                Ok(_) | Err(_) => return Err("invalid"),
            }
        }
        if out.len() > Self::MAX_PROJECTS {
            return Err("above_max");
        }
        out.sort_unstable();
        let before = out.len();
        out.dedup();
        if out.len() != before {
            return Err("duplicate");
        }
        Ok(out)
    }
}

/// The configuration keys of the GPU runner's policy.
pub const GPU_EVAL_PROJECTS_KEY: &str = "ci.gpu_eval_projects";
pub const GPU_EVAL_MAX_RUNNING_KEY: &str = "ci.gpu_eval_max_running";

impl Default for CiConfig {
    fn default() -> Self {
        Self {
            secret_keys_file: None,
            artifact_quota_bytes: Self::DEFAULT_ARTIFACT_QUOTA_BYTES,
            gpu_eval: GpuEvalPolicy::default(),
        }
    }
}

/// The configuration key of the artifact quota.
pub const ARTIFACT_QUOTA_KEY: &str = "ci.artifact_quota_bytes";

impl CiConfig {
    pub const DEFAULT_ARTIFACT_QUOTA_BYTES: u64 = 5 * 1024 * 1024 * 1024;
    const MIN_ARTIFACT_QUOTA_BYTES: u64 = 1024 * 1024;
    const MAX_ARTIFACT_QUOTA_BYTES: u64 = 1024 * 1024 * 1024 * 1024;

    pub(super) fn load<'a>(
        get: impl Fn(&str) -> Option<&'a str> + Copy,
    ) -> Result<Self, ForgeError> {
        let secret_keys_file = match get(SECRET_KEYS_KEY).map(str::trim) {
            None => None,
            Some("") => return Err(invalid(SECRET_KEYS_KEY, "empty")),
            Some(p) => Some(database::password_path(SECRET_KEYS_KEY, p)?),
        };
        let artifact_quota_bytes = match get(ARTIFACT_QUOTA_KEY).map(str::trim) {
            None => Self::DEFAULT_ARTIFACT_QUOTA_BYTES,
            Some(raw) => match raw.parse::<u64>() {
                Ok(n) if n < Self::MIN_ARTIFACT_QUOTA_BYTES => {
                    return Err(invalid(ARTIFACT_QUOTA_KEY, "below_min"))
                }
                Ok(n) if n > Self::MAX_ARTIFACT_QUOTA_BYTES => {
                    return Err(invalid(ARTIFACT_QUOTA_KEY, "above_max"))
                }
                Ok(n) => n,
                Err(_) => return Err(invalid(ARTIFACT_QUOTA_KEY, "invalid")),
            },
        };
        let projects = match get(GPU_EVAL_PROJECTS_KEY) {
            None => Vec::new(),
            Some(raw) => GpuEvalPolicy::parse_projects(raw)
                .map_err(|reason| invalid(GPU_EVAL_PROJECTS_KEY, reason))?,
        };
        let max_running_per_project = match get(GPU_EVAL_MAX_RUNNING_KEY).map(str::trim) {
            None => 1,
            Some(raw) => match raw.parse::<u32>() {
                Ok(0) => return Err(invalid(GPU_EVAL_MAX_RUNNING_KEY, "below_min")),
                Ok(n) if n > GpuEvalPolicy::MAX_RUNNING => {
                    return Err(invalid(GPU_EVAL_MAX_RUNNING_KEY, "above_max"))
                }
                Ok(n) => n,
                Err(_) => return Err(invalid(GPU_EVAL_MAX_RUNNING_KEY, "invalid")),
            },
        };
        Ok(Self {
            secret_keys_file,
            artifact_quota_bytes,
            gpu_eval: GpuEvalPolicy {
                projects,
                max_running_per_project,
            },
        })
    }

    /// Read `ci.secret_keys_file` now (`None` when not configured).
    pub fn secret_keys(&self) -> Result<Option<CiSecretKeys>, ForgeError> {
        self.secret_keys_file
            .as_deref()
            .map(CiSecretKeys::read)
            .transpose()
    }
}
