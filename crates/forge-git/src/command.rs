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
//! Known gap (M1 phase 9, streaming): only the direct child is killed on
//! timeout; helpers such as `pack-objects` must be killed as a process group
//! (Unix) / Job Object (Windows) once long-running streaming commands land.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::RefName;
use forge_errors::{ErrorCode, ForgeError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

/// How git is run. Loaded from configuration in `forge serve` / `worker`.
#[derive(Debug, Clone)]
pub struct GitConfig {
    /// Path to the git binary (or `git` to resolve via `PATH`).
    pub bin: PathBuf,
    /// The `HOME` git sees — a forge-owned directory; the global config is
    /// pinned to `<home>/.gitconfig`, so no user or shared config applies.
    pub home: PathBuf,
    /// Upper bound for one invocation.
    pub timeout_total: Duration,
    /// How long to wait for a free subprocess slot.
    pub acquire_timeout: Duration,
    /// Maximum concurrent git subprocesses.
    pub max_concurrency: usize,
    /// Maximum captured stdout per invocation.
    pub max_output_bytes: usize,
}

/// Captured stderr is for logs only; keep a bounded tail.
const MAX_STDERR_BYTES: usize = 64 * 1024;

impl GitConfig {
    pub fn new(bin: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            home: home.into(),
            timeout_total: Duration::from_secs(300),
            acquire_timeout: Duration::from_secs(10),
            max_concurrency: 32,
            max_output_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Handle shared by everything that runs git. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Git {
    config: Arc<GitConfig>,
    slots: Arc<Semaphore>,
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
        Self {
            config: Arc::new(config),
            slots,
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
            timeout: self.config.timeout_total,
        }
    }
}

/// A revision operand: a validated ref name or a full object id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision(String);

impl Revision {
    /// A full hex object id (40 for SHA-1, 64 for SHA-256).
    pub fn object_id(hex: &str) -> Result<Self, ForgeError> {
        let ok = matches!(hex.len(), 40 | 64) && hex.bytes().all(|b| b.is_ascii_hexdigit());
        if ok {
            Ok(Self(hex.to_ascii_lowercase()))
        } else {
            Err(ForgeError::new(ErrorCode::InvalidRefName).with_context("reason", "bad_object_id"))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&RefName> for Revision {
    fn from(r: &RefName) -> Self {
        Self(r.as_str().to_string())
    }
}

/// One git invocation being built.
#[derive(Debug)]
pub struct GitCommand {
    git: Git,
    subcommand: &'static str,
    config: Vec<(&'static str, OsString)>,
    options: Vec<OsString>,
    revisions: Vec<OsString>,
    paths: Vec<OsString>,
    git_dir: Option<PathBuf>,
    cwd: Option<PathBuf>,
    envs: Vec<(&'static str, OsString)>,
    stdin: Option<Vec<u8>>,
    /// Keep stdin open without writing (tests: simulate a stalled client).
    #[cfg(test)]
    hold_stdin: bool,
    timeout: Duration,
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
        self.options.push(arg.as_ref().to_os_string());
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
    /// `core.hooksPath` on every receive-pack).
    #[must_use]
    pub fn config_override(mut self, key: &'static str, value: impl AsRef<OsStr>) -> Self {
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

    /// An explicitly intended git variable (e.g. `GIT_PROTOCOL`).
    #[must_use]
    pub fn env(mut self, key: &'static str, value: impl AsRef<OsStr>) -> Self {
        self.envs.push((key, value.as_ref().to_os_string()));
        self
    }

    #[must_use]
    pub fn stdin(mut self, input: Vec<u8>) -> Self {
        self.stdin = Some(input);
        self
    }

    #[cfg(test)]
    #[must_use]
    fn hold_stdin_open(mut self) -> Self {
        self.hold_stdin = true;
        self
    }

    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
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
    fn build(&self) -> tokio::process::Command {
        let config = self.git.config();
        let mut cmd = tokio::process::Command::new(&config.bin);
        cmd.env_clear();
        for key in PASSTHROUGH_ENV {
            if let Some(v) = std::env::var_os(key) {
                cmd.env(key, v);
            }
        }
        cmd.env("HOME", &config.home)
            .env("GIT_CONFIG_GLOBAL", config.home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("LANG", "C");
        if let Some(dir) = &self.git_dir {
            cmd.env("GIT_DIR", dir);
        } else if let Some(parent) = self.cwd.as_deref().and_then(Path::parent) {
            // Ceilings are directories git must not move *up into*: stop at the
            // working directory itself.
            cmd.env("GIT_CEILING_DIRECTORIES", parent);
        }
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
        cmd.args(self.argv());
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        cmd.stdin(if self.wants_stdin() {
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

    fn fail(&self, code: ErrorCode) -> ForgeError {
        ForgeError::new(code).with_context("subcommand", self.subcommand)
    }

    fn timed_out(&self) -> ForgeError {
        self.fail(ErrorCode::GitTimeout)
            .with_context("kind", "total")
            .with_context("timeout_ms", self.timeout.as_millis())
    }

    /// Run to completion and capture stdout.
    ///
    /// Non-zero exit → `FG-GIT-0003`, timeout → `FG-GIT-0002` (child killed and
    /// reaped), stdout over the cap → `FG-GIT-0006`, no free slot → `FG-GIT-0004`,
    /// binary not runnable → `FG-GIT-0001`.
    pub async fn output(mut self) -> Result<GitOutput, ForgeError> {
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
        let mut child = self.build().spawn().map_err(|e| {
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
        let max_out = config.max_output_bytes;

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
        let io = async {
            tokio::join!(
                write,
                read_capped(stdout, max_out),
                read_capped(stderr, MAX_STDERR_BYTES)
            )
        };

        let Ok((write_res, out_res, err_res)) = tokio::time::timeout(self.timeout, io).await else {
            reap(&mut child).await;
            return Err(self.timed_out());
        };
        let (stderr_bytes, _) = err_res.unwrap_or_default();
        let stderr_head = redact(&stderr_head(&stderr_bytes));

        let (stdout_bytes, overflow) = match out_res {
            Ok(r) => r,
            Err(e) => {
                reap(&mut child).await;
                return Err(self.fail(ErrorCode::GitProcessFailed).with_source(e));
            }
        };
        if overflow {
            reap(&mut child).await;
            return Err(self
                .fail(ErrorCode::GitOutputTooLarge)
                .with_context("max_output_bytes", max_out));
        }

        let status = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                return Err(self
                    .fail(ErrorCode::GitProcessFailed)
                    .with_context("stderr", stderr_head)
                    .with_source(e))
            }
            Err(_) => {
                reap(&mut child).await;
                return Err(self.timed_out());
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
        Ok(GitOutput {
            stdout: stdout_bytes,
        })
    }
}

/// git may exit before reading all of stdin; its exit status (and stderr)
/// then explains the failure better than the pipe error.
fn ignore_broken_pipe(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// Read up to `cap` bytes; returns `(bytes, overflowed)`.
async fn read_capped<R: AsyncRead + Unpin>(
    reader: Option<R>,
    cap: usize,
) -> io::Result<(Vec<u8>, bool)> {
    let Some(reader) = reader else {
        return Ok((Vec::new(), false));
    };
    let mut buf = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    reader.take(limit).read_to_end(&mut buf).await?;
    let overflow = buf.len() > cap;
    buf.truncate(cap);
    Ok((buf, overflow))
}

/// Kill and reap a child we are abandoning (bounded wait).
async fn reap(child: &mut tokio::process::Child) {
    // start_kill fails only if the child already exited; the wait observes it.
    let _ = child.start_kill(); // ignore: outcome observed by the wait below
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await; // ignore: best-effort reap after kill
}

/// The first lines of stderr for the log (bounded).
fn stderr_head(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut head: String = text.lines().take(20).collect::<Vec<_>>().join(" | ");
    if head.len() > 2000 {
        let mut cut = 2000;
        while !head.is_char_boundary(cut) {
            cut -= 1;
        }
        head.truncate(cut);
        head.push('…');
    }
    head
}

/// Remove `user:password@` from URLs (git prints remote URLs in errors).
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("://") {
        let (before, after) = rest.split_at(idx + 3);
        out.push_str(before);
        let end = after
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(after.len());
        let (authority, tail) = after.split_at(end);
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***@");
                out.push_str(authority.get(at + 1..).unwrap_or_default());
            }
            None => out.push_str(authority),
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
