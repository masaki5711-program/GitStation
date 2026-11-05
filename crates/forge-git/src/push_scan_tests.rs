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
    use std::io::Write as _;
    let mut child = Command::new("git")
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

#[tokio::test]
async fn large_and_secret_blobs_of_new_commits_are_found_with_path_and_commit() {
    let f = fixture().await;
    let base = commit(&f.path, &[("README", b"hello\n")], None);
    let tip = commit(
        &f.path,
        &[
            ("README", b"hello\n"),
            ("big.bin", &[7u8; 4096]),
            ("conf/id_rsa", &key_file()),
        ],
        Some(&base),
    );
    let report = f
        .git
        .scan_push(&f.path, None, std::slice::from_ref(&tip), &limits(1024))
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
