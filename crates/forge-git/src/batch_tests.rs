#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! The `cat-file --batch-command` protocol reader (in memory) and the pool
//! against a real git (synthetic content).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::*;
use crate::{Git, GitConfig};

const A: &str = "0123456789abcdef0123456789abcdef01234567";
const B: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn reader(bytes: &[u8]) -> BufReader<&[u8]> {
    BufReader::new(bytes)
}

// ── the reader ─────────────────────────────────────────────────────────

#[tokio::test]
async fn headers_name_found_and_missing_objects() {
    let input = format!("{A} blob 12\n{B} missing\n{A} tree 0\n");
    let mut r = reader(input.as_bytes());
    assert_eq!(
        read_info(&mut r, A).await.unwrap(),
        Info::Found {
            kind: "blob".into(),
            size: 12
        }
    );
    assert_eq!(read_info(&mut r, B).await.unwrap(), Info::Missing);
    assert_eq!(
        read_info(&mut r, A).await.unwrap(),
        Info::Found {
            kind: "tree".into(),
            size: 0
        }
    );
    // The output ended: the process died between answers.
    let e = read_info(&mut r, A).await.unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn a_header_out_of_protocol_is_refused() {
    for bad in [
        format!("{B} blob 12\n"),                  // another object
        format!("{A} blob\n"),                     // no size
        format!("{A} blob 12 extra\n"),            // a field more
        format!("{A} weird 12\n"),                 // an unknown type
        format!("{A} blob -1\n"),                  // not a size
        format!("{A} ambiguous\n"),                // never for a full id
        format!("{A}\n"),                          // one field
        format!("{A} blob {}\n", "9".repeat(300)), // too long
    ] {
        let mut r = reader(bad.as_bytes());
        let e = read_info(&mut r, A).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{bad:?}");
    }
    // A header cut before its newline: the process died mid-header.
    let mut r = reader(A.as_bytes());
    let e = read_info(&mut r, A).await.unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn contents_keep_binary_bytes_and_check_their_end() {
    let data: Vec<u8> = (0..=255u8).chain([b'\n', 0, b'\n']).collect();
    let mut input = format!("{A} blob {}\n", data.len()).into_bytes();
    input.extend_from_slice(&data);
    input.push(b'\n');
    input.extend_from_slice(format!("{B} missing\n").as_bytes());
    let mut r = reader(&input);
    assert_eq!(
        read_contents(&mut r, A, 1024).await.unwrap(),
        Contents::Found {
            kind: "blob".into(),
            data
        }
    );
    assert_eq!(
        read_contents(&mut r, B, 1024).await.unwrap(),
        Contents::Missing
    );
}

#[tokio::test]
async fn large_contents_are_read_whole() {
    let data: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let mut input = format!("{A} blob {}\n", data.len()).into_bytes();
    input.extend_from_slice(&data);
    input.push(b'\n');
    let mut r = reader(&input);
    let Contents::Found { data: read, .. } = read_contents(&mut r, A, u64::MAX).await.unwrap()
    else {
        panic!("found");
    };
    assert_eq!(read, data);
}

#[tokio::test]
async fn contents_cut_short_or_not_terminated_are_errors() {
    // The process died in the middle of the content.
    let mut input = format!("{A} blob 10\n").into_bytes();
    input.extend_from_slice(b"12345");
    let e = read_contents(&mut reader(&input), A, 100)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    // The content is followed by something other than a newline.
    let mut input = format!("{A} blob 3\n").into_bytes();
    input.extend_from_slice(b"abcX");
    let e = read_contents(&mut reader(&input), A, 100)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    // Larger than the caller asked for.
    let input = format!("{A} blob 300\n");
    let e = read_contents(&mut reader(input.as_bytes()), A, 100)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn only_full_object_ids_reach_the_process() {
    let mut input = Vec::new();
    for bad in [
        "HEAD",
        "main",
        "-n",
        &A[..39],
        "g".repeat(40).as_str(),
        "a\nflush",
    ] {
        let e = command_line(&mut input, "info", bad).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
    }
    assert!(input.is_empty());
    command_line(&mut input, "contents", A).unwrap();
    assert_eq!(input, format!("contents {A}\n").into_bytes());
}

// ── the pool, real git ─────────────────────────────────────────────────

struct Repo {
    _home: tempfile::TempDir,
    _dir: tempfile::TempDir,
    home: PathBuf,
    path: PathBuf,
}

fn repo() -> Repo {
    let home = tempfile::tempdir().expect("home");
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("r.git");
    let ok = Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(&path)
        .status()
        .expect("git init");
    assert!(ok.success());
    Repo {
        home: home.path().to_path_buf(),
        _home: home,
        _dir: dir,
        path,
    }
}

fn write_blob(repo: &Path, content: &[u8]) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("git");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(content)
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn config(per_repo: usize, max_repos: usize, idle: Duration) -> BatchConfig {
    BatchConfig {
        per_repo,
        max_repos,
        idle,
        wait: Duration::from_secs(5),
    }
}

fn pool(r: &Repo, cfg: BatchConfig) -> CatFilePool {
    CatFilePool::new(Git::new(GitConfig::new("git", &r.home)), cfg)
}

const T: Duration = Duration::from_secs(10);

#[tokio::test]
async fn the_pool_reads_blobs_binary_and_large_and_names_missing_ones() {
    let r = repo();
    let p = pool(&r, config(2, 4, Duration::from_secs(60)));
    let binary: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    let large: Vec<u8> = (0..3 * 1024 * 1024u32)
        .map(|i| (i * 7 % 253) as u8)
        .collect();
    let small = write_blob(&r.path, b"hello\n");
    let bin = write_blob(&r.path, &binary);
    let big = write_blob(&r.path, &large);
    let missing = "0".repeat(40);

    let read = |oid: String, max: u64| {
        let p = p.clone(); // clone: a shared handle
        let path = r.path.clone(); // clone: moved into the read
        async move { p.contents(&path, &oid, max, T).await }
    };
    let Contents::Found { kind, data } = read(small.clone(), 100).await.unwrap() else {
        panic!("small");
    };
    assert_eq!((kind.as_str(), data.as_slice()), ("blob", &b"hello\n"[..]));
    let Contents::Found { data, .. } = read(bin.clone(), u64::MAX).await.unwrap() else {
        panic!("binary");
    };
    assert_eq!(data, binary);
    let Contents::Found { data, .. } = read(big.clone(), u64::MAX).await.unwrap() else {
        panic!("large");
    };
    assert_eq!(data, large);
    // Over the cap: not read, the process stays usable.
    assert_eq!(
        read(big.clone(), 1024).await.unwrap(),
        Contents::TooLarge {
            kind: "blob".into(),
            size: large.len() as u64
        }
    );
    assert_eq!(read(missing.clone(), 100).await.unwrap(), Contents::Missing);
    // One process did all of it.
    assert_eq!(p.idle_count(&r.path), 1);

    let infos = p.info(&r.path, &[&small, &missing, &bin], T).await.unwrap();
    assert_eq!(
        infos,
        vec![
            Info::Found {
                kind: "blob".into(),
                size: 6
            },
            Info::Missing,
            Info::Found {
                kind: "blob".into(),
                size: 70_000
            },
        ]
    );
}

/// Thousands of `info` lines in one exchange: written and read together,
/// so the full pipe never deadlocks.
#[tokio::test]
async fn a_long_info_list_does_not_fill_the_pipes() {
    let r = repo();
    let p = pool(&r, config(1, 4, Duration::from_secs(60)));
    let known = write_blob(&r.path, b"x");
    let ids: Vec<String> = (0..20_000u32)
        .map(|i| {
            if i % 2 == 0 {
                known.clone() // clone: the same id many times
            } else {
                format!("{i:040x}")
            }
        })
        .collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let infos = p.info(&r.path, &refs, T).await.unwrap();
    assert_eq!(infos.len(), ids.len());
    assert!(infos
        .iter()
        .step_by(2)
        .all(|i| matches!(i, Info::Found { size: 1, .. })));
    assert!(infos.iter().skip(1).step_by(2).all(|i| *i == Info::Missing));
}

#[tokio::test]
async fn a_dead_process_is_a_failure_and_is_not_reused() {
    let r = repo();
    let p = pool(&r, config(1, 4, Duration::from_secs(60)));
    let oid = write_blob(&r.path, b"x");
    p.info(&r.path, &[&oid], T).await.unwrap();
    assert_eq!(p.idle_count(&r.path), 1);
    p.kill_idle(&r.path).await;
    let Err(BatchError::Failed(e)) = p.info(&r.path, &[&oid], T).await else {
        panic!("a dead process answers nothing");
    };
    assert_eq!(e.code(), ErrorCode::GitBatchFailed);
    assert!(
        e.context().iter().any(|(k, v)| *k == "reason" && v == "io"),
        "{e:?}"
    );
    // Not put back: the next exchange starts a new process.
    assert_eq!(p.idle_count(&r.path), 0);
    p.info(&r.path, &[&oid], T).await.unwrap();
    assert_eq!(p.idle_count(&r.path), 1);
}

#[tokio::test]
async fn a_process_that_never_answers_times_out_and_is_killed() {
    let r = repo();
    let fake = tempfile::tempdir().expect("fake");
    let git = crate::test_git::stalled_git(&r.home, fake.path(), Some("--batch-command"));
    let p = CatFilePool::new(git, config(1, 4, Duration::from_secs(60)));
    let started = std::time::Instant::now();
    let Err(BatchError::Failed(e)) = p.info(&r.path, &[A], crate::test_git::STALL).await else {
        panic!("a timeout");
    };
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(e.code(), ErrorCode::GitBatchFailed);
    assert!(e
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "timeout"));
    assert_eq!(p.idle_count(&r.path), 0);
}

#[tokio::test]
async fn processes_idle_past_the_limit_are_closed() {
    let r = repo();
    let p = pool(&r, config(2, 4, Duration::from_millis(200)));
    let oid = write_blob(&r.path, b"x");
    p.info(&r.path, &[&oid], T).await.unwrap();
    assert_eq!(p.idle_count(&r.path), 1);
    let mut closed = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if p.repo_count() == 0 {
            closed = true;
            break;
        }
    }
    assert!(closed, "the idle process and its repository were closed");
    // The pool starts a new one on the next read.
    p.info(&r.path, &[&oid], T).await.unwrap();
    assert_eq!(p.idle_count(&r.path), 1);
}

#[tokio::test]
async fn the_pool_keeps_at_most_max_repos() {
    let a = repo();
    let b = repo();
    let p = pool(&a, config(1, 1, Duration::from_secs(60)));
    let oa = write_blob(&a.path, b"a");
    let ob = write_blob(&b.path, b"b");
    p.info(&a.path, &[&oa], T).await.unwrap();
    p.info(&b.path, &[&ob], T).await.unwrap();
    assert_eq!(p.repo_count(), 1);
    assert_eq!(p.idle_count(&a.path), 0, "the older repository was closed");
    assert_eq!(p.idle_count(&b.path), 1);
}

#[tokio::test]
async fn many_concurrent_reads_share_at_most_per_repo_processes() {
    let r = repo();
    let p = pool(&r, config(2, 4, Duration::from_secs(60)));
    let oids: Vec<(String, Vec<u8>)> = (0..16u8)
        .map(|i| {
            let content = vec![i; 1000 + usize::from(i)];
            (write_blob(&r.path, &content), content)
        })
        .collect();
    let tasks: Vec<_> = (0..64usize)
        .map(|n| {
            let p = p.clone(); // clone: one handle per task
            let path = r.path.clone(); // clone: moved into the task
            let (oid, content) = oids[n % oids.len()].clone(); // clone: owned by the task
            tokio::spawn(async move {
                let got = p.contents(&path, &oid, u64::MAX, T).await;
                matches!(got, Ok(Contents::Found { data, .. }) if data == content)
            })
        })
        .collect();
    for t in tasks {
        assert!(t.await.expect("task"), "every read answered right");
    }
    let idle = p.idle_count(&r.path);
    assert!((1..=2).contains(&idle), "{idle} processes");
}

#[tokio::test]
async fn closing_a_repository_ends_its_processes_and_waits_for_them() {
    let r = repo();
    let p = pool(&r, config(2, 4, Duration::from_secs(60)));
    let oid = write_blob(&r.path, b"x");
    p.info(&r.path, &[&oid], T).await.unwrap();
    // One busy exchange (a long list) while the repository is closed.
    let ids: Vec<String> = (0..200_000u32).map(|i| format!("{i:040x}")).collect();
    let busy = {
        let p = p.clone(); // clone: moved into the task
        let path = r.path.clone(); // clone: moved into the task
        tokio::spawn(async move {
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            p.info(&path, &refs, T).await.is_ok()
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let path = r.path.clone(); // clone: moved to the blocking pool
    let closer = p.clone(); // clone: moved to the blocking pool
    let closed = tokio::task::spawn_blocking(move || closer.close_repo(&path, T))
        .await
        .expect("close");
    assert!(closed, "every process exited within the bound");
    assert_eq!(p.repo_count(), 0);
    // The busy exchange finished either way; its process was not kept.
    let _ = busy.await.expect("task");
    assert_eq!(p.idle_count(&r.path), 0);
    // The repository is usable again (a new process).
    p.info(&r.path, &[&oid], T).await.unwrap();
    assert_eq!(p.idle_count(&r.path), 1);
}
