//! The one way the runner starts a program (git, podman): an argument
//! vector, never a shell string; a scrubbed environment; a deadline; a stop
//! flag. [`ProcessRunner`] is a trait so the tests assert the exact argv
//! and environment without git or Podman ([`SystemRunner`] is the real
//! one).
//!
//! The runner is outside the server's crates (contract 9), so this is its
//! own wrapper rather than `forge-git`'s; it keeps the same rules
//! (`rust/security.md`): argv only, `--` / `--end-of-options` before
//! values that could look like options (the callers), no inherited
//! `GIT_*` variable, a timeout on every call.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How much of a program's stderr is kept for the log (`git_stderr`,
/// `podman_stderr`).
pub const STDERR_TAIL: usize = 2048;
/// The most stdout a captured call keeps (one byte past the definition's
/// limit is enough to tell "too large").
pub const MAX_CAPTURE: usize = 256 * 1024;
/// How often a running program's deadline and stop flag are looked at.
const POLL: Duration = Duration::from_millis(100);

/// A program to run. The environment holds only what is set here (plus
/// `PATH`); [`Cmd`]'s `Debug` never shows an environment value (the job's
/// token travels in one).
#[derive(Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(String, OsString)>,
    pub cwd: Option<PathBuf>,
    /// Keep stdout in [`Ran::stdout`] (at most [`MAX_CAPTURE`]) instead of
    /// passing it to the output sink.
    pub capture: bool,
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("Cmd")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env_keys", &keys)
            .field("cwd", &self.cwd)
            .field("capture", &self.capture)
            .finish()
    }
}

impl Cmd {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            capture: false,
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn env(mut self, key: &str, value: impl Into<OsString>) -> Self {
        self.env.push((key.to_owned(), value.into()));
        self
    }

    #[must_use]
    pub fn captured(mut self) -> Self {
        self.capture = true;
        self
    }

    /// The arguments as text (tests, logs).
    pub fn argv(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The program's file name without extension (`git`, `podman`).
    pub fn program_name(&self) -> String {
        std::path::Path::new(&self.program)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The value of `key` in the environment set here.
    pub fn env_value(&self, key: &str) -> Option<&OsStr> {
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_os_str())
    }
}

/// How a program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Its exit status (`-1` when a signal ended it).
    Code(i32),
    /// The deadline passed; it was killed.
    TimedOut,
    /// The stop flag was raised (a cancel, a lost lease); it was killed.
    Stopped,
}

/// What a finished program left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    pub exit: Exit,
    /// Captured stdout ([`Cmd::capture`]), else empty.
    pub stdout: Vec<u8>,
    /// The last [`STDERR_TAIL`] bytes of stderr (lossy UTF-8).
    pub stderr_tail: String,
}

impl Ran {
    pub fn ok(&self) -> bool {
        self.exit == Exit::Code(0)
    }
}

/// Starts programs. `stop` is polled while the program runs; `out` gets
/// its output (stdout unless captured, and stderr) as it comes.
pub trait ProcessRunner: Send + Sync {
    fn run(
        &self,
        cmd: &Cmd,
        limit: Duration,
        stop: &dyn Fn() -> bool,
        out: &mut dyn FnMut(&[u8]),
    ) -> std::io::Result<Ran>;
}

/// The real [`ProcessRunner`]: `std::process::Command` with the environment
/// cleared (only `PATH` and the command's own variables), stdin closed,
/// both pipes read on threads, killed at the deadline or on stop.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

enum Chunk {
    Out(Vec<u8>),
    Err(Vec<u8>),
}

fn pump(mut from: impl Read + Send + 'static, tx: mpsc::Sender<Chunk>, err: bool) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    let sent = tx.send(if err {
                        Chunk::Err(chunk)
                    } else {
                        Chunk::Out(chunk)
                    });
                    if sent.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

impl ProcessRunner for SystemRunner {
    fn run(
        &self,
        cmd: &Cmd,
        limit: Duration,
        stop: &dyn Fn() -> bool,
        out: &mut dyn FnMut(&[u8]),
    ) -> std::io::Result<Ran> {
        let mut command = Command::new(&cmd.program);
        command
            .args(&cmd.args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in ["PATH", "SystemRoot", "TEMP", "TMP"] {
            if let Some(v) = std::env::var_os(key) {
                command.env(key, v);
            }
        }
        for (k, v) in &cmd.env {
            command.env(k, v);
        }
        if let Some(dir) = &cmd.cwd {
            command.current_dir(dir);
        }
        let mut child = command.spawn()?;
        let (tx, rx) = mpsc::channel();
        if let Some(o) = child.stdout.take() {
            pump(o, tx.clone(), false); // clone: one sender per pipe
        }
        if let Some(e) = child.stderr.take() {
            pump(e, tx.clone(), true); // clone: as above
        }
        drop(tx);
        let deadline = Instant::now() + limit;
        let mut stdout = Vec::new();
        let mut tail: Vec<u8> = Vec::new();
        let mut ended: Option<Exit> = None;
        let mut take = |chunk: Chunk, stdout: &mut Vec<u8>, tail: &mut Vec<u8>| match chunk {
            Chunk::Out(b) if cmd.capture => {
                let room = (MAX_CAPTURE + 1).saturating_sub(stdout.len());
                stdout.extend_from_slice(&b[..b.len().min(room)]);
            }
            Chunk::Out(b) => out(&b),
            Chunk::Err(b) => {
                tail.extend_from_slice(&b);
                if tail.len() > STDERR_TAIL {
                    tail.drain(..tail.len() - STDERR_TAIL);
                }
                if !cmd.capture {
                    out(&b);
                }
            }
        };
        let status = loop {
            match rx.recv_timeout(POLL) {
                Ok(chunk) => take(chunk, &mut stdout, &mut tail),
                Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
            }
            if ended.is_none() {
                if Instant::now() >= deadline {
                    ended = Some(Exit::TimedOut);
                } else if stop() {
                    ended = Some(Exit::Stopped);
                }
                if ended.is_some() {
                    // A program that already exited makes kill fail: the
                    // wait below still reaps it.
                    let _ = child.kill();
                }
            }
            if let Some(status) = child.try_wait()? {
                // Drain what the pipes still hold (bounded: the pumps end
                // with the program's pipes).
                while let Ok(chunk) = rx.recv_timeout(POLL) {
                    take(chunk, &mut stdout, &mut tail);
                }
                break status;
            }
        };
        let exit = ended.unwrap_or(Exit::Code(status.code().unwrap_or(-1)));
        Ok(Ran {
            exit,
            stdout,
            stderr_tail: String::from_utf8_lossy(&tail).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use super::*;

    #[test]
    fn debug_never_shows_an_environment_value() {
        let cmd = Cmd::new("git")
            .arg("fetch")
            .env("GIT_CONFIG_VALUE_0", "Bearer fgj_secret");
        let shown = format!("{cmd:?}");
        assert!(!shown.contains("fgj_secret"), "{shown}");
        assert!(shown.contains("GIT_CONFIG_VALUE_0"));
    }

    fn sleeper() -> Cmd {
        if cfg!(windows) {
            Cmd::new("powershell")
                .arg("-NoProfile")
                .arg("-Command")
                .arg("Start-Sleep -Seconds 30")
        } else {
            Cmd::new("sleep").arg("30")
        }
    }

    /// The deadline kills the program: the call returns long before it
    /// would have ended.
    #[test]
    fn a_program_past_its_deadline_is_killed() {
        let started = Instant::now();
        let ran = SystemRunner
            .run(
                &sleeper(),
                Duration::from_millis(500),
                &|| false,
                &mut |_| {},
            )
            .expect("ran");
        assert_eq!(ran.exit, Exit::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn the_stop_flag_kills_the_program() {
        let started = Instant::now();
        let ran = SystemRunner
            .run(&sleeper(), Duration::from_secs(60), &|| true, &mut |_| {})
            .expect("ran");
        assert_eq!(ran.exit, Exit::Stopped);
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn stdout_is_captured_when_asked() {
        let cmd = Cmd::new("git").arg("--version").captured();
        let ran = SystemRunner
            .run(&cmd, Duration::from_secs(30), &|| false, &mut |_| {})
            .expect("git");
        assert!(ran.ok(), "{ran:?}");
        assert!(String::from_utf8_lossy(&ran.stdout).starts_with("git version"));
    }
}
