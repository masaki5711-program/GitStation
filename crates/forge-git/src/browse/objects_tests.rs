#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! The batch read path answers exactly like the one-shot commands, falls
//! back when its process fails, and its cache stays bounded.

use std::time::Duration;

use super::objects::{canon_mode, parse_raw_tree};
use super::tests::{blob, commit, fixture, mktree, reason, update_ref, Fixture};
use super::*;

fn one_shot() -> BrowseLimits {
    BrowseLimits {
        batch_per_repo: 0,
        object_cache_bytes: 0,
        ..BrowseLimits::default()
    }
}

struct World {
    c1: String,
    c2: String,
    tree2: String,
    files: Vec<&'static str>,
    dirs: Vec<&'static str>,
}

/// Every kind of entry: regular, executable, empty, binary, symlink,
/// submodule, nested directories, names with spaces, non-ASCII and
/// option-like names.
fn world(f: &Fixture) -> World {
    let p = f.path.as_path();
    let readme = blob(p, b"# synthetic\n");
    let script = blob(p, b"#!/bin/sh\necho hi\n");
    let empty = blob(p, b"");
    let binary: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    let bin = blob(p, &binary);
    let deep = blob(p, b"deep\n");
    let target = blob(p, b"README.md");
    let c = mktree(p, &[("100644", "blob", &deep, "c.txt")]);
    let b_dir = mktree(p, &[("040000", "tree", &c, "b")]);
    let sub_tree = mktree(p, &[("100644", "blob", &readme, "x")]);
    let sub_commit = commit(p, &sub_tree, None);
    let tree1 = mktree(
        p,
        &[
            ("100644", "blob", &readme, "README.md"),
            ("040000", "tree", &b_dir, "a"),
        ],
    );
    let c1 = commit(p, &tree1, None);
    let tree2 = mktree(
        p,
        &[
            ("100644", "blob", &readme, "README.md"),
            ("100755", "blob", &script, "run.sh"),
            ("100644", "blob", &empty, "empty"),
            ("100644", "blob", &bin, "data.bin"),
            ("100644", "blob", &deep, "a b.txt"),
            ("100644", "blob", &deep, "日本語.md"),
            ("100644", "blob", &deep, "-n"),
            ("120000", "blob", &target, "link"),
            ("160000", "commit", &sub_commit, "sub"),
            ("040000", "tree", &b_dir, "a"),
        ],
    );
    let c2 = commit(p, &tree2, Some(&c1));
    update_ref(p, "refs/heads/main", &c2);
    World {
        c1,
        c2,
        tree2,
        files: vec![
            "README.md",
            "run.sh",
            "empty",
            "data.bin",
            "a b.txt",
            "日本語.md",
            "-n",
            "link",
            "a/b/c.txt",
        ],
        dirs: vec!["", "a", "a/b"],
    }
}

/// An error's code and reason (what callers act on).
fn shape<T: std::fmt::Debug>(r: Result<T, ForgeError>) -> Result<T, (ErrorCode, String)> {
    r.map_err(|e| (e.code(), reason(&e)))
}

#[tokio::test]
async fn batch_and_one_shot_answer_alike() {
    let f = fixture().await;
    let w = world(&f);
    let fast = f.browser(BrowseLimits::default());
    let slow = f.browser(one_shot());
    for commit in [&w.c1, &w.c2] {
        let paths = w.dirs.iter().chain(&w.files).copied().chain([
            "missing",
            "README.md/x",
            "a/missing",
            "sub",
            "sub/x",
        ]);
        for raw in paths {
            let path = RepoPath::parse(raw).unwrap();
            // Twice: the second answer comes from the cache.
            for _ in 0..2 {
                assert_eq!(
                    shape(fast.tree(f.id, commit, &path).await),
                    shape(slow.tree(f.id, commit, &path).await),
                    "tree {raw:?} at {commit}"
                );
                if path.is_root() {
                    continue;
                }
                assert_eq!(
                    shape(fast.blob(f.id, commit, &path).await),
                    shape(slow.blob(f.id, commit, &path).await),
                    "blob {raw:?}"
                );
                assert_eq!(
                    shape(fast.raw_entry(f.id, commit, &path).await),
                    shape(slow.raw_entry(f.id, commit, &path).await),
                    "raw entry {raw:?}"
                );
                let small = |b: &Browser| {
                    let path = path.clone(); // clone: owned by the call
                    let b = b.clone(); // clone: a shared handle
                    let id = f.id;
                    let commit = commit.clone(); // clone: owned by the call
                    async move { b.small_file(id, &commit, &path, 100).await }
                };
                assert_eq!(
                    shape(small(&fast).await),
                    shape(small(&slow).await),
                    "small file {raw:?}"
                );
                match (
                    fast.raw(f.id, commit, &path, "user:1").await,
                    slow.raw(f.id, commit, &path, "user:1").await,
                ) {
                    (Ok(a), Ok(b)) => {
                        assert_eq!((a.size, &a.oid), (b.size, &b.oid));
                        match (a.body, b.body) {
                            (RawBody::Bytes(x), RawBody::Bytes(y)) => assert_eq!(x, y),
                            (x, y) => panic!("small raw files are read whole: {x:?} {y:?}"),
                        }
                    }
                    (Err(a), Err(b)) => {
                        assert_eq!((a.code(), reason(&a)), (b.code(), reason(&b)));
                    }
                    (a, b) => panic!("raw {raw:?}: {a:?} vs {b:?}"),
                }
            }
        }
    }
    // A commit named by id (and a tree id, and an id that does not exist).
    for id in [
        w.c1.as_str(),
        w.c2.as_str(),
        w.tree2.as_str(),
        &"0".repeat(40),
    ] {
        let a = fast.resolve_ref(f.id, id, "user:fast").await;
        let b = slow.resolve_ref(f.id, id, "user:slow").await;
        assert_eq!(
            a.as_ref()
                .map_err(|e| (e.code(), reason(e), e.context().to_vec())),
            b.as_ref()
                .map_err(|e| (e.code(), reason(e), e.context().to_vec())),
            "resolve {id}"
        );
    }
    // History pages, twice (the second from the cache).
    for raw in ["", "a", "a/b/c.txt", "README.md", "missing"] {
        let path = RepoPath::parse(raw).unwrap();
        for _ in 0..2 {
            let a = fast
                .commits(f.id, &w.c2, &path, 1, None, "u")
                .await
                .unwrap();
            let b = slow
                .commits(f.id, &w.c2, &path, 1, None, "u")
                .await
                .unwrap();
            assert_eq!(a, b, "commits {raw:?}");
            if let Some(next) = &a.next {
                let a2 = fast
                    .commits(f.id, &w.c2, &path, 1, Some(next), "u")
                    .await
                    .unwrap();
                let b2 = slow
                    .commits(f.id, &w.c2, &path, 1, Some(next), "u")
                    .await
                    .unwrap();
                assert_eq!(a2, b2, "second page {raw:?}");
            }
        }
    }
}

#[tokio::test]
async fn a_warm_tree_and_blob_start_no_git() {
    let f = fixture().await;
    let w = world(&f);
    let b = f.browser(BrowseLimits::default());
    let dir = RepoPath::parse("a/b").unwrap();
    let file = RepoPath::parse("a/b/c.txt").unwrap();
    b.tree(f.id, &w.c2, &dir).await.unwrap();
    b.blob(f.id, &w.c2, &file).await.unwrap();
    let before = crate::git_spawns();
    for _ in 0..5 {
        b.tree(f.id, &w.c2, &RepoPath::root()).await.unwrap();
        b.tree(f.id, &w.c2, &dir).await.unwrap();
        b.blob(f.id, &w.c2, &file).await.unwrap();
    }
    // Other tests of this binary may start git meanwhile (cargo test runs
    // them in threads); nextest runs this one alone in its process.
    if std::env::var_os("NEXTEST").is_some() {
        assert_eq!(crate::git_spawns(), before, "no git started");
    }
}

#[tokio::test]
async fn a_batch_process_that_dies_mid_answer_falls_back() {
    let f = fixture().await;
    let w = world(&f);
    let b = f.browser(BrowseLimits::default());
    let file = RepoPath::parse("README.md").unwrap();
    let first = b.blob(f.id, &w.c2, &file).await.unwrap();
    let pool = b.batch.clone().expect("a pool");
    assert_eq!(pool.idle_count(&f.path), 1);
    // The idle process dies; the next read meets it, logs FG-GIT-0032 and
    // is answered by the one-shot command.
    pool.kill_idle(&f.path).await;
    let other = RepoPath::parse("a/b/c.txt").unwrap();
    let blob = b.blob(f.id, &w.c2, &other).await.unwrap();
    assert_eq!(blob.content, b"deep\n");
    assert_eq!(b.blob(f.id, &w.c2, &file).await.unwrap(), first);
    // The pool itself reports the failure with the registered code.
    pool.kill_idle(&f.path).await;
    let Err(crate::batch::BatchError::Failed(e)) = pool
        .info(&f.path, &[w.c2.as_str()], Duration::from_secs(5))
        .await
    else {
        panic!("a dead process fails");
    };
    assert_eq!(e.code(), ErrorCode::GitBatchFailed);
}

#[tokio::test]
async fn a_batch_process_that_never_answers_falls_back_within_the_deadline() {
    let f = fixture().await;
    let w = world(&f);
    let fake = tempfile::tempdir().expect("fake");
    let git = crate::test_git::stalled_git(&f.home, fake.path(), Some("--batch-command"));
    let b = Browser::new(
        git,
        f.storage.clone(),
        BrowseLimits {
            batch_timeout: crate::test_git::STALL,
            ..BrowseLimits::default()
        },
    );
    let started = std::time::Instant::now();
    let blob = b
        .blob(f.id, &w.c2, &RepoPath::parse("a/b/c.txt").unwrap())
        .await
        .unwrap();
    assert_eq!(blob.content, b"deep\n");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "fell back in time"
    );
}

#[tokio::test]
async fn many_concurrent_reads_on_one_repository_answer_right() {
    let f = fixture().await;
    let w = world(&f);
    let b = f.browser(BrowseLimits {
        max_per_repo: 64,
        batch_wait: Duration::from_secs(5),
        ..BrowseLimits::default()
    });
    let slow = f.browser(one_shot());
    let mut expected = Vec::new();
    for raw in &w.files {
        let path = RepoPath::parse(raw).unwrap();
        expected.push((path.clone(), slow.blob(f.id, &w.c2, &path).await.unwrap()));
    }
    let tasks: Vec<_> = (0..48usize)
        .map(|n| {
            let b = b.clone(); // clone: one handle per task
            let (path, want) = expected[n % expected.len()].clone(); // clone: owned by the task
            let (id, c2) = (f.id, w.c2.clone()); // clone: owned by the task
            tokio::spawn(async move {
                let tree = b.tree(id, &c2, &RepoPath::root()).await.is_ok();
                tree && b.blob(id, &c2, &path).await.ok() == Some(want)
            })
        })
        .collect();
    for t in tasks {
        assert!(t.await.expect("task"));
    }
    let idle = b.batch.as_ref().expect("pool").idle_count(&f.path);
    assert!((1..=2).contains(&idle), "{idle} processes");
}

#[tokio::test]
async fn the_object_cache_stays_within_its_bytes() {
    let f = fixture().await;
    let w = world(&f);
    let cap = 4 * 1024;
    let b = f.browser(BrowseLimits {
        object_cache_bytes: cap,
        ..BrowseLimits::default()
    });
    let slow = f.browser(one_shot());
    for _ in 0..3 {
        for raw in w.dirs.iter().chain(&w.files) {
            let path = RepoPath::parse(raw).unwrap();
            assert_eq!(
                shape(b.tree(f.id, &w.c2, &path).await),
                shape(slow.tree(f.id, &w.c2, &path).await)
            );
            assert!(b.object_cache_bytes() <= cap);
        }
    }
}

#[test]
fn modes_are_canonical_like_git() {
    assert_eq!(canon_mode(0o100_644), 0o100_644);
    assert_eq!(canon_mode(0o100_664), 0o100_644);
    assert_eq!(canon_mode(0o100_755), 0o100_755);
    assert_eq!(canon_mode(0o100_775), 0o100_755);
    assert_eq!(canon_mode(0o040_000), 0o040_000);
    assert_eq!(canon_mode(0o040_755), 0o040_000);
    assert_eq!(canon_mode(0o120_000), 0o120_000);
    assert_eq!(canon_mode(0o160_000), 0o160_000);
}

#[test]
fn raw_trees_parse_or_are_refused() {
    let id = [0xabu8; 20];
    let mut data = b"100644 a\0".to_vec();
    data.extend_from_slice(&id);
    data.extend_from_slice(b"40000 dir\0");
    data.extend_from_slice(&id);
    let entries = parse_raw_tree(&data, 20).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].mode, 0o040_000);
    assert_eq!(entries[1].name, b"dir");
    assert_eq!(entries[0].oid, "ab".repeat(20));
    assert_eq!(parse_raw_tree(b"", 20), Some(Vec::new()));
    // Cut in the id, a bad mode, an empty name, no NUL.
    for bad in [
        data.get(..data.len() - 1).unwrap().to_vec(),
        b"10064x a\0".iter().chain(&id).copied().collect(),
        b"100644 \0".iter().chain(&id).copied().collect(),
        b"100644 a".to_vec(),
    ] {
        assert_eq!(parse_raw_tree(&bad, 20), None, "{bad:?}");
    }
}

// ── releasing a repository (purge, gc, restore) ─────────────────────────

/// A fake git whose `cat-file --batch-command` exits with git's usage
/// status (a git older than 2.36); everything else is the real git.
fn git_without_batch_command(f: &Fixture, dir: &std::path::Path) -> crate::Git {
    #[cfg(unix)]
    let (path, body) = (
        dir.join("git"),
        "#!/bin/sh\ncase \" $* \" in\n*\" --batch-command \"*) exit 129;;\nesac\nexec git \"$@\"\n",
    );
    #[cfg(windows)]
    let (path, body) = (
        dir.join("git.cmd"),
        "@echo off\r\necho %* | findstr /c:\"--batch-command\" >nul\r\n\
         if not errorlevel 1 exit /b 129\r\ngit.exe %*\r\n",
    );
    crate::test_git::write_script(&path, body);
    crate::Git::new(crate::GitConfig::new(path, &f.home))
}

#[tokio::test]
async fn a_git_without_batch_command_turns_the_pool_off_once() {
    let f = fixture().await;
    let w = world(&f);
    let fake = tempfile::tempdir().expect("fake");
    let b = Browser::new(
        git_without_batch_command(&f, fake.path()),
        f.storage.clone(),
        BrowseLimits::default(),
    );
    let file = RepoPath::parse("a/b/c.txt").unwrap();
    for _ in 0..3 {
        let blob = b.blob(f.id, &w.c2, &file).await.unwrap();
        assert_eq!(blob.content, b"deep\n");
    }
    let pool = b.batch.clone().expect("a pool");
    assert!(pool.is_unsupported(), "latched off after the first process");
    assert_eq!(pool.idle_count(&f.path), 0, "no process is kept or started");
}

#[tokio::test]
async fn a_purge_right_after_a_browse_succeeds() {
    let f = fixture().await;
    let w = world(&f);
    let b = f.browser(BrowseLimits::default());
    let file = RepoPath::parse("a/b/c.txt").unwrap();
    b.blob(f.id, &w.c2, &file).await.unwrap();
    let pool = b.batch.clone().expect("a pool");
    assert_eq!(
        pool.idle_count(&f.path),
        1,
        "a process holds the repository"
    );
    let removed = f.storage.remove_blocking(f.id).expect("removed");
    assert!(removed.existed);
    assert!(!f.path.exists());
    assert_eq!(pool.idle_count(&f.path), 0, "its processes were closed");
}

/// Another process's batch process (a `forge serve` while the worker
/// purges) is not closed by the release; it must not keep the directory
/// from being renamed (its working directory is not the repository).
#[tokio::test]
async fn a_live_batch_process_does_not_hold_the_repository_directory() {
    let f = fixture().await;
    let w = world(&f);
    let b = f.browser(BrowseLimits::default());
    b.blob(f.id, &w.c2, &RepoPath::parse("data.bin").unwrap())
        .await
        .unwrap();
    assert_eq!(b.batch.as_ref().expect("pool").idle_count(&f.path), 1);
    let aside = f.path.with_extension("aside");
    std::fs::rename(&f.path, &aside).expect("renamed while the process lives");
    std::fs::remove_dir_all(&aside).expect("removed while the process lives");
}

#[tokio::test]
async fn gc_right_after_a_browse_runs_and_closes_the_processes() {
    let f = fixture().await;
    let w = world(&f);
    let b = f.browser(BrowseLimits::default());
    b.tree(f.id, &w.c2, &RepoPath::root()).await.unwrap();
    let ran = f
        .git
        .gc_objects(
            &f.storage,
            f.id,
            Duration::from_secs(48 * 3600),
            Duration::from_secs(60),
        )
        .await
        .expect("gc");
    assert_eq!(ran, crate::maintenance::GcRun::Ran);
    assert_eq!(b.batch.as_ref().expect("pool").idle_count(&f.path), 0);
    // The browser reads the repacked repository right after.
    let tree = b.tree(f.id, &w.c2, &RepoPath::parse("a").unwrap()).await;
    assert!(tree.is_ok(), "{tree:?}");
}

#[tokio::test]
async fn releasing_a_repository_drops_its_cached_objects() {
    let f = fixture().await;
    let w = world(&f);
    let b = f.browser(BrowseLimits::default());
    let dir = RepoPath::parse("a/b").unwrap();
    let before = b.tree(f.id, &w.c2, &dir).await.unwrap();
    b.commits(f.id, &w.c2, &RepoPath::root(), 5, None, "u")
        .await
        .unwrap();
    assert!(b.objects.entries_of(f.id) > 0);
    assert!(release_repository(&f.storage, f.id));
    assert_eq!(b.objects.entries_of(f.id), 0, "nothing of it is cached");
    assert_eq!(b.object_cache_bytes(), 0);
    // Read again from git, the same answer.
    assert_eq!(b.tree(f.id, &w.c2, &dir).await.unwrap(), before);
}

#[test]
fn git_saying_a_file_is_in_use_is_recognised() {
    use crate::maintenance::files_in_use;
    for said in [
        "warning: unable to unlink '.git/objects/pack/pack-1.pack': Permission denied",
        "Unlink of file 'objects/pack/pack-1.idx' failed. Should I try again? (y/n) n",
        "Deletion of directory 'objects/17' failed. Should I try again? (y/n) n",
        "error: The process cannot access the file because it is being used by another process.",
    ] {
        assert!(files_in_use(said), "{said}");
    }
    assert!(!files_in_use(""));
    assert!(!files_in_use("fatal: gc is already running on machine"));
}

/// Windows: a directory another process works in cannot be renamed. The
/// removal waits and retries once; a holder that went away meanwhile is
/// no failure, one that stays is `reason=files_in_use`.
#[cfg(windows)]
#[tokio::test]
async fn a_removal_blocked_by_another_process_is_retried_once() {
    use std::process::{Command, Stdio};
    let f = fixture().await;
    let known = blob(&f.path, b"x");
    // A git working in the repository directory, seen to answer (so it
    // runs there), like the one forge serve kept before the fix.
    let hold = |f: &Fixture| {
        use std::io::{BufRead as _, Write as _};
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(&f.path)
            .args(["cat-file", "--batch-command"])
            .current_dir(&f.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("holder");
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, "info {known}").expect("write");
        stdin.flush().expect("flush");
        let mut out = std::io::BufReader::new(child.stdout.take().expect("stdout"));
        let mut line = String::new();
        out.read_line(&mut line).expect("answer");
        assert!(line.starts_with(&known), "{line}");
        (child, stdin, out)
    };
    // Held for the whole time: refused, said.
    let (mut holder, _stdin, _out) = hold(&f);
    let e = f
        .storage
        .remove_blocking_retrying(f.id, Duration::from_millis(200))
        .expect_err("in use");
    assert_eq!(e.code(), ErrorCode::StorageIoFailed);
    assert_eq!(reason(&e), "files_in_use");
    // Gone before the retry: removed.
    let storage = f.storage.clone();
    let id = f.id;
    let remover = std::thread::spawn(move || {
        storage.remove_blocking_retrying(id, Duration::from_millis(1500))
    });
    std::thread::sleep(Duration::from_millis(300));
    holder.kill().expect("kill");
    holder.wait().expect("wait");
    let removed = remover.join().expect("thread").expect("removed");
    assert!(removed.existed);
}
