//! Killing a streaming git subprocess together with its helpers.
//!
//! `git upload-pack` / `receive-pack` start helpers (`pack-objects`,
//! `index-pack`, `rev-list`). Killing only the direct child would leave them
//! running and holding the repository (and its `.lock` files).
//!
//! - Unix (the server): the child starts its own process group
//!   (`process_group(0)`), and the whole group gets `SIGKILL`. The group is
//!   killed while the leader is still unreaped, so its id cannot have been
//!   reused by another process. When the leader exits by itself, its exit is
//!   first observed with `waitid(WNOWAIT)` (it stays a zombie, its id stays
//!   taken); on a failure (non-zero or a signal) the group is killed then,
//!   so helpers left behind (`pack-objects`, `index-pack`) do not keep
//!   running and holding repository locks. Only then is the leader reaped.
//! - Windows (development machines only): the child is put into a Job Object
//!   with kill-on-close right after spawn; closing the job kills every
//!   process in it. git starts helpers only after reading the request, so
//!   the window between spawn and assignment does not matter in practice.
//! - Other platforms are refused at compile time.
//! - Dropping a `ProcessTree` that was neither killed nor released (the
//!   supervisor task was dropped, e.g. at runtime shutdown) kills the tree.

#[cfg(not(any(unix, windows)))]
compile_error!(
    "forge-git streaming needs process groups (Unix) or Job Objects (Windows) \
     to kill git together with its helpers"
);

use std::io;

/// The kill handle of one streaming subprocess.
#[derive(Debug)]
pub(crate) struct ProcessTree {
    #[cfg(unix)]
    pgid: Option<rustix::process::Pid>,
    /// The leader's pid (equal to the group id), for `waitid`.
    #[cfg(unix)]
    leader: Option<rustix::process::Pid>,
    #[cfg(windows)]
    job: Option<win32job::Job>,
}

/// Prepare the command so that its descendants can be killed with it.
pub(crate) fn configure(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        // 0: a new group whose id is the child's pid.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        let _ = cmd; // the job is attached after spawn
    }
}

impl ProcessTree {
    /// Attach to a freshly spawned child.
    pub(crate) fn attach(child: &tokio::process::Child) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let pgid = child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .and_then(rustix::process::Pid::from_raw);
            Ok(Self { pgid, leader: pgid })
        }
        #[cfg(windows)]
        {
            let Some(handle) = child.raw_handle() else {
                return Ok(Self { job: None }); // already exited and reaped
            };
            let mut info = win32job::ExtendedLimitInfo::new();
            info.limit_kill_on_job_close();
            let job = win32job::Job::create_with_limit_info(&info).map_err(io::Error::other)?;
            // A raw HANDLE is a pointer-sized integer for the Win32 API.
            job.assign_process(handle as isize)
                .map_err(io::Error::other)?;
            Ok(Self { job: Some(job) })
        }
    }

    /// Kill every process of the tree (`SIGKILL` to the group / close the
    /// job). Idempotent; a group that is already gone is not an error.
    pub(crate) fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            match rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL) {
                Ok(()) => {}
                Err(e) if e == rustix::io::Errno::SRCH => {}
                Err(e) => tracing::warn!(
                    target: "forge_git::stream",
                    error = %e,
                    "git.stream.killpg_failed (the direct child is still killed)"
                ),
            }
        }
        #[cfg(windows)]
        {
            // Kill-on-close: dropping the last handle ends every process in it.
            drop(self.job.take());
        }
    }

    /// Whether the leader has exited, without reaping it (`waitid` with
    /// `WNOWAIT | WNOHANG`): `Some(true)` exit 0, `Some(false)` non-zero or a
    /// signal, `None` still running.
    #[cfg(unix)]
    pub(crate) fn peek_leader(&self) -> io::Result<Option<bool>> {
        use rustix::process::{waitid, WaitId, WaitIdOptions};
        let Some(pid) = self.leader else {
            return Err(io::Error::other("no leader pid"));
        };
        let opts = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        match waitid(WaitId::Pid(pid), opts) {
            Ok(Some(status)) => Ok(Some(status.exited() && status.exit_status() == Some(0))),
            Ok(None) => Ok(None),
            Err(e) => Err(io::Error::from(e)),
        }
    }

    /// The leader exited with status 0 and the tree is no longer ours to
    /// kill. Unix: nothing to do (a successful git has waited for its
    /// helpers; the group id must not be used after the leader is reaped).
    /// Windows: closing the job ends any helper left behind.
    pub(crate) fn release(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
        #[cfg(windows)]
        {
            drop(self.job.take());
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        // No-op after `kill` / `release`.
        self.kill();
    }
}
