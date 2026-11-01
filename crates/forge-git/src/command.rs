//! The single git subprocess wrapper.
//!
//! Argument layout (CLAUDE.md §3-5):
//!
//! ```text
//! git [-c key=value]* <subcommand> <options>* [--end-of-options <revisions>*] [-- <paths>*]
//! ```
//!
//! - options are `&'static str` (or crate-internal forge-generated values), so
//!   user input can never reach the option position;
//! - revisions (refs, object ids) follow `--end-of-options`, paths follow `--`
//!   — the two are different for git (`git log -- main` means the *path* main);
//! - the environment is cleared and rebuilt (no inherited `GIT_*`), the target
//!   repository is pinned with `GIT_DIR`, the global config is forge-owned;
//! - stdin, stdout and stderr are pumped concurrently (no pipe deadlock),
//!   stdout is capped, there is a total timeout, a global concurrency limit,
//!   and the exit status is checked.
//!
//! Captured runs ([`GitCommand::output`]) kill only the direct child on
//! timeout (they are short and spawn no long-lived helpers). Streaming runs
//! ([`GitCommand::spawn_stream`], `stream.rs`) kill the whole process group
//! (Unix) / Job Object (Windows), helpers such as `pack-objects` included.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use crate::config::GitConfig;

use forge_errors::{ErrorCode, ForgeError};

use crate::process::{ignore_broken_pipe, read_capped, reap, with_reap};
use crate::redact::stderr_head;
pub use crate::revision::Revision;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

/// Captured stderr is for logs only; keep a bounded tail.
pub(crate) const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Handle shared by everything that runs git. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Git {
    config: Arc<GitConfig>,
    slots: Arc<Semaphore>,
    pub(crate) streams: crate::stream::StreamLimits,
}

/// Captured result of a successful invocation. stderr is not exposed: it may
/// contain paths and must never reach a user (CLAUDE.md §3-3).
#[derive(Debug)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
}

impl GitOutput {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

impl Git {
    pub fn new(config: GitConfig) -> Self {
        let slots = Arc::new(Semaphore::new(config.max_concurrency.max(1)));
        let streams = crate::stream::StreamLimits::new(&config);
        Self {
            config: Arc::new(config),
            slots,
            streams,
        }
    }

    pub fn config(&self) -> &GitConfig {
        &self.config
    }

    /// Start building `git <subcommand> …`.
    pub fn command(&self, subcommand: &'static str) -> GitCommand {
        GitCommand {
            git: self.clone(),
            subcommand,
            config: Vec::new(),
            options: Vec::new(),
            revisions: Vec::new(),
            paths: Vec::new(),
            git_dir: None,
            cwd: None,
            envs: Vec::new(),
            stdin: None,
            #[cfg(test)]
            hold_stdin: false,
            option_like_value: false,
            timeout: None,
            idle_timeout: None,
        }
    }
}

/// The `Git-Protocol` value git may see, or `None` (dropped): non-empty, at
/// most 256 bytes, only `[A-Za-z0-9=:._-]`.
pub(crate) fn sanitize_git_protocol(value: &str) -> Option<&str> {
    let valid = !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"=:._-".contains(&b));
    valid.then_some(value)
}

/// One git invocation being built.
#[derive(Debug)]
pub struct GitCommand {
    pub(crate) git: Git,
    pub(crate) subcommand: &'static str,
    config: Vec<(&'static str, OsString)>,
    options: Vec<OsString>,
    revisions: Vec<OsString>,
    paths: Vec<OsString>,
    git_dir: Option<PathBuf>,
    cwd: Option<PathBuf>,
    envs: Vec<(&'static str, OsString)>,
    pub(crate) stdin: Option<Vec<u8>>,
    /// Keep stdin open without writing (tests: simulate a stalled client).
    #[cfg(test)]
    hold_stdin: bool,
    /// Set when a forge-generated value starts with `-` (refused at run time).
    pub(crate) option_like_value: bool,
    /// `None`: `GitConfig::timeout_total` (captured) /
    /// `GitConfig::stream_timeout_total` (streaming).
    pub(crate) timeout: Option<Duration>,
    /// Streaming only; `None`: `GitConfig::stream_idle_timeout`.
    pub(crate) idle_timeout: Option<Duration>,
}

/// Variables passed through from the server process. Everything else
/// (notably every `GIT_*`) is dropped.
const PASSTHROUGH_ENV: &[&str] = &["PATH", "SystemRoot", "SYSTEMROOT", "TEMP", "TMP", "TMPDIR"];

impl GitCommand {
    /// A fixed option or argument. `&'static str` keeps user input out of the
    /// option position by construction.
    #[must_use]
    pub fn arg(mut self, arg: &'static str) -> Self {
        self.options.push(OsString::from(arg));
        self
    }

    /// A forge-generated value in the option position (config values, paths
    /// forge derived itself). Crate-internal on purpose.
    #[must_use]
    pub(crate) fn arg_owned(mut self, arg: impl AsRef<OsStr>) -> Self {
        let arg = arg.as_ref();
        // A forge-generated value must never look like an option; `output()`
        // refuses to run such a command instead of passing it to git.
        if arg.to_string_lossy().starts_with('-') {
            self.option_like_value = true;
        }
        self.options.push(arg.to_os_string());
        self
    }

    /// `--exclude=<ref>` (rev-list: leave this ref out of the next
    /// `--all` / `--glob`). A validated `RefName` starts with `refs/` and holds no
    /// glob character, so the pattern names exactly that ref.
    #[must_use]
    pub(crate) fn exclude_ref(mut self, name: &forge_core::RefName) -> Self {
        self.options
            .push(OsString::from(format!("--exclude={}", name.as_str())));
        self
    }

    /// A revision (ref or object id), placed after `--end-of-options`.
    #[must_use]
    pub fn revision(mut self, rev: impl Into<Revision>) -> Self {
        self.revisions.push(OsString::from(rev.into().0));
        self
    }

    /// A path or pathspec, placed after `--`.
    #[must_use]
    pub fn path(mut self, path: impl AsRef<OsStr>) -> Self {
        self.paths.push(path.as_ref().to_os_string());
        self
    }

    /// `git -c key=value` for this invocation only (e.g. forcing
    /// `core.hooksPath` on every receive-pack). Crate-internal: `-c` counts as
    /// protected config (hooks, pack-objects hook), so values are forge-owned.
    #[must_use]
    pub(crate) fn config_override(mut self, key: &'static str, value: impl AsRef<OsStr>) -> Self {
        self.config.push((key, value.as_ref().to_os_string()));
        self
    }

    /// Pin the repository (`GIT_DIR`) and run inside it — git never discovers
    /// a repository by walking up from the working directory.
    #[must_use]
    pub fn repo(mut self, git_dir: impl AsRef<Path>) -> Self {
        let dir = git_dir.as_ref().to_path_buf();
        self.cwd = Some(dir.clone()); // clone: the same path is both GIT_DIR and cwd
        self.git_dir = Some(dir);
        self
    }

    /// Working directory without a repository (discovery never leaves this
    /// directory: its parent is set as `GIT_CEILING_DIRECTORIES`).
    #[must_use]
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Pass the client's `Git-Protocol` header as `GIT_PROTOCOL` (the only
    /// git variable a caller may set). Anything but `key[=value]` pairs
    /// separated by `:` (e.g. `version=2`) is dropped, so a client cannot
    /// smuggle other settings.
    #[must_use]
    pub fn git_protocol(mut self, value: &str) -> Self {
        if let Some(valid) = sanitize_git_protocol(value) {
            self.envs.push(("GIT_PROTOCOL", OsString::from(valid)));
        }
        self
    }

    #[must_use]
    pub fn stdin(mut self, input: Vec<u8>) -> Self {
        self.stdin = Some(input);
        self
    }

    /// A caller-set environment variable (tests: what git would see).
    #[cfg(test)]
    pub(crate) fn env_value(&self, key: &str) -> Option<&OsStr> {
        self.envs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_os_str())
    }

    #[cfg(test)]
    #[must_use]
    fn hold_stdin_open(mut self) -> Self {
        self.hold_stdin = true;
        self
    }

    /// Total timeout for this invocation (captured or streaming).
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Idle timeout for a stream: no bytes in either direction for this long
    /// kills it (FG-GIT-0002 `kind=idle`). Ignored by [`GitCommand::output`].
    #[must_use]
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = Some(timeout);
        self
    }

    fn total_timeout(&self) -> Duration {
        self.timeout.unwrap_or(self.git.config().timeout_total)
    }

    /// The full argument vector after `git` (for tests and logs).
    pub fn argv(&self) -> Vec<OsString> {
        let mut v = Vec::new();
        for (key, value) in &self.config {
            let mut kv = OsString::from(*key);
            kv.push("=");
            kv.push(value);
            v.push(OsString::from("-c"));
            v.push(kv);
        }
        v.push(OsString::from(self.subcommand));
        v.extend(self.options.iter().cloned());
        if !self.revisions.is_empty() {
            v.push(OsString::from("--end-of-options"));
            v.extend(self.revisions.iter().cloned());
        }
        if !self.paths.is_empty() {
            v.push(OsString::from("--"));
            v.extend(self.paths.iter().cloned());
        }
        v
    }

    /// Build the `tokio::process::Command` with the scrubbed environment.
    /// `stream`: stdin is always piped (the caller writes it).
    pub(crate) fn build(&self, stream: bool) -> tokio::process::Command {
        let config = self.git.config();
        let mut cmd = tokio::process::Command::new(&config.bin);
        cmd.env_clear();
        for key in PASSTHROUGH_ENV {
            if let Some(v) = std::env::var_os(key) {
                cmd.env(key, v);
            }
        }
        // Caller-provided variables first (only GIT_PROTOCOL can get here);
        // forge's pinned variables are applied after, so nothing overrides them.
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
        cmd.env("HOME", &config.home)
            .env("GIT_CONFIG_GLOBAL", config.home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            // Replace refs could make a reviewed SHA show other content.
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            // Pathspec magic (`:/`, `:(glob)`) could escape a scoped path.
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("LC_ALL", "C")
            .env("LANG", "C");
        // Without an explicit directory git runs in forge's own home, never in
        // the server's inherited working directory.
        let cwd = self.cwd.as_deref().unwrap_or(&config.home);
        if let Some(dir) = &self.git_dir {
            cmd.env("GIT_DIR", dir);
        } else if let Some(parent) = cwd.parent() {
            // Ceilings are directories git must not move *up into*: stop at the
            // working directory itself.
            cmd.env("GIT_CEILING_DIRECTORIES", parent);
        }
        cmd.args(self.argv());
        cmd.current_dir(cwd);
        cmd.stdin(if stream || self.wants_stdin() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        cmd
    }

    fn wants_stdin(&self) -> bool {
        #[cfg(test)]
        if self.hold_stdin {
            return true;
        }
        self.stdin.is_some()
    }

    pub(crate) fn fail(&self, code: ErrorCode) -> ForgeError {
        ForgeError::new(code).with_context("subcommand", self.subcommand)
    }

    fn timed_out(&self) -> ForgeError {
        self.fail(ErrorCode::GitTimeout)
            .with_context("kind", "total")
            .with_context("timeout_ms", self.total_timeout().as_millis())
    }

    /// Run to completion and capture stdout.
    ///
    /// Non-zero exit → `FG-GIT-0003`, timeout → `FG-GIT-0002` (child killed and
    /// reaped), stdout over the cap → `FG-GIT-0006`, no free slot → `FG-GIT-0004`,
    /// binary not runnable → `FG-GIT-0001`.
    pub async fn output(self) -> Result<GitOutput, ForgeError> {
        let cap = self.git.config().max_output_bytes;
        self.capture(cap, false).await.map(|(out, _)| out)
    }

    /// Run and keep at most `cap` bytes of stdout (never more than
    /// `max_output_bytes`). More output is not an error: git is killed and
    /// reaped as soon as the cap is passed, and the result says
    /// `truncated = true` (the browse limits, M2 MR7). A killed git that
    /// cannot be confirmed reaped is FG-GIT-0003 `reason=stopped_early`
    /// with `reap_failed`.
    pub(crate) async fn output_truncated(
        self,
        cap: usize,
    ) -> Result<(GitOutput, bool), ForgeError> {
        let cap = cap.min(self.git.config().max_output_bytes);
        self.capture(cap, true).await
    }

    async fn capture(
        mut self,
        max_out: usize,
        truncate: bool,
    ) -> Result<(GitOutput, bool), ForgeError> {
        if self.option_like_value {
            // A forge bug, never user input (user input cannot reach arg_owned).
            return Err(self
                .fail(ErrorCode::Internal)
                .with_context("reason", "option_like_generated_value"));
        }
        let git = self.git.clone(); // clone: Arc handle; `self` is used mutably below
        let config = git.config();
        let _permit = match tokio::time::timeout(
            config.acquire_timeout,
            Arc::clone(&git.slots).acquire_owned(),
        )
        .await
        {
            Err(_) => return Err(self.fail(ErrorCode::GitBusy)),
            // A closed semaphore is a bug, not load.
            Ok(Err(e)) => return Err(self.fail(ErrorCode::Internal).with_source(e)),
            Ok(Ok(permit)) => permit,
        };

        let started = Instant::now();
        // One deadline for the whole run (I/O and the final wait together).
        let deadline = tokio::time::Instant::now() + self.total_timeout();
        let mut child = self.build(false).spawn().map_err(|e| {
            self.fail(ErrorCode::GitUnavailable)
                .with_context("reason", "spawn_failed")
                .with_context("git_bin", config.bin.display())
                .with_source(e)
        })?;

        let stdin_pipe = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let input = self.stdin.take();
        #[cfg(test)]
        let hold = self.hold_stdin;
        #[cfg(not(test))]
        let hold = false;

        let write = async move {
            let Some(mut pipe) = stdin_pipe else {
                return Ok::<(), io::Error>(());
            };
            if hold {
                // Keep the pipe open forever; only the timeout ends this.
                std::future::pending::<()>().await;
            }
            if let Some(data) = input {
                ignore_broken_pipe(pipe.write_all(&data).await)?;
                ignore_broken_pipe(pipe.shutdown().await)?;
            }
            Ok(())
        };
        let child_ref = &mut child;
        let io = async move {
            let out = async {
                let r = read_capped(stdout, max_out).await;
                if matches!(r, Ok((_, true))) {
                    // Over the cap: stop git now. Otherwise it blocks writing to
                    // the undrained pipe, stderr never closes and the run would
                    // wait for the full timeout while holding a slot.
                    let _ = child_ref.start_kill(); // ignore: reaped below
                }
                r
            };
            tokio::join!(write, out, read_capped(stderr, MAX_STDERR_BYTES))
        };

        let Ok((write_res, out_res, err_res)) = tokio::time::timeout_at(deadline, io).await else {
            let reaped = reap(&mut child).await;
            return Err(with_reap(self.timed_out(), reaped));
        };
        let (stderr_bytes, _) = err_res.unwrap_or_default();
        let stderr_head = stderr_head(&stderr_bytes);

        let (stdout_bytes, overflow) = match out_res {
            Ok(r) => r,
            Err(e) => {
                let reaped = reap(&mut child).await;
                return Err(with_reap(
                    self.fail(ErrorCode::GitProcessFailed).with_source(e),
                    reaped,
                ));
            }
        };
        if overflow && truncate {
            // Enough read: git was killed at the cap; confirm it is gone.
            return match reap(&mut child).await {
                Ok(()) => Ok((
                    GitOutput {
                        stdout: stdout_bytes,
                    },
                    true,
                )),
                Err(failure) => Err(failure.attach(
                    self.fail(ErrorCode::GitProcessFailed)
                        .with_context("reason", "stopped_early"),
                )),
            };
        }
        if overflow {
            let reaped = reap(&mut child).await;
            return Err(with_reap(
                self.fail(ErrorCode::GitOutputTooLarge)
                    .with_context("max_output_bytes", max_out),
                reaped,
            ));
        }

        let status = match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                return Err(self
                    .fail(ErrorCode::GitProcessFailed)
                    .with_context("stderr", stderr_head)
                    .with_source(e))
            }
            Err(_) => {
                let reaped = reap(&mut child).await;
                return Err(with_reap(self.timed_out(), reaped));
            }
        };
        let exit_code = status
            .code()
            .map_or_else(|| "signal".to_string(), |c| c.to_string());
        tracing::debug!(
            target: "forge_git::exec",
            subcommand = self.subcommand,
            duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            exit_code = %exit_code,
            "git.exec"
        );

        if !status.success() {
            return Err(self
                .fail(ErrorCode::GitProcessFailed)
                .with_context("exit_code", exit_code)
                .with_context("stderr", stderr_head));
        }
        if let Err(e) = write_res {
            // git succeeded but did not receive all input: not a success.
            return Err(self.fail(ErrorCode::GitProcessFailed).with_source(e));
        }
        Ok((
            GitOutput {
                stdout: stdout_bytes,
            },
            false,
        ))
    }
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
