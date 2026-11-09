//! Test helpers (`cfg(test)` and the `test-support` feature only): a git that
//! never answers, so a timeout is certain.
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
    use std::os::unix::fs::PermissionsExt as _;
    // The real git: the first `git` on PATH (the fake is never on PATH).
    let body = match stall_on {
        None => "#!/bin/sh\nexec sleep 60\n".to_owned(),
        Some(word) => format!(
            "#!/bin/sh\ncase \" $* \" in\n*\" {word} \"*) exec sleep 60;;\nesac\nexec git \"$@\"\n"
        ),
    };
    let path = dir.join("git");
    std::fs::write(&path, body).expect("fake git");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
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
