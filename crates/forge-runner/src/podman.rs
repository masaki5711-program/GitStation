//! The Podman calls of a job (M4 C4, plan 14 §3.1, §9.3): one throwaway
//! rootless container per job attempt, argv only.
//!
//! ```text
//! podman create --name=forge-job-<job>-<attempt> --network=none
//!   --pid=private --ipc=private --uts=private --cgroupns=private --read-only
//!   --tmpfs=/tmp:rw,nosuid,nodev,size=<tmpfs> --security-opt=no-new-privileges
//!   --cap-drop=all --pids-limit=<n> --memory=<m> --memory-swap=<m>
//!   --ulimit=fsize=<bytes>:<bytes> --ulimit=nofile=<n>:<n> --pull=missing
//!   --volume=<work>:/workspace:rw,Z --volume=<script>:/forge:ro,Z --workdir=/workspace
//!   --env=CI=true --env=FORGE_JOB_ID=<job> … --entrypoint=/bin/sh
//!   -- <image@sha256:…> -eu /forge/script.sh
//! podman start --attach -- forge-job-<job>-<attempt>
//! ```
//!
//! Create and start are separate calls (M4 C4 review): a failed create
//! (the pull, the storage) is Podman's failure (FG-CI-0007); `start
//! --attach` exits with the script's status, and only when it is 125 —
//! Podman's own error code, but also a status a script may exit with —
//! `podman inspect` tells them apart: a container that `exited` ran the
//! script (a failed job with exit code 125), one still `created` never
//! started (FG-CI-0007).
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
//!   removed after the job. Memory without extra swap, a bounded file size,
//!   open files and processes; the work directory's total is bounded by
//!   the filesystem's quota (runbook `ci-runner`) and checked after the job
//!   (`work_dir_max_bytes`, FG-CI-0012).
//! - Its own PID, IPC, UTS and cgroup namespaces, set on every container
//!   rather than left to containers.conf (ADR-0029 amendment 2026-11-12:
//!   the podman/stable image sets `ipcns`/`utsns`/`cgroupns = "host"`).
//!   A default `volumes` entry of containers.conf (that image's
//!   `/proc:/proc`) is added to every container whatever the flags, so the
//!   runner refuses to start when one is configured
//!   ([`PodmanPlan::isolation_probe`], FG-CI-0015).
//! - The job's token is never in the container's environment.
//! - A timeout or a cancel: `podman kill`; then always `podman rm
//!   --force` (the container is removed whatever happened).
//!
//! A job with `services` (plan 14 C5s, ADR-0029 amendment) runs in a pod:
//!
//! ```text
//! podman pod create --name=forge-job-<job>-<attempt> --network=none --share=net
//! podman create --pod=<pod> --name=forge-job-<job>-<attempt>-svc-<name> <the same
//!   confinement and limits> [--user=<user>] [--tmpfs=<path>:…,mode=1777]…
//!   [--env=K=V]… -- <service image@sha256:…>
//! podman start -- <service container>                (each service, detached)
//! podman create --name=forge-job-<job>-<attempt> --pod=<pod> … (the job, as above)
//! podman pod rm --force --ignore --time=0 -- <pod>    (always)
//! ```
//!
//! The pod's network namespace holds only a loopback: services and the job
//! reach each other on `127.0.0.1`, and nothing else. It is the only
//! namespace the pod shares (`--share=net`): each container keeps its own
//! PID, IPC, UTS and cgroup namespaces. Pod and container
//! names are separate in Podman, so the pod shares the job container's
//! name.

use std::path::{Path, PathBuf};

use crate::process::Cmd;

/// The script's path inside the container.
pub const SCRIPT_IN_CONTAINER: &str = "/forge/script.sh";

/// The isolation probe's command: never run (the probe is only created).
pub const PROBE_COMMAND: &str = "/forge-isolation-probe";

/// Container limits (runner configuration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// `--memory`, e.g. `4g`.
    pub memory: String,
    /// `--pids-limit`.
    pub pids: u32,
    /// The size of the writable `/tmp`, e.g. `1g`.
    pub tmpfs: String,
    /// The largest file a job may write (`--ulimit=fsize`), e.g. `4g`.
    pub file_size: String,
    /// The open files of one process (`--ulimit=nofile`).
    pub nofile: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory: "4g".to_owned(),
            pids: 4096,
            tmpfs: "1g".to_owned(),
            file_size: "4g".to_owned(),
            nofile: 4096,
        }
    }
}

/// `4g`, `512m`, `16k` in bytes; `None` for anything else (the
/// configuration checked the form already).
pub fn size_bytes(raw: &str) -> Option<u64> {
    let (digits, unit) = raw.split_at_checked(raw.len().checked_sub(1)?)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let shift = match unit {
        "k" => 10,
        "m" => 20,
        "g" => 30,
        _ => return None,
    };
    digits.parse::<u64>().ok()?.checked_mul(1u64 << shift)
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

/// `forge-job-<job>-<attempt>-svc-<service>`: a service's container in the
/// job's pod (the service name is forge-ci's `[a-z][a-z0-9-]{0,30}`).
pub fn service_container_name(job: i64, attempt: i32, service: &str) -> String {
    format!("forge-job-{job}-{attempt}-svc-{service}")
}

/// The lines of a service's log appended to a failed job's log.
pub const SERVICE_LOG_LINES: u32 = 50;

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
    /// The job's pod when it has services: the container joins it (and
    /// its `--network=none` namespace) instead of getting its own.
    pub pod: Option<&'a str>,
}

/// What one service container runs (plan 14 C5s): the image's own
/// entrypoint, in the job's pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceContainer<'a> {
    pub pod: &'a str,
    pub name: &'a str,
    pub image: &'a str,
    pub env: &'a [(String, String)],
    pub user: Option<&'a str>,
    pub tmpfs: &'a [String],
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

    /// `podman create …` (module docs; its stdout is the container id).
    /// The caller checked [`PodmanPlan::image_allowed`].
    pub fn create(&self, c: &Container<'_>) -> Cmd {
        let mut work = std::ffi::OsString::from("--volume=");
        work.push(c.work);
        work.push(":/workspace:rw,Z");
        let mut script = std::ffi::OsString::from("--volume=");
        script.push(c.script_dir);
        script.push(":/forge:ro,Z");
        let cmd = self.cmd().arg("create").arg(format!("--name={}", c.name));
        // In a pod the network namespace is the pod's (`--network=none`).
        let cmd = match c.pod {
            Some(pod) => cmd.arg(format!("--pod={pod}")),
            None => cmd.arg("--network=none"),
        };
        self.confined(cmd)
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
            .captured()
    }

    /// The flags every container of a job gets (the job's and its
    /// services'): its own PID, IPC, UTS and cgroup namespaces (explicit:
    /// containers.conf may default them to the host's), a read-only root
    /// with a bounded `/tmp`, no capabilities, no new privileges, the same
    /// memory, process, file-size and open-file limits.
    fn confined(&self, cmd: Cmd) -> Cmd {
        // The configuration checked the size; 4 GiB if it did not.
        let fsize = size_bytes(&self.limits.file_size).unwrap_or(4 << 30);
        cmd.arg("--pid=private")
            .arg("--ipc=private")
            .arg("--uts=private")
            .arg("--cgroupns=private")
            .arg("--read-only")
            .arg(format!(
                "--tmpfs=/tmp:rw,nosuid,nodev,size={}",
                self.limits.tmpfs
            ))
            .arg("--security-opt=no-new-privileges")
            .arg("--cap-drop=all")
            .arg(format!("--pids-limit={}", self.limits.pids))
            .arg(format!("--memory={}", self.limits.memory))
            // No swap on top of the memory limit.
            .arg(format!("--memory-swap={}", self.limits.memory))
            .arg(format!("--ulimit=fsize={fsize}:{fsize}"))
            .arg(format!("--ulimit=nofile={n}:{n}", n = self.limits.nofile))
            .arg("--pull=missing")
    }

    /// `podman pod create --name=<pod> --network=none --share=net`: the
    /// job's pod when it has services. Its containers share one network
    /// namespace that holds only a loopback (ADR-0029 amendment), and
    /// nothing else: Podman's default `--share` adds the infra container's
    /// IPC and UTS namespaces, which containers.conf may set to the
    /// runner's own (amendment 2026-11-12).
    ///
    /// The pod's cgroup gets the job's memory limit (without extra swap):
    /// each container keeps its own limits, and the pod as a whole — job
    /// and services together — cannot use more memory than one job may
    /// (security review). Podman has no `--pids-limit` on a pod: the
    /// process limit stays per container (at most 3 × `pids_limit` for a
    /// job with 2 services; runbook `ci-runner`).
    ///
    /// No `--infra-image`: rootless Podman builds its pause image locally
    /// (`localhost/podman-pause:<version>`, from `catatonit`) and never
    /// pulls it; the real-Podman test asserts that image.
    pub fn pod_create(&self, pod: &str) -> Cmd {
        self.cmd()
            .arg("pod")
            .arg("create")
            .arg(format!("--name={pod}"))
            .arg("--network=none")
            .arg("--share=net")
            .arg(format!("--memory={}", self.limits.memory))
            .arg(format!("--memory-swap={}", self.limits.memory))
            .captured()
    }

    /// `podman create --name=<probe> --network=none <confined> --rootfs --
    /// <empty dir> /forge-isolation-probe`: a container made like a
    /// service (no `--volume` of its own), never started, from an empty
    /// directory instead of an image (no pull). Its mounts
    /// ([`PodmanPlan::mounts`]) are the ones containers.conf adds to every
    /// container: e.g. the podman/stable image's `/proc:/proc`, which
    /// gives a job the runner's processes and is added even next to a
    /// job's own `--volume` (Podman 5.3: `podman inspect` then hides it,
    /// so the probe has none). The runner refuses to start when there is
    /// any (FG-CI-0015).
    pub fn isolation_probe(&self, name: &str, rootfs: &Path) -> Cmd {
        self.confined(
            self.cmd()
                .arg("create")
                .arg(format!("--name={name}"))
                .arg("--network=none"),
        )
        .arg("--rootfs")
        .arg("--")
        .arg(rootfs.as_os_str())
        .arg(PROBE_COMMAND)
        .captured()
    }

    /// `podman inspect --type=container --format={{range .Mounts}}{{println
    /// .Type .Destination}}{{end}} -- <name>`: one `<type> <destination>`
    /// line per mount (`--tmpfs` is not listed).
    pub fn mounts(&self, name: &str) -> Cmd {
        self.cmd()
            .arg("inspect")
            .arg("--type=container")
            .arg("--format={{range .Mounts}}{{println .Type .Destination}}{{end}}")
            .arg("--")
            .arg(name)
            .captured()
    }

    /// `podman pod inspect --format={{.InfraContainerID}} -- <pod>` (the
    /// real-Podman test: which image the pod's infra container runs).
    pub fn pod_infra(&self, pod: &str) -> Cmd {
        self.cmd()
            .arg("pod")
            .arg("inspect")
            .arg("--format={{.InfraContainerID}}")
            .arg("--")
            .arg(pod)
            .captured()
    }

    /// `podman create --pod=<pod> --name=<svc> <confined> [--user=…]
    /// [--tmpfs=<path>:…,mode=1777]… [--env=K=V]… -- <image>`: a service
    /// runs its image's own entrypoint. The caller checked
    /// [`PodmanPlan::image_allowed`]; forge-ci checked the name, user, paths
    /// and environment (each one argv element; the image after `--`).
    pub fn service_create(&self, s: &ServiceContainer<'_>) -> Cmd {
        let mut cmd = self.confined(
            self.cmd()
                .arg("create")
                .arg(format!("--pod={}", s.pod))
                .arg(format!("--name={}", s.name)),
        );
        if let Some(user) = s.user {
            cmd = cmd.arg(format!("--user={user}"));
        }
        for path in s.tmpfs {
            cmd = cmd.arg(format!(
                "--tmpfs={path}:rw,nosuid,nodev,size={},mode=1777",
                self.limits.tmpfs
            ));
        }
        for (key, value) in s.env {
            cmd = cmd.arg(format!("--env={key}={value}"));
        }
        cmd.arg("--").arg(s.image).captured()
    }

    /// `podman start -- <svc>`: a service runs detached (its output stays
    /// in its own log, [`PodmanPlan::logs_tail`]).
    pub fn service_start(&self, name: &str) -> Cmd {
        self.cmd().arg("start").arg("--").arg(name).captured()
    }

    /// `podman logs --tail=<n> -- <svc>`: the end of a service's output
    /// (to the job's log after a failure).
    pub fn logs_tail(&self, name: &str) -> Cmd {
        self.cmd()
            .arg("logs")
            .arg(format!("--tail={SERVICE_LOG_LINES}"))
            .arg("--")
            .arg(name)
    }

    /// `podman pod rm --force --ignore --time=0 -- <pod>`: the pod and every
    /// container in it are gone whatever happened.
    pub fn pod_remove(&self, pod: &str) -> Cmd {
        self.cmd()
            .arg("pod")
            .arg("rm")
            .arg("--force")
            .arg("--ignore")
            .arg("--time=0")
            .arg("--")
            .arg(pod)
    }

    /// `podman pod ps --filter=name=^forge-job- --format={{.Name}}`: the
    /// job pods that exist (at startup: a crashed run's leftovers).
    pub fn stale_pods(&self) -> Cmd {
        self.cmd()
            .arg("pod")
            .arg("ps")
            .arg("--filter=name=^forge-job-")
            .arg("--format={{.Name}}")
            .captured()
    }

    /// `podman start --attach -- <name>`: runs the created container, its
    /// output to the job's log, its exit status the script's (125 is
    /// ambiguous: [`PodmanPlan::state`]).
    pub fn start(&self, name: &str) -> Cmd {
        self.cmd().arg("start").arg("--attach").arg("--").arg(name)
    }

    /// `podman inspect --type=container --format={{.State.Status}}
    /// {{.State.ExitCode}} -- <name>`, e.g. `exited 125` or `created 0`.
    pub fn state(&self, name: &str) -> Cmd {
        self.cmd()
            .arg("inspect")
            .arg("--type=container")
            .arg("--format={{.State.Status}} {{.State.ExitCode}}")
            .arg("--")
            .arg(name)
            .captured()
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
    /// whatever happened (no error when it never existed).
    pub fn remove(&self, name: &str) -> Cmd {
        self.cmd()
            .arg("rm")
            .arg("--force")
            .arg("--ignore")
            .arg("--")
            .arg(name)
    }

    /// `podman ps --all --filter=name=^forge-job- --format={{.Names}}`: the
    /// job containers that exist (at startup: a crashed run's leftovers).
    pub fn stale(&self) -> Cmd {
        self.cmd()
            .arg("ps")
            .arg("--all")
            .arg("--filter=name=^forge-job-")
            .arg("--format={{.Names}}")
            .captured()
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
        let cmd = plan.create(&Container {
            name: "forge-job-7-1",
            image: &image,
            work: Path::new("/r/job/work"),
            script_dir: Path::new("/r/job/script"),
            job_id: 7,
            pipeline_id: 3,
            project_id: 2,
            sha: &"b".repeat(40),
            pod: None,
        });
        let argv = cmd.argv();
        for flag in [
            "create",
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

    /// M4 C4 review: memory without swap on top, a bounded file size and
    /// open files, a bounded process count — all before `--`.
    #[test]
    fn the_run_argv_bounds_memory_files_and_processes() {
        let image = format!("registry.internal/rust@sha256:{}", digest());
        let limits = Limits {
            memory: "2g".to_owned(),
            pids: 512,
            tmpfs: "1g".to_owned(),
            file_size: "512m".to_owned(),
            nofile: 1024,
        };
        let plan = PodmanPlan::new(PathBuf::from("podman"), limits);
        let argv = plan
            .create(&Container {
                name: "forge-job-7-1",
                image: &image,
                work: Path::new("/r/job/work"),
                script_dir: Path::new("/r/job/script"),
                job_id: 7,
                pipeline_id: 3,
                project_id: 2,
                sha: &"b".repeat(40),
                pod: None,
            })
            .argv();
        let dash = argv.iter().position(|a| a == "--").expect("--");
        for flag in [
            "--memory=2g",
            "--memory-swap=2g",
            "--pids-limit=512",
            "--ulimit=fsize=536870912:536870912",
            "--ulimit=nofile=1024:1024",
        ] {
            let at = argv.iter().position(|a| a == flag);
            assert!(at.is_some_and(|i| i < dash), "{flag}: {argv:?}");
        }
        let defaults = Limits::default();
        assert_eq!(defaults.file_size, "4g");
        assert_eq!(defaults.nofile, 4096);
        assert_eq!(size_bytes("4g"), Some(4 * 1024 * 1024 * 1024));
        assert_eq!(size_bytes("16k"), Some(16 * 1024));
        assert_eq!(size_bytes("4x"), None);
    }

    /// Plan 14 C5s: the pod has no network; a service gets the job's
    /// confinement and limits, its own writable paths, its image after
    /// `--` and no command; the job joins the pod instead of its own
    /// `--network=none`.
    #[test]
    fn a_pod_and_its_services_are_confined_like_the_job() {
        let plan = PodmanPlan::new(PathBuf::from("podman"), Limits::default());
        assert_eq!(
            plan.pod_create("forge-job-7-1").argv(),
            [
                "pod",
                "create",
                "--name=forge-job-7-1",
                "--network=none",
                "--share=net",
                "--memory=4g",
                "--memory-swap=4g"
            ]
        );
        let pg = format!("registry.internal/postgres@sha256:{}", digest());
        let env = vec![("POSTGRES_PASSWORD".to_owned(), "a b;$(id)".to_owned())];
        let tmpfs = vec!["/var/run/postgresql".to_owned()];
        let argv = plan
            .service_create(&ServiceContainer {
                pod: "forge-job-7-1",
                name: "forge-job-7-1-svc-db",
                image: &pg,
                env: &env,
                user: Some("999:999"),
                tmpfs: &tmpfs,
            })
            .argv();
        let dash = argv.iter().position(|a| a == "--").expect("--");
        assert_eq!(&argv[dash + 1..], [pg.as_str()]);
        for flag in [
            "create",
            "--pod=forge-job-7-1",
            "--name=forge-job-7-1-svc-db",
            "--read-only",
            "--tmpfs=/tmp:rw,nosuid,nodev,size=1g",
            "--security-opt=no-new-privileges",
            "--cap-drop=all",
            "--pids-limit=4096",
            "--memory=4g",
            "--memory-swap=4g",
            "--ulimit=nofile=4096:4096",
            "--user=999:999",
            "--tmpfs=/var/run/postgresql:rw,nosuid,nodev,size=1g,mode=1777",
            // One argv element: the value never meets a shell.
            "--env=POSTGRES_PASSWORD=a b;$(id)",
        ] {
            let at = argv.iter().position(|a| a == flag);
            assert!(at.is_some_and(|i| i < dash), "{flag}: {argv:?}");
        }
        assert!(argv.iter().all(|a| !a.starts_with("--network")
            && !a.starts_with("--privileged")
            && !a.starts_with("--cap-add")
            && !a.starts_with("--entrypoint")));
        let job = plan
            .create(&Container {
                name: "forge-job-7-1",
                image: &pg,
                work: Path::new("/r/job/work"),
                script_dir: Path::new("/r/job/script"),
                job_id: 7,
                pipeline_id: 3,
                project_id: 2,
                sha: &"b".repeat(40),
                pod: Some("forge-job-7-1"),
            })
            .argv();
        assert!(job.iter().any(|a| a == "--pod=forge-job-7-1"), "{job:?}");
        assert!(job.iter().all(|a| !a.starts_with("--network")), "{job:?}");
        assert!(job.iter().any(|a| a == "--read-only"));
        assert_eq!(
            plan.service_start("forge-job-7-1-svc-db").argv(),
            ["start", "--", "forge-job-7-1-svc-db"]
        );
        assert_eq!(
            plan.logs_tail("forge-job-7-1-svc-db").argv(),
            ["logs", "--tail=50", "--", "forge-job-7-1-svc-db"]
        );
        assert_eq!(
            plan.pod_remove("forge-job-7-1").argv(),
            [
                "pod",
                "rm",
                "--force",
                "--ignore",
                "--time=0",
                "--",
                "forge-job-7-1"
            ]
        );
        assert_eq!(service_container_name(7, 1, "db"), "forge-job-7-1-svc-db");
    }

    /// The namespaces a job and its services get, whatever containers.conf
    /// sets (the podman/stable image sets `ipcns`/`utsns`/`cgroupns` to
    /// `host`): each container its own PID, IPC, UTS and cgroup namespace,
    /// in or out of a pod. A configured default `volumes` entry cannot be
    /// undone by a flag ([`PodmanPlan::isolation_probe`] refuses it).
    const PRIVATE_NS: [&str; 4] = [
        "--pid=private",
        "--ipc=private",
        "--uts=private",
        "--cgroupns=private",
    ];

    fn job<'a>(image: &'a str, pod: Option<&'a str>) -> Container<'a> {
        Container {
            name: "forge-job-7-1",
            image,
            work: Path::new("/r/job/work"),
            script_dir: Path::new("/r/job/script"),
            job_id: 7,
            pipeline_id: 3,
            project_id: 2,
            sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            pod,
        }
    }

    #[test]
    fn every_container_of_a_job_gets_private_namespaces_before_the_separator() {
        let image = format!("registry.internal/rust@sha256:{}", digest());
        let plan = PodmanPlan::new(PathBuf::from("podman"), Limits::default());
        let alone = plan.create(&job(&image, None)).argv();
        let in_pod = plan.create(&job(&image, Some("forge-job-7-1"))).argv();
        let service = plan
            .service_create(&ServiceContainer {
                pod: "forge-job-7-1",
                name: "forge-job-7-1-svc-db",
                image: &image,
                env: &[],
                user: None,
                tmpfs: &[],
            })
            .argv();
        for argv in [&alone, &in_pod, &service] {
            let dash = argv.iter().position(|a| a == "--").expect("--");
            for flag in PRIVATE_NS {
                let at = argv.iter().position(|a| a == flag);
                assert!(at.is_some_and(|i| i < dash), "{flag}: {argv:?}");
            }
            for host in ["--pid=host", "--ipc=host", "--uts=host", "--userns=host"] {
                assert!(argv.iter().all(|a| a != host), "{host}: {argv:?}");
            }
        }
    }

    /// The pod shares only its loopback network namespace: a pod's default
    /// `--share` (ipc, uts) would put the job and its services in the
    /// infra container's namespaces, which containers.conf may set to the
    /// runner's own (`ipcns = "host"`).
    #[test]
    fn a_pod_shares_only_its_network_namespace() {
        let plan = PodmanPlan::new(PathBuf::from("podman"), Limits::default());
        let argv = plan.pod_create("forge-job-7-1").argv();
        assert!(argv.iter().any(|a| a == "--share=net"), "{argv:?}");
        assert_eq!(
            argv.iter().filter(|a| a.starts_with("--share")).count(),
            1,
            "{argv:?}"
        );
    }

    /// The isolation probe is made like a service (the same confinement,
    /// no `--volume` of its own, which would hide the configured ones from
    /// `podman inspect`), from an empty directory after `--`.
    #[test]
    fn the_isolation_probe_is_a_service_like_container_without_volumes() {
        let plan = PodmanPlan::new(PathBuf::from("podman"), Limits::default());
        let argv = plan
            .isolation_probe(
                "forge-isolation-probe-42",
                Path::new("/r/runner/empty-rootfs"),
            )
            .argv();
        let dash = argv.iter().position(|a| a == "--").expect("--");
        assert_eq!(argv[0], "create");
        assert_eq!(&argv[dash + 1..], ["/r/runner/empty-rootfs", PROBE_COMMAND]);
        for flag in PRIVATE_NS.iter().copied().chain([
            "--name=forge-isolation-probe-42",
            "--network=none",
            "--read-only",
            "--cap-drop=all",
            "--rootfs",
        ]) {
            let at = argv.iter().position(|a| a == flag);
            assert!(at.is_some_and(|i| i < dash), "{flag}: {argv:?}");
        }
        assert!(
            argv.iter()
                .all(|a| !a.starts_with("--volume") && !a.starts_with("--mount")),
            "{argv:?}"
        );
        assert_eq!(
            plan.mounts("forge-isolation-probe-42").argv(),
            [
                "inspect",
                "--type=container",
                "--format={{range .Mounts}}{{println .Type .Destination}}{{end}}",
                "--",
                "forge-isolation-probe-42"
            ]
        );
    }
}
