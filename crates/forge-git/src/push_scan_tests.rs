#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

// Sample secrets are assembled at run time, so this file never trips the
// checks of the repository that hosts it.

use std::path::{Path, PathBuf};
use std::process::Command;

use forge_core::RepoId;

use super::*;
use crate::storage::StorageRoot;
use crate::GitConfig;

struct Fixture {
    _home: tempfile::TempDir,
    _root_dir: tempfile::TempDir,
    git: Git,
    path: PathBuf,
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root_dir = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let root = StorageRoot::open(root_dir.path()).expect("root");
    let path = root
        .init_bare(&git, RepoId::new(7), uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    Fixture {
        _home: home,
        _root_dir: root_dir,
        git,
        path,
    }
}

pub(crate) fn plumb(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    plumb_in(repo, None, args, input)
}

/// `plumb` writing new objects into `quarantine` (as receive-pack does:
/// the repository's objects as the alternate).
pub(crate) fn plumb_in(
    repo: &Path,
    quarantine: Option<&Path>,
    args: &[&str],
    input: Option<&[u8]>,
) -> String {
    use std::io::Write as _;
    let mut cmd = Command::new("git");
    if let Some(q) = quarantine {
        cmd.env("GIT_OBJECT_DIRECTORY", q)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", repo.join("objects"));
    }
    let mut child = cmd
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// A commit (no ref points at it) with `files` (name, content).
pub(crate) fn commit(repo: &Path, files: &[(&str, &[u8])], parent: Option<&str>) -> String {
    commit_in(repo, None, files, parent)
}

/// A quarantine directory of `repo`, as git creates one for a push.
pub(crate) fn quarantine_dir(repo: &Path, name: &str) -> PathBuf {
    let dir = repo
        .join("objects")
        .join(format!("tmp_objdir-incoming-{name}"));
    std::fs::create_dir_all(&dir).expect("quarantine");
    dir
}

pub(crate) fn quarantine_of(repo: &Path, dir: &Path) -> Quarantine {
    Quarantine::from_hook_env(
        repo,
        Some(dir.as_os_str()),
        Some(repo.join("objects").as_os_str()),
    )
    .expect("valid")
    .expect("some")
}

/// `commit`, its new objects written into `quarantine`.
pub(crate) fn commit_in(
    repo: &Path,
    quarantine: Option<&Path>,
    files: &[(&str, &[u8])],
    parent: Option<&str>,
) -> String {
    let plumb =
        |repo: &Path, args: &[&str], input: Option<&[u8]>| plumb_in(repo, quarantine, args, input);
    // Top-level files, and files one directory down (`dir/name`).
    let mut tree = String::new();
    let mut dirs: std::collections::BTreeMap<&str, String> = std::collections::BTreeMap::new();
    for (name, content) in files {
        let blob = plumb(repo, &["hash-object", "-w", "--stdin"], Some(content));
        match name.split_once('/') {
            Some((dir, file)) => dirs
                .entry(dir)
                .or_default()
                .push_str(&format!("100644 blob {blob}\t{file}\n")),
            None => tree.push_str(&format!("100644 blob {blob}\t{name}\n")),
        }
    }
    for (dir, entries) in dirs {
        let sub = plumb(repo, &["mktree"], Some(entries.as_bytes()));
        tree.push_str(&format!("040000 tree {sub}\t{dir}\n"));
    }
    let tree = plumb(repo, &["mktree"], Some(tree.as_bytes()));
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "c"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

pub(crate) fn key_file() -> Vec<u8> {
    [
        "x = 1\n-----BEGIN RSA PRIV",
        "ATE KEY-----\nMIIEsynthetic\n",
    ]
    .concat()
    .into_bytes()
}

fn limits(max_blob_bytes: u64) -> ScanLimits {
    ScanLimits::new(max_blob_bytes)
}

/// X8 review (HIGH): a secret scan cut short by its caps must not let a
/// large blob through — the size check reads the whole quarantine.
#[tokio::test]
async fn a_capped_listing_still_refuses_a_large_blob() {
    let f = fixture().await;
    let dir = quarantine_dir(&f.path, "pad");
    let mut files: Vec<(String, Vec<u8>)> = (0..30)
        .map(|i| (format!("pad{i:02}.txt"), format!("pad {i}\n").into_bytes()))
        .collect();
    files.push(("zz-big.bin".to_owned(), vec![9u8; 8192]));
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_slice()))
        .collect();
    let tip = commit_in(&f.path, Some(&dir), &refs, None);
    let q = quarantine_of(&f.path, &dir);
    let mut l = limits(4096);
    l.max_objects = 3;
    let report = f
        .git
        .scan_push(&f.path, Some(&q), &[tip], &l)
        .await
        .expect("scan");
    assert_eq!(report.incomplete, Some("objects_cap"), "{report:?}");
    assert_eq!(report.total_findings, 1, "{report:?}");
    let finding = report.findings.first().expect("finding");
    assert_eq!(finding.kind, FindingKind::LargeBlob { size: 8192 });
    assert_eq!(finding.path, "zz-big.bin");
}

#[tokio::test]
async fn large_and_secret_blobs_of_new_commits_are_found_with_path_and_commit() {
    let f = fixture().await;
    let base = commit(&f.path, &[("README", b"hello\n")], None);
    let dir = quarantine_dir(&f.path, "both");
    let tip = commit_in(
        &f.path,
        Some(&dir),
        &[
            ("README", b"hello\n"),
            ("big.bin", &[7u8; 4096]),
            ("conf/id_rsa", &key_file()),
        ],
        Some(&base),
    );
    let q = quarantine_of(&f.path, &dir);
    let report = f
        .git
        .scan_push(&f.path, Some(&q), std::slice::from_ref(&tip), &limits(1024))
        .await
        .expect("scan");
    assert_eq!(report.incomplete, None);
    assert_eq!(report.total_findings, 2, "{report:?}");
    let rules: Vec<&str> = report.findings.iter().map(|f| f.kind.rule()).collect();
    assert!(
        rules.contains(&"large_blob") && rules.contains(&"private_key"),
        "{rules:?}"
    );
    for finding in &report.findings {
        assert_eq!(finding.commit.as_deref(), Some(tip.as_str()), "{finding:?}");
    }
    let key = report
        .findings
        .iter()
        .find(|f| f.kind.rule() == "private_key")
        .expect("key");
    assert_eq!(key.path, "conf/id_rsa");
    assert_eq!(
        report
            .findings
            .iter()
            .find(|f| f.kind.rule() == "large_blob")
            .expect("big")
            .kind,
        FindingKind::LargeBlob { size: 4096 }
    );
}

#[tokio::test]
async fn objects_already_reachable_are_not_scanned() {
    let f = fixture().await;
    let old = commit(&f.path, &[("id_rsa", &key_file())], None);
    plumb(&f.path, &["update-ref", "refs/heads/main", &old], None);
    let tip = commit(
        &f.path,
        &[("id_rsa", &key_file()), ("README", b"new\n")],
        Some(&old),
    );
    let report = f
        .git
        .scan_push(&f.path, None, &[tip], &limits(1 << 20))
        .await
        .expect("scan");
    assert!(report.findings.is_empty(), "{report:?}");
    // The new commit, its tree and the new README blob.
    assert_eq!(report.objects, 3);
}

#[tokio::test]
async fn a_marked_false_positive_is_counted_not_found() {
    let f = fixture().await;
    let mut content = b"# synthetic fixture forge:allow-secret\n".to_vec();
    content.extend_from_slice(&key_file());
    let tip = commit(&f.path, &[("fixture.pem", &content)], None);
    let report = f
        .git
        .scan_push(&f.path, None, &[tip], &limits(1 << 20))
        .await
        .expect("scan");
    assert!(report.findings.is_empty(), "{report:?}");
    assert_eq!(report.allowed, 1);
}

#[tokio::test]
async fn a_scan_past_its_byte_cap_is_incomplete_and_accepted() {
    let f = fixture().await;
    let tip = commit(
        &f.path,
        &[("a.txt", &[b'a'; 600]), ("b.txt", &[b'b'; 600])],
        None,
    );
    let mut l = limits(1 << 20);
    l.max_scan_bytes = 1000;
    let report = f
        .git
        .scan_push(&f.path, None, &[tip], &l)
        .await
        .expect("scan");
    assert_eq!(report.incomplete, Some("bytes_cap"));
    assert!(report.findings.is_empty());
    assert_eq!(report.blobs_scanned, 1);
}

#[tokio::test]
async fn the_object_cap_and_the_deadline_make_it_incomplete() {
    let f = fixture().await;
    let tip = commit(&f.path, &[("a.txt", b"a"), ("b.txt", b"b")], None);
    let mut l = limits(1 << 20);
    l.max_objects = 2;
    let report = f
        .git
        .scan_push(&f.path, None, std::slice::from_ref(&tip), &l)
        .await
        .expect("scan");
    assert_eq!(report.incomplete, Some("objects_cap"));
    assert_eq!(report.objects, 2);

    let mut l = limits(1 << 20);
    l.timeout = Duration::ZERO;
    let report = f
        .git
        .scan_push(&f.path, None, &[tip], &l)
        .await
        .expect("scan");
    assert_eq!(report.incomplete, Some("deadline"));
}

#[tokio::test]
async fn a_tip_that_is_not_an_object_id_is_refused() {
    let f = fixture().await;
    let err = f
        .git
        .scan_push(&f.path, None, &["--all".to_owned()], &limits(1))
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::PushCheckFailed);
}

#[tokio::test]
async fn a_quarantine_outside_the_repository_is_refused() {
    let f = fixture().await;
    let objects = f.path.join("objects");
    let incoming = objects.join("tmp_objdir-incoming-abc");
    std::fs::create_dir(&incoming).expect("dir");
    let ok = Quarantine::from_hook_env(
        &f.path,
        Some(incoming.as_os_str()),
        Some(objects.as_os_str()),
    )
    .expect("valid");
    assert!(ok.is_some());
    assert_eq!(
        Quarantine::from_hook_env(&f.path, None, None).expect("none"),
        None
    );

    let elsewhere = tempfile::tempdir().expect("other");
    let foreign = elsewhere.path().join("tmp_objdir-incoming-x");
    std::fs::create_dir(&foreign).expect("dir");
    for (dir, alt) in [
        (foreign.as_os_str(), objects.as_os_str()),
        (incoming.as_os_str(), elsewhere.path().as_os_str()),
        (objects.as_os_str(), objects.as_os_str()),
    ] {
        let err = Quarantine::from_hook_env(&f.path, Some(dir), Some(alt)).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PushCheckFailed);
    }
    let err =
        Quarantine::from_hook_env(&f.path, Some(incoming.as_os_str()), None).expect_err("refused");
    assert_eq!(err.code(), ErrorCode::PushCheckFailed);
}

#[test]
fn paths_lose_control_characters_and_are_cut() {
    assert_eq!(clean_path(b"a\tb\x1b[31m"), "a?b?[31m");
    let long = "é".repeat(200);
    let cut = clean_path(long.as_bytes());
    assert!(cut.len() <= 256 && cut.chars().all(|c| c == 'é'));
}

/// X8 review (MEDIUM): what the secret scan leaves out is not silent. A
/// text blob too large to scan makes it incomplete; a binary one is
/// counted; a waived finding is kept with its path for serve's log.
#[tokio::test]
async fn skipped_blobs_and_waivers_are_reported() {
    let f = fixture().await;
    let mut marked = b"# synthetic forge:allow-secret\n".to_vec();
    marked.extend_from_slice(&key_file());
    let mut binary = vec![0u8; 16];
    binary.extend_from_slice(&key_file());
    let tip = commit(
        &f.path,
        &[
            ("big.txt", &[b'a'; 3000]),
            ("image.bin", &binary),
            ("fixtures/key.pem", &marked),
        ],
        None,
    );
    let mut l = limits(1 << 20);
    l.secret_blob_max = 2048;
    let report = f
        .git
        .scan_push(&f.path, None, &[tip], &l)
        .await
        .expect("scan");
    assert!(report.findings.is_empty(), "{report:?}");
    assert_eq!(report.incomplete, Some("large_blob_unscanned"));
    assert_eq!(report.skipped_large, 1);
    assert_eq!(report.skipped_binary, 1);
    assert_eq!(report.allowed, 1);
    let waiver = report.waivers.first().expect("waiver");
    assert_eq!(waiver.rule, "private_key");
    assert_eq!(waiver.path, "fixtures/key.pem");
}

/// The size check cannot be cut short into an accept: past its bound the
/// scan is an error (FG-GIT-0023, the push refused). Its git never answers
/// (`test_git`): a zero bound is no timeout — under load a fast `cat-file`
/// answered first and the scan, complete, passed (2026-11-09, 3 of 5 runs).
#[tokio::test]
async fn a_size_check_that_cannot_finish_fails_closed() {
    let f = fixture().await;
    let dir = quarantine_dir(&f.path, "slow");
    let tip = commit_in(&f.path, Some(&dir), &[("a.txt", b"a")], None);
    let q = quarantine_of(&f.path, &dir);
    let mut l = limits(1 << 20);
    l.size_timeout = crate::test_git::STALL;
    let bin = tempfile::tempdir().expect("bin");
    // Only the size check (`cat-file --batch-all-objects`) stalls.
    let git = crate::test_git::stalled_git(f._home.path(), bin.path(), Some("--batch-all-objects"));
    let err = git
        .scan_push(&f.path, Some(&q), &[tip], &l)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::PushCheckFailed);
    assert!(
        format!("{:?}", err.context()).contains("size_check_failed"),
        "{err:?}"
    );
}

// ── 2026-11-12: the one-time history import (tip tree vs history) ───────

/// A key added by an old commit and removed later: only in history. The
/// import scan keeps it apart (with its commit and path); the full scan
/// refuses it as before.
#[tokio::test]
async fn an_import_scan_keeps_a_history_only_secret_apart() {
    let f = fixture().await;
    let old = commit(&f.path, &[("deploy/id_rsa", &key_file())], None);
    let tip = commit(&f.path, &[("README", b"clean now\n")], Some(&old));
    let l = limits(1 << 20);
    let full = f
        .git
        .scan_push(&f.path, None, std::slice::from_ref(&tip), &l)
        .await
        .expect("scan");
    assert_eq!(full.total_findings, 1, "{full:?}");
    assert!(full.history.is_empty());

    let import = f
        .git
        .scan_push_with(
            &f.path,
            None,
            std::slice::from_ref(&tip),
            &l,
            ScanMode::HistoryImport,
        )
        .await
        .expect("scan");
    assert!(import.findings.is_empty(), "{import:?}");
    assert_eq!(import.total_findings, 0);
    assert_eq!(import.total_history, 1);
    let h = import.history.first().expect("history finding");
    assert_eq!(h.kind.rule(), "private_key");
    assert_eq!(h.path, "deploy/id_rsa");
    assert_eq!(h.commit.as_deref(), Some(old.as_str()));
}

/// A key still in a pushed tip's tree refuses an import like any push,
/// also when an older commit added it.
#[tokio::test]
async fn an_import_scan_still_refuses_a_secret_in_the_tip_tree() {
    let f = fixture().await;
    let old = commit(&f.path, &[("id_rsa", &key_file())], None);
    let tip = commit(
        &f.path,
        &[("id_rsa", &key_file()), ("README", b"more\n")],
        Some(&old),
    );
    let clean = commit(&f.path, &[("README", b"other\n")], None);
    let report = f
        .git
        .scan_push_with(
            &f.path,
            None,
            &[clean, tip],
            &limits(1 << 20),
            ScanMode::HistoryImport,
        )
        .await
        .expect("scan");
    assert_eq!(report.total_findings, 1, "{report:?}");
    assert_eq!(report.findings[0].path, "id_rsa");
    assert!(report.history.is_empty(), "{report:?}");
}
