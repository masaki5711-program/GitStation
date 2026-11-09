//! Test helpers (`cfg(test)` and the `test-support` feature only): a git that
//! never answers, so a timeout is certain, and [`write_script`] for every
//! fake git / hook a test runs.
//!
//! A near-zero timeout (`Duration::ZERO`, 1 ns) is **not** certain: the
//! deadline is set before git is spawned and `timeout_at` polls the run
//! first, so under load a fast git that already wrote its answer and closed
//! stdout wins and the call succeeds (2026-11-09: forge-git
//! `merge_base_questions_time_out_and_refuse_missing_objects` failed so in a
//! full run). Tests that need a timeout use these instead.

// Test-only code (built for tests and the `test-support` feature, never
// for a release): a setup failure panics the test, as in the tests.
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{Git, GitConfig};

/// A timeout short enough to keep the tests quick and long enough that
/// spawning the fake never decides the outcome.
pub const STALL: Duration = Duration::from_millis(300);

/// A fake `git` in `dir`: for every invocation (`stall_on: None`), or only
/// when its arguments contain `stall_on`, it sleeps far past [`STALL`];
/// otherwise it runs the real `git` from `PATH` with the same arguments.
pub fn stalled_git(home: &Path, dir: &Path, stall_on: Option<&str>) -> Git {
    let bin = write_fake(dir, stall_on);
    Git::new(GitConfig::new(bin, home))
}

#[cfg(unix)]
fn write_fake(dir: &Path, stall_on: Option<&str>) -> PathBuf {
    // The real git: the first `git` on PATH (the fake is never on PATH).
    let body = match stall_on {
        None => "#!/bin/sh\nexec sleep 60\n".to_owned(),
        Some(word) => format!(
            "#!/bin/sh\ncase \" $* \" in\n*\" {word} \"*) exec sleep 60;;\nesac\nexec git \"$@\"\n"
        ),
    };
    let path = dir.join("git");
    write_script(&path, &body);
    path
}

/// Write an executable script (mode 0755) at `path`, replacing any file
/// there, **from a child process**: this process never holds the script
/// open for writing. A test binary runs many tests in parallel threads, and
/// a child another thread forks inherits every open descriptor until its
/// own `exec` (`O_CLOEXEC` closes it only then); running a script while
/// such a child still holds it open for writing fails with `ETXTBSY` ("Text
/// file busy") — forge sees `GitUnavailable reason=spawn_failed`. Measured
/// 2026-11-09: 1 421 of 7 956 runs while six threads spawned processes.
#[cfg(unix)]
pub fn write_script(path: &Path, body: &str) {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cat > \"$1\" && chmod 755 \"$1\"")
        .arg("sh")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("sh to write the script");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(body.as_bytes()).expect("script body");
    } // closed: `cat` ends
    let status = child.wait().expect("sh");
    assert!(
        status.success(),
        "writing {} failed: {status}",
        path.display()
    );
}

/// Windows: a plain write (no `ETXTBSY` there; git runs hooks through its
/// own `sh`, and the executable bit does not exist).
#[cfg(windows)]
pub fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("script");
}

#[cfg(windows)]
fn write_fake(dir: &Path, stall_on: Option<&str>) -> PathBuf {
    let body = match stall_on {
        None => "@ping -n 60 127.0.0.1 >nul\r\n".to_owned(),
        Some(word) => format!(
            "@echo off\r\necho %* | findstr /c:\"{word}\" >nul\r\n\
             if not errorlevel 1 (ping -n 60 127.0.0.1 >nul & exit /b 1)\r\n\
             git.exe %*\r\n"
        ),
    };
    let path = dir.join("git.cmd");
    std::fs::write(&path, body).expect("fake git");
    path
}
