#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use forge_errors::ErrorCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{Git, GitConfig};

fn git_with(config: impl FnOnce(&mut GitConfig)) -> (tempfile::TempDir, Git) {
    let home = tempfile::tempdir().expect("home");
    let mut cfg = GitConfig::new("git", home.path());
    config(&mut cfg);
    (home, Git::new(cfg))
}

async fn bare_repo(git: &Git) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("repo");
    git.command("init")
        .arg("--bare")
        .arg("--quiet")
        .path(dir.path())
        .output()
        .await
        .expect("init");
    dir
}

/// A fake `git` (shell script) for behaviour real git does not show on
/// demand: forking helpers, stderr floods. Synthetic.
#[cfg(unix)]
fn fake_git(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-git.sh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// Whether `pid` is gone (no such process, or a zombie waiting for init).
#[cfg(unix)]
fn process_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

#[cfg(unix)]
async fn wait_gone(pid: u32) -> bool {
    for _ in 0..100 {
        if process_gone(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[cfg(unix)]
async fn read_pids(file: &Path) -> Vec<u32> {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(file) {
            let pids: Vec<u32> = text
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect();
            if pids.len() >= 2 {
                return pids;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the fake git did not write its pids");
}

#[tokio::test]
async fn stalled_stream_is_killed_after_idle_timeout() {
    let (_h, g) = git_with(|_| {});
    let repo = bare_repo(&g).await;
    // `cat-file --batch` waits for stdin forever: a client that stalls.
    let mut stream = g
        .command("cat-file")
        .repo(repo.path())
        .arg("--batch")
        .idle_timeout(Duration::from_millis(300))
        .timeout(Duration::from_secs(60))
        .spawn_stream("user:1")
        .await
        .expect("spawn");
    let _held_stdin = stream.take_stdin().expect("stdin");
    let mut stdout = stream.take_stdout().expect("stdout");
    let started = Instant::now();
    let mut out = Vec::new();
    stdout.read_to_end(&mut out).await.expect("stdout closes");
    let err = stream.finish().await.expect_err("killed");
    assert_eq!(err.code(), ErrorCode::GitTimeout);
    let text = err.to_string();
    assert!(text.contains("kind=idle"), "{text}");
    assert!(text.contains("idle_timeout_ms=300"), "{text}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "killed at the idle timeout, not the total: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn traffic_keeps_a_stream_alive_past_the_idle_timeout() {
    let (_h, g) = git_with(|_| {});
    let repo = bare_repo(&g).await;
    let mut stream = g
        .command("cat-file")
        .repo(repo.path())
        .arg("--batch-check")
        .idle_timeout(Duration::from_millis(400))
        .spawn_stream("user:1")
        .await
        .expect("spawn");
    let mut stdin = stream.take_stdin().expect("stdin");
    let mut stdout = stream.take_stdout().expect("stdout");
    let reader = tokio::spawn(async move {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).await.map(|_| out)
    });
    // 1.2 s of traffic, a line every 100 ms: three idle periods long.
    for _ in 0..12 {
        stdin
            .write_all(b"0000000000000000000000000000000000000001\n")
            .await
            .expect("write");
        stdin.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stdin.shutdown().await.expect("close stdin");
    drop(stdin);
    let out = reader.await.expect("join").expect("read");
    let stats = stream.finish().await.expect("not killed");
    assert_eq!(stats.bytes_in, 12 * 41);
    assert_eq!(stats.bytes_out, u64::try_from(out.len()).expect("len"));
    assert!(String::from_utf8_lossy(&out).contains("missing"));
}

#[tokio::test]
async fn busy_stream_is_killed_at_the_total_timeout() {
    let (_h, g) = git_with(|_| {});
    let repo = bare_repo(&g).await;
    let mut stream = g
        .command("cat-file")
        .repo(repo.path())
        .arg("--batch-check")
        .idle_timeout(Duration::from_secs(30))
        .timeout(Duration::from_millis(500))
        .spawn_stream("user:1")
        .await
        .expect("spawn");
    let mut stdin = stream.take_stdin().expect("stdin");
    let mut stdout = stream.take_stdout().expect("stdout");
    let reader = tokio::spawn(async move {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out).await; // ends when git is killed
    });
    let mut ended = stream.ended();
    let writer = async {
        loop {
            if stdin
                .write_all(b"0000000000000000000000000000000000000001\n")
                .await
                .is_err()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::select! {
        () = writer => {}
        () = ended.wait() => {}
    }
    reader.await.expect("join");
    let err = stream.finish().await.expect_err("killed");
    assert_eq!(err.code(), ErrorCode::GitTimeout);
    assert!(err.to_string().contains("kind=total"), "{err}");
    assert!(err.to_string().contains("timeout_ms=500"), "{err}");
}

/// The fake git forks a helper (like upload-pack starting pack-objects) and
/// stalls; the idle kill must take the helper with it.
#[cfg(unix)]
#[tokio::test]
async fn grandchildren_die_with_the_process_group() {
    let tmp = tempfile::tempdir().expect("tmp");
    let pids = tmp.path().join("pids");
    let script = fake_git(
        tmp.path(),
        &format!("sleep 300 &\necho $$ $! > '{}'\nwait", pids.display()),
    );
    let (_h, g) = git_with(|c| c.bin = script);
    let stream = g
        .command("upload-pack")
        .idle_timeout(Duration::from_millis(300))
        .spawn_stream("user:1")
        .await
        .expect("spawn");
    let found = read_pids(&pids).await;
    let (leader, helper) = (found[0], found[1]);
    assert!(!process_gone(helper), "the helper runs");
    let err = stream.finish().await.expect_err("killed");
    assert!(err.to_string().contains("kind=idle"), "{err}");
    assert!(wait_gone(leader).await, "the leader is gone");
    assert!(wait_gone(helper).await, "the helper died with the group");
}

#[cfg(unix)]
#[tokio::test]
async fn client_disconnect_kills_git_and_is_git_client_disconnected() {
    let tmp = tempfile::tempdir().expect("tmp");
    let pids = tmp.path().join("pids");
    let script = fake_git(
        tmp.path(),
        &format!("sleep 300 &\necho $$ $! > '{}'\nwait", pids.display()),
    );
    let (_h, g) = git_with(|c| c.bin = script);
    let stream = g
        .command("upload-pack")
        .spawn_stream("user:1")
        .await
        .expect("spawn");
    let found = read_pids(&pids).await;
    let started = Instant::now();
    let err = stream.abort_client_disconnected().await;
    assert_eq!(err.code(), ErrorCode::GitClientDisconnected);
    let text = err.to_string();
    assert!(text.contains("reason=client_disconnected"), "{text}");
    assert!(text.contains("subcommand=upload-pack"), "{text}");
    assert!(started.elapsed() < Duration::from_secs(5));
    for pid in found {
        assert!(wait_gone(pid).await, "{pid} killed");
    }
}

/// axum drops the response body when the client goes away: dropping the
/// stream must kill the tree, too.
#[cfg(unix)]
#[tokio::test]
async fn dropping_the_stream_kills_the_process_tree() {
    let tmp = tempfile::tempdir().expect("tmp");
    let pids = tmp.path().join("pids");
    let script = fake_git(
        tmp.path(),
        &format!("sleep 300 &\necho $$ $! > '{}'\nwait", pids.display()),
    );
    let (_h, g) = git_with(|c| c.bin = script);
    let stream = g
        .command("upload-pack")
        .spawn_stream("user:1")
        .await
        .expect("spawn");
    let found = read_pids(&pids).await;
    drop(stream);
    for pid in found {
        assert!(wait_gone(pid).await, "{pid} killed");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stderr_is_capped_and_drained() {
    let tmp = tempfile::tempdir().expect("tmp");
    // 1 MiB of stderr, then a failure: git must not block on the pipe.
    let script = fake_git(
        tmp.path(),
        "head -c 1048576 /dev/zero | tr '\\000' e >&2\necho fatal: synthetic >&2\nexit 3",
    );
    let (_h, g) = git_with(|c| c.bin = script);
    let mut stream = g
        .command("receive-pack")
        .idle_timeout(Duration::from_secs(20))
        .spawn_stream("user:1")
        .await
        .expect("spawn");
    let mut stdout = stream.take_stdout().expect("stdout");
    let mut out = Vec::new();
    stdout.read_to_end(&mut out).await.expect("read");
    let started = Instant::now();
    let err = stream.finish().await.expect_err("exit 3");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(err.code(), ErrorCode::GitProcessFailed);
    let text = err.to_string();
    assert!(
        text.contains("exit_code=3"),
        "{}",
        &text[..200.min(text.len())]
    );
    assert!(text.contains("stderr_truncated=true"), "the tail was cut");
    assert!(
        text.len() < 8 * 1024,
        "the log line stays bounded: {}",
        text.len()
    );
}

#[tokio::test]
async fn the_stderr_tail_keeps_at_most_64_kib() {
    let mut tail = super::StderrTail::default();
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg("head -c 200000 /dev/zero >&2")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("sh");
    let mut pipe = child.stderr.take();
    while pipe.is_some() {
        tail.read(&mut pipe).await;
    }
    let _ = child.wait().await;
    assert_eq!(tail.buf.len(), crate::command::MAX_STDERR_BYTES);
    assert!(tail.truncated);
}

#[tokio::test]
async fn stream_slot_exhaustion_is_git_busy_streams() {
    let (_h, g) = git_with(|c| {
        c.max_streams = 1;
        c.acquire_timeout = Duration::from_millis(100);
    });
    let repo = bare_repo(&g).await;
    let first = g
        .command("cat-file")
        .repo(repo.path())
        .arg("--batch")
        .spawn_stream("user:1")
        .await
        .expect("first");
    let err = g
        .command("cat-file")
        .repo(repo.path())
        .arg("--batch")
        .spawn_stream("user:2")
        .await
        .expect_err("no slot");
    assert_eq!(err.code(), ErrorCode::GitBusy);
    assert!(err.to_string().contains("reason=streams"), "{err}");
    // The captured pool is separate: short git calls still run.
    g.command("rev-parse")
        .repo(repo.path())
        .arg("--is-bare-repository")
        .output()
        .await
        .expect("captured call while the stream pool is full");
    let _ = first.abort_client_disconnected().await;
    g.command("cat-file")
        .repo(repo.path())
        .arg("--batch")
        .spawn_stream("user:2")
        .await
        .expect("the slot is free again once git is reaped");
}

#[tokio::test]
async fn per_principal_cap_refuses_at_once_and_releases() {
    let (_h, g) = git_with(|c| {
        c.max_streams_per_principal = 2;
        c.acquire_timeout = Duration::from_secs(30);
    });
    let repo = bare_repo(&g).await;
    let spawn = |principal: &'static str| {
        g.command("cat-file")
            .repo(repo.path())
            .arg("--batch")
            .spawn_stream(principal)
    };
    let a1 = spawn("user:1").await.expect("1st");
    let _a2 = spawn("user:1").await.expect("2nd");
    let started = Instant::now();
    let err = spawn("user:1").await.expect_err("3rd is over the cap");
    assert!(started.elapsed() < Duration::from_secs(5), "no waiting");
    assert_eq!(err.code(), ErrorCode::GitBusy);
    assert!(
        err.to_string().contains("reason=principal_streams"),
        "{err}"
    );
    let _b = spawn("user:2")
        .await
        .expect("another principal is not affected");
    let _ = a1.abort_client_disconnected().await;
    let _a3 = spawn("user:1")
        .await
        .expect("released after the stream ended");
}

#[tokio::test]
async fn spawn_failure_is_git_unavailable() {
    let (_h, g) = git_with(|c| c.bin = PathBuf::from("/nonexistent/forge-test-git"));
    let err = g
        .command("upload-pack")
        .spawn_stream("user:1")
        .await
        .expect_err("no binary");
    assert_eq!(err.code(), ErrorCode::GitUnavailable);
    assert!(err.to_string().contains("reason=spawn_failed"));
    // The claim and the slot were released.
    let err = g
        .command("upload-pack")
        .spawn_stream("user:1")
        .await
        .expect_err("still no binary");
    assert!(err.to_string().contains("reason=spawn_failed"), "{err}");
}

#[tokio::test]
async fn stdin_bytes_are_refused_on_a_stream() {
    let (_h, g) = git_with(|_| {});
    let err = g
        .command("cat-file")
        .stdin(b"x".to_vec())
        .spawn_stream("user:1")
        .await
        .expect_err("stdin bytes are for output()");
    assert_eq!(err.code(), ErrorCode::Internal);
}
