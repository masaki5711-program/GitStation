//! The Podman calls of a job (M4 C4, plan 14 §3.1, §9.3): one throwaway
//! rootless container per job attempt, argv only.
//!
//! ```text
//! podman run --rm --name=forge-job-<job>-<attempt> --network=none --read-only
//!   --tmpfs=/tmp:rw,nosuid,nodev,size=<tmpfs> --security-opt=no-new-privileges
//!   --cap-drop=all --pids-limit=<n> --memory=<m> --pull=missing
//!   --volume=<work>:/workspace:rw,Z --volume=<script>:/forge:ro,Z --workdir=/workspace
//!   --env=CI=true --env=FORGE_JOB_ID=<job> … --entrypoint=/bin/sh
//!   -- <image@sha256:…> -eu /forge/script.sh
//! ```
//!
//! - The image is the claim's, and must be pinned by digest
//!   ([`valid_image`]) and come from an allowed registry
//!   ([`PodmanPlan::image_allowed`], the runner's `image_registries`;
//!   FG-CI-0011 before any pull); it comes after `--`, so it can never be
//!   an option.
//! - The job's script lines are written to `script.sh` in a directory
//!   mounted read-only; they never reach argv, and only the container's
//!   own shell reads them (the job's sandbox). The runner host runs no
//!   shell at all.
//! - No network (`--network=none`), a read-only root, no capabilities, no
//!   new privileges; the work directory is the only writable mount and is
//!   removed after the job.
//! - The job's token is never in the container's environment.
//! - A timeout or a cancel: `podman kill` and `podman rm --force` (the
//!   container is removed whatever happened; `--rm` covers a normal end).

use std::path::{Path, PathBuf};

use crate::process::Cmd;

/// The script's path inside the container.
pub const SCRIPT_IN_CONTAINER: &str = "/forge/script.sh";

/// Container limits (runner configuration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// `--memory`, e.g. `4g`.
    pub memory: String,
    /// `--pids-limit`.
    pub pids: u32,
    /// The size of the writable `/tmp`, e.g. `1g`.
    pub tmpfs: String,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory: "4g".to_owned(),
            pids: 4096,
            tmpfs: "1g".to_owned(),
        }
    }
}

/// `<name>@sha256:<64 hex>`: a lower-case registry path, no option, no
/// whitespace, no shell character; pinned by digest (plan 14 §9.3).
pub fn valid_image(image: &str) -> bool {
    let Some((name, digest)) = image.split_once("@sha256:") else {
        return false;
    };
    let name_ok = !name.is_empty()
        && name.len() <= 255
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-/:".contains(&b));
    let digest_ok = digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    name_ok && digest_ok
}

/// `forge-job-<job>-<attempt>`: numbers only.
pub fn container_name(job: i64, attempt: i32) -> String {
    format!("forge-job-{job}-{attempt}")
}

/// Builds the Podman calls.
#[derive(Debug, Clone)]
pub struct PodmanPlan {
    podman: PathBuf,
    limits: Limits,
    /// The registry prefixes an image must start with (none: every image
    /// is refused).
    registries: Vec<String>,
}

/// What one container runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub work: &'a Path,
    pub script_dir: &'a Path,
    pub job_id: i64,
    pub pipeline_id: i64,
    pub project_id: i64,
    pub sha: &'a str,
}

impl PodmanPlan {
    pub fn new(podman: PathBuf, limits: Limits) -> Self {
        Self {
            podman,
            limits,
            registries: Vec::new(),
        }
    }

    /// Allow images under these registry prefixes (the runner's
    /// `image_registries`, each ending at a `/`).
    #[must_use]
    pub fn with_registries(mut self, registries: Vec<String>) -> Self {
        self.registries = registries;
        self
    }

    /// A digest-pinned image ([`valid_image`]) under one of the allowed
    /// registry prefixes (FG-CI-0011 otherwise, before any pull).
    pub fn image_allowed(&self, image: &str) -> bool {
        valid_image(image)
            && self
                .registries
                .iter()
                .any(|p| image.starts_with(p.as_str()))
    }

    fn cmd(&self) -> Cmd {
        Cmd::new(self.podman.as_os_str())
    }

    /// `podman run …` (module docs). The caller checked [`valid_image`].
    pub fn run(&self, c: &Container<'_>) -> Cmd {
        let mut work = std::ffi::OsString::from("--volume=");
        work.push(c.work);
        work.push(":/workspace:rw,Z");
        let mut script = std::ffi::OsString::from("--volume=");
        script.push(c.script_dir);
        script.push(":/forge:ro,Z");
        self.cmd()
            .arg("run")
            .arg("--rm")
            .arg(format!("--name={}", c.name))
            .arg("--network=none")
            .arg("--read-only")
            .arg(format!(
                "--tmpfs=/tmp:rw,nosuid,nodev,size={}",
                self.limits.tmpfs
            ))
            .arg("--security-opt=no-new-privileges")
            .arg("--cap-drop=all")
            .arg(format!("--pids-limit={}", self.limits.pids))
            .arg(format!("--memory={}", self.limits.memory))
            .arg("--pull=missing")
            .arg(work)
            .arg(script)
            .arg("--workdir=/workspace")
            .arg("--env=CI=true")
            .arg(format!("--env=FORGE_JOB_ID={}", c.job_id))
            .arg(format!("--env=FORGE_PIPELINE_ID={}", c.pipeline_id))
            .arg(format!("--env=FORGE_PROJECT_ID={}", c.project_id))
            .arg(format!("--env=FORGE_COMMIT_SHA={}", c.sha))
            .arg("--entrypoint=/bin/sh")
            .arg("--")
            .arg(c.image)
            .arg("-eu")
            .arg(SCRIPT_IN_CONTAINER)
    }

    /// `podman kill --signal=KILL -- <name>` (a timeout or a cancel).
    pub fn kill(&self, name: &str) -> Cmd {
        self.cmd()
            .arg("kill")
            .arg("--signal=KILL")
            .arg("--")
            .arg(name)
    }

    /// `podman rm --force --ignore -- <name>`: the container is gone
    /// whatever happened (no error when `--rm` already removed it).
    pub fn remove(&self, name: &str) -> Cmd {
        self.cmd()
            .arg("rm")
            .arg("--force")
            .arg("--ignore")
            .arg("--")
            .arg(name)
    }

    /// `podman info --format={{.Host.Security.Rootless}}`: `true` on a
    /// rootless host (the runner refuses to run jobs otherwise).
    pub fn rootless(&self) -> Cmd {
        self.cmd()
            .arg("info")
            .arg("--format={{.Host.Security.Rootless}}")
            .captured()
    }

    /// `podman unshare rm -rf -- <dir>`: remove a work directory holding
    /// files owned by the container's subordinate ids (which the runner's
    /// own user cannot delete).
    pub fn unshare_remove(&self, dir: &Path) -> Cmd {
        self.cmd()
            .arg("unshare")
            .arg("rm")
            .arg("-rf")
            .arg("--")
            .arg(dir.as_os_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> String {
        "a".repeat(64)
    }

    #[test]
    fn only_a_digest_pinned_image_is_valid() {
        assert!(valid_image(&format!(
            "registry.internal/ci/rust:1.88@sha256:{}",
            digest()
        )));
        for bad in [
            "registry.internal/rust:latest".to_owned(),
            format!("--privileged@sha256:{}", digest()),
            format!("-v=/:/host@sha256:{}", digest()),
            format!("rust;rm -rf /@sha256:{}", digest()),
            format!("rust $(id)@sha256:{}", digest()),
            format!("Rust@sha256:{}", digest()),
            format!("rust@sha256:{}", "A".repeat(64)),
            format!("rust@sha256:{}", "a".repeat(63)),
            "@sha256:".to_owned(),
        ] {
            assert!(!valid_image(&bad), "{bad}");
        }
    }

    #[test]
    fn the_run_argv_is_closed_by_default_and_the_image_follows_the_separator() {
        let image = format!("registry.internal/rust@sha256:{}", digest());
        let plan = PodmanPlan::new(PathBuf::from("podman"), Limits::default());
        let cmd = plan.run(&Container {
            name: "forge-job-7-1",
            image: &image,
            work: Path::new("/r/job/work"),
            script_dir: Path::new("/r/job/script"),
            job_id: 7,
            pipeline_id: 3,
            project_id: 2,
            sha: &"b".repeat(40),
        });
        let argv = cmd.argv();
        for flag in [
            "run",
            "--rm",
            "--network=none",
            "--read-only",
            "--cap-drop=all",
            "--security-opt=no-new-privileges",
            "--entrypoint=/bin/sh",
        ] {
            assert!(argv.iter().any(|a| a == flag), "{flag}: {argv:?}");
        }
        let dash = argv.iter().position(|a| a == "--").expect("--");
        assert_eq!(
            &argv[dash + 1..],
            [image.as_str(), "-eu", SCRIPT_IN_CONTAINER]
        );
        assert!(argv.iter().all(|a| !a.starts_with("--privileged")));
        assert!(cmd.env.is_empty(), "{cmd:?}");
    }
}
