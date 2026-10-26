//! The single git subprocess wrapper.
//!
//! Every invocation: argument vector only (never a shell), environment cleared
//! and rebuilt (no inherited `GIT_*`), user-controlled operands after `--`, a
//! total timeout, a global concurrency limit, exit status checked and stderr
//! kept for the log only.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

/// How git is run. Loaded from configuration in `forge serve` / `worker`.
#[derive(Debug, Clone)]
pub struct GitConfig {
    /// Path to the git binary (or `git` to resolve via `PATH`).
    pub bin: PathBuf,
    /// The `HOME` git sees — owned by forge, so no user `.gitconfig` applies.
    pub home: PathBuf,
    /// Upper bound for a non-streaming invocation.
    pub timeout_total: Duration,
    /// How long to wait for a free subprocess slot.
    pub acquire_timeout: Duration,
    /// Maximum concurrent git subprocesses.
    pub max_concurrency: usize,
}

impl GitConfig {
    pub fn new(bin: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            home: home.into(),
            timeout_total: Duration::from_secs(300),
            acquire_timeout: Duration::from_secs(10),
            max_concurrency: 32,
        }
    }
}

/// Handle shared by everything that runs git. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Git {
    config: Arc<GitConfig>,
    slots: Arc<Semaphore>,
}

/// Captured result of a successful invocation.
#[derive(Debug)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
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
            args: Vec::new(),
            dashdash: false,
            cwd: None,
            envs: Vec::new(),
            stdin: None,
            hold_stdin: false,
            timeout: self.config.timeout_total,
        }
    }
}

/// One git invocation being built.
#[derive(Debug)]
pub struct GitCommand {
    git: Git,
    subcommand: &'static str,
    args: Vec<OsString>,
    dashdash: bool,
    cwd: Option<PathBuf>,
    envs: Vec<(&'static str, OsString)>,
    stdin: Option<Vec<u8>>,
    /// Keep stdin open without writing (tests: simulate a client that stalls).
    hold_stdin: bool,
    timeout: Duration,
}

/// Variables passed through from the server process. Everything else
/// (notably every `GIT_*`) is dropped.
const PASSTHROUGH_ENV: &[&str] = &["PATH", "SystemRoot", "SYSTEMROOT", "TEMP", "TMP", "TMPDIR"];

impl GitCommand {
    /// A forge-controlled option or argument (never user input).
    #[must_use]
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    /// A user-controlled operand (ref, path, revision). Always placed after a
    /// single `--`, so it can never be parsed as an option.
    #[must_use]
    pub fn operand(mut self, value: impl AsRef<OsStr>) -> Self {
        if !self.dashdash {
            self.args.push(OsString::from("--"));
            self.dashdash = true;
        }
        self.args.push(value.as_ref().to_os_string());
        self
    }

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

    /// Keep the child's stdin open and never write to it (a stalled client).
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

    /// The argument vector after the subcommand (for tests and logs).
    pub fn args(&self) -> &[OsString] {
        &self.args
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
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("LANG", "C");
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
        cmd.arg(self.subcommand).args(&self.args);
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        cmd.stdin(if self.stdin.is_some() || self.hold_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        cmd
    }

    fn fail(&self, code: ErrorCode) -> ForgeError {
        ForgeError::new(code).with_context("subcommand", self.subcommand)
    }

    /// Run to completion and capture output. Non-zero exit is `FG-GIT-0003`,
    /// exceeding the timeout is `FG-GIT-0002` (the process is killed).
    pub async fn output(mut self) -> Result<GitOutput, ForgeError> {
        let git = self.git.clone();
        let config = git.config();
        let permit = tokio::time::timeout(config.acquire_timeout, git.slots.acquire())
            .await
            .map_err(|_| self.fail(ErrorCode::GitBusy))?
            .map_err(|e| self.fail(ErrorCode::GitBusy).with_source(e))?;

        let mut child = self.build().spawn().map_err(|e| {
            self.fail(ErrorCode::GitUnavailable)
                .with_context("reason", "spawn_failed")
                .with_context("git_bin", config.bin.display())
                .with_source(e)
        })?;

        // Held for the whole run when `hold_stdin` is set, so the child never
        // sees EOF (wait_with_output would otherwise close it).
        let held_stdin = if self.hold_stdin {
            child.stdin.take()
        } else {
            None
        };
        let input = self.stdin.take();
        let run = async move {
            if let Some(input) = input {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(&input).await?;
                    stdin.shutdown().await?;
                }
            }
            child.wait_with_output().await
        };
        let result = tokio::time::timeout(self.timeout, run).await;
        drop(held_stdin);
        drop(permit);

        let output = match result {
            Err(_) => {
                // `kill_on_drop` killed the child when the future was dropped.
                return Err(self
                    .fail(ErrorCode::GitTimeout)
                    .with_context("kind", "total")
                    .with_context("timeout_ms", self.timeout.as_millis()));
            }
            Ok(Err(e)) => return Err(self.fail(ErrorCode::GitProcessFailed).with_source(e)),
            Ok(Ok(output)) => output,
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let head: String = stderr.lines().take(5).collect::<Vec<_>>().join(" | ");
            return Err(self
                .fail(ErrorCode::GitProcessFailed)
                .with_context(
                    "exit_code",
                    output
                        .status
                        .code()
                        .map_or_else(|| "signal".to_string(), |c| c.to_string()),
                )
                .with_context("stderr", head));
        }
        Ok(GitOutput {
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git() -> Git {
        let home = std::env::temp_dir();
        Git::new(GitConfig::new("git", home))
    }

    #[test]
    fn user_ref_starting_with_dash_is_after_double_dash() {
        let cmd = git()
            .command("log")
            .arg("--format=%H")
            .operand("--upload-pack=evil")
            .operand("refs/heads/main");
        let args: Vec<_> = cmd.args().iter().map(|a| a.to_string_lossy()).collect();
        assert_eq!(
            args,
            ["--format=%H", "--", "--upload-pack=evil", "refs/heads/main"]
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_git_process_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = git()
            .command("rev-parse")
            .arg("--verify")
            .current_dir(dir.path())
            .operand("HEAD")
            .output()
            .await
            .expect_err("not a repository");
        assert_eq!(err.code(), ErrorCode::GitProcessFailed);
        assert!(err.context().iter().any(|(k, _)| *k == "exit_code"));
    }

    #[tokio::test]
    async fn hung_git_is_killed_after_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let g = git();
        g.command("init")
            .arg("--bare")
            .operand(dir.path())
            .output()
            .await
            .expect("init");
        // `cat-file --batch` reads stdin until EOF; a stalled client never
        // sends EOF, so only the timeout can end it.
        let err = g
            .command("cat-file")
            .arg("--batch")
            .current_dir(dir.path())
            .hold_stdin_open()
            .timeout(Duration::from_millis(300))
            .output()
            .await
            .expect_err("must time out");
        assert_eq!(err.code(), ErrorCode::GitTimeout);
    }

    #[tokio::test]
    async fn concurrency_limit_reports_git_busy() {
        let mut config = GitConfig::new("git", std::env::temp_dir());
        config.max_concurrency = 1;
        config.acquire_timeout = Duration::from_millis(100);
        let g = Git::new(config);
        let _held = g.slots.clone().acquire_owned().await.expect("permit");
        let err = g
            .command("version")
            .output()
            .await
            .expect_err("no free slot");
        assert_eq!(err.code(), ErrorCode::GitBusy);
    }

    #[tokio::test]
    async fn inherited_git_dir_is_not_passed_to_child() {
        let repo = tempfile::tempdir().expect("tempdir");
        let g = git();
        g.command("init")
            .operand(repo.path())
            .output()
            .await
            .expect("init");
        // The server process has GIT_DIR pointing at a real repository; a
        // scrubbed child in an empty directory must not see it.
        std::env::set_var("GIT_DIR", repo.path().join(".git"));
        let empty = tempfile::tempdir().expect("tempdir");
        let res = g
            .command("rev-parse")
            .arg("--git-dir")
            .current_dir(empty.path())
            .output()
            .await;
        std::env::remove_var("GIT_DIR");
        let err = res.expect_err("GIT_DIR must not leak into the child");
        assert_eq!(err.code(), ErrorCode::GitProcessFailed);
    }

    #[tokio::test]
    async fn missing_git_binary_is_git_unavailable() {
        let g = Git::new(GitConfig::new(
            "/nonexistent/forge-test/git",
            std::env::temp_dir(),
        ));
        let err = g.command("version").output().await.expect_err("no binary");
        assert_eq!(err.code(), ErrorCode::GitUnavailable);
    }
}
