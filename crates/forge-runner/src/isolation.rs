//! The runner's start-up check that a job's container is isolated from the
//! runner (ADR-0029 amendment 2026-11-12, FG-CI-0015).
//!
//! Every container of a job gets its own PID, IPC, UTS and cgroup
//! namespaces by flag ([`PodmanPlan`]'s confinement), whatever
//! containers.conf says. One setting has no flag that undoes it: a
//! default `volumes` entry (`[containers] volumes = [...]`) is mounted into
//! every container, next to the container's own `--volume`s. The
//! quay.io/podman/stable image ships `volumes = ["/proc:/proc"]`, which
//! gives a job the runner's `/proc`: every process of the runner, of other
//! jobs and of their services. So before it claims a job (and in `check`)
//! the runner creates one probe container like a service — never started,
//! from an empty directory, no pull — reads its mounts and refuses to start
//! when there is any.
//!
//! ```text
//! podman rm --force --ignore -- forge-isolation-probe-<pid>      (a leftover)
//! podman create --name=forge-isolation-probe-<pid> … --rootfs -- <empty dir> /forge-isolation-probe
//! podman inspect --type=container --format=… -- forge-isolation-probe-<pid>
//! podman rm --force --ignore -- forge-isolation-probe-<pid>      (always)
//! ```

use std::path::Path;
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use crate::podman::PodmanPlan;
use crate::process::{Exit, ProcessRunner, Ran};

/// The limit of each probe call.
const PROBE_CALL: Duration = Duration::from_secs(60);

/// `forge-isolation-probe-<n>` (the runner's process id): not a
/// `forge-job-` name, so the stale-container cleanup never sees it.
pub fn probe_name(id: u32) -> String {
    format!("forge-isolation-probe-{id}")
}

fn unsafe_isolation(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::JobIsolationUnsafe).with_context("reason", reason)
}

fn podman_failed(step: &'static str, ran: Ran) -> ForgeError {
    let e = unsafe_isolation("probe_failed").with_context("step", step);
    let e = match ran.exit {
        Exit::Code(code) => e.with_context("exit_code", code),
        Exit::TimedOut => e.with_context("timeout_ms", PROBE_CALL.as_millis()),
        Exit::Stopped => e,
    };
    e.with_context("podman_stderr", ran.stderr_tail)
}

/// Creates the probe container `name` from the empty directory `rootfs`,
/// reads its mounts and removes it. `Ok` when containers.conf adds no
/// mount; FG-CI-0015 `reason=default_volumes` (with `mounts`) when it adds
/// any, `reason=probe_failed` (with `step`) when Podman failed.
pub fn check(
    proc: &dyn ProcessRunner,
    podman: &PodmanPlan,
    name: &str,
    rootfs: &Path,
) -> Result<(), ForgeError> {
    let call = |step: &'static str, cmd| {
        proc.run(&cmd, PROBE_CALL, &|| false, &mut |_| {})
            .map_err(|e| {
                unsafe_isolation("probe_failed")
                    .with_context("step", step)
                    .with_source(e)
            })
    };
    let leftover = call("remove", podman.remove(name))?;
    if !leftover.ok() {
        return Err(podman_failed("remove", leftover));
    }
    let inspected = (|| {
        let created = call("create", podman.isolation_probe(name, rootfs))?;
        if !created.ok() {
            return Err(podman_failed("create", created));
        }
        let mounts = call("inspect", podman.mounts(name))?;
        if !mounts.ok() {
            return Err(podman_failed("inspect", mounts));
        }
        Ok(mounts.stdout)
    })();
    // Always removed, whatever happened (it was never started).
    let removed = call("remove", podman.remove(name));
    let stdout = inspected?;
    let removed = removed?;
    if !removed.ok() {
        return Err(podman_failed("remove", removed));
    }
    let mounts: Vec<String> = String::from_utf8_lossy(&stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    if mounts.is_empty() {
        return Ok(());
    }
    Err(unsafe_isolation("default_volumes").with_context("mounts", mounts.join(", ")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::podman::Limits;
    use crate::process::Cmd;

    /// Answers `create`, `inspect` and `rm` and records every call.
    struct Fake {
        calls: Mutex<Vec<Vec<String>>>,
        /// What `inspect` prints.
        mounts: &'static str,
        /// The call (`create` / `inspect` / `rm`) that exits 125.
        fails: Option<&'static str>,
    }

    impl Fake {
        fn new(mounts: &'static str, fails: Option<&'static str>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                mounts,
                fails,
            }
        }

        fn subcommands(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|a| a[0].clone())
                .collect()
        }
    }

    impl ProcessRunner for Fake {
        fn run(
            &self,
            cmd: &Cmd,
            _limit: Duration,
            _stop: &dyn Fn() -> bool,
            _out: &mut dyn FnMut(&[u8]),
        ) -> std::io::Result<Ran> {
            let argv = cmd.argv();
            self.calls.lock().unwrap().push(argv.clone());
            if self.fails == Some(argv[0].as_str()) {
                return Ok(Ran {
                    exit: Exit::Code(125),
                    stdout: Vec::new(),
                    stderr_tail: "Error: crun: something".to_owned(),
                });
            }
            let stdout = if argv[0] == "inspect" {
                self.mounts.as_bytes().to_vec()
            } else {
                Vec::new()
            };
            Ok(Ran {
                exit: Exit::Code(0),
                stdout,
                stderr_tail: String::new(),
            })
        }
    }

    fn plan() -> PodmanPlan {
        PodmanPlan::new(PathBuf::from("podman"), Limits::default())
    }

    fn context<'a>(err: &'a ForgeError, key: &str) -> Option<&'a str> {
        err.context()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn no_configured_mount_passes_and_the_probe_is_removed() {
        // `println` leaves one empty line per container without mounts.
        let fake = Fake::new("\n", None);
        let rootfs = Path::new("/r/runner/empty-rootfs");
        check(&fake, &plan(), "forge-isolation-probe-42", rootfs).expect("isolated");
        assert_eq!(fake.subcommands(), ["rm", "create", "inspect", "rm"]);
        let calls = fake.calls.lock().unwrap();
        assert_eq!(
            calls[1],
            plan()
                .isolation_probe("forge-isolation-probe-42", rootfs)
                .argv()
        );
        assert_eq!(calls[2], plan().mounts("forge-isolation-probe-42").argv());
        assert_eq!(
            calls[3],
            [
                "rm",
                "--force",
                "--ignore",
                "--",
                "forge-isolation-probe-42"
            ]
        );
        assert_eq!(probe_name(42), "forge-isolation-probe-42");
    }

    /// The finding: the podman/stable image's `volumes = ["/proc:/proc"]`
    /// gave every job the runner's processes. The runner refuses to start.
    #[test]
    fn configured_default_volumes_stop_the_runner_before_any_job() {
        let fake = Fake::new("bind /proc\n\n", None);
        let err = check(&fake, &plan(), "forge-isolation-probe-42", Path::new("/e"))
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::JobIsolationUnsafe);
        assert_eq!(err.code().id(), "FG-CI-0015");
        assert_eq!(context(&err, "reason"), Some("default_volumes"));
        assert_eq!(context(&err, "mounts"), Some("bind /proc"));
        // Removed all the same.
        assert_eq!(fake.subcommands(), ["rm", "create", "inspect", "rm"]);
    }

    #[test]
    fn a_podman_failure_is_reported_with_its_step_and_the_probe_is_removed() {
        for (fails, step, calls) in [
            ("create", "create", vec!["rm", "create", "rm"]),
            ("inspect", "inspect", vec!["rm", "create", "inspect", "rm"]),
            ("rm", "remove", vec!["rm"]),
        ] {
            let fake = Fake::new("\n", Some(fails));
            let err = check(&fake, &plan(), "forge-isolation-probe-42", Path::new("/e"))
                .expect_err(fails);
            assert_eq!(err.code(), ErrorCode::JobIsolationUnsafe, "{fails}");
            assert_eq!(context(&err, "reason"), Some("probe_failed"), "{fails}");
            assert_eq!(context(&err, "step"), Some(step), "{fails}");
            assert_eq!(
                context(&err, "podman_stderr"),
                Some("Error: crun: something"),
                "{fails}"
            );
            assert_eq!(fake.subcommands(), calls, "{fails}");
        }
    }
}
