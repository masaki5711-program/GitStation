#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `BranchProbe` on the real `forge_git::browse::Browser` (what `forge
//! serve` wires) against a real bare repository with synthetic content:
//! only an existing branch of that exact name answers `Ok`; a tag of the
//! same name, a missing branch and a missing or broken repository never do
//! — the last ones fail with their own error, never as "exists" and never
//! as `ref_not_found`.

use std::path::Path;
use std::process::{Command, Stdio};

use forge_core::{RefName, RepoId};
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::BranchProbe;

/// Plumbing on the bare repository (test setup only; synthetic content).
fn plumb(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
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

fn name(raw: &str) -> RefName {
    RefName::parse(raw).expect("ref")
}

#[tokio::test]
async fn browser_probe_accepts_only_an_existing_branch() {
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let id = RepoId::new(21);
    let path = storage
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let blob = plumb(
        &path,
        &["hash-object", "-w", "--stdin"],
        Some(b"# synthetic\n"),
    );
    let tree = plumb(
        &path,
        &["mktree"],
        Some(format!("100644 blob {blob}\tREADME.md\n").as_bytes()),
    );
    let commit = plumb(&path, &["commit-tree", &tree, "-m", "synthetic"], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &commit], None);
    plumb(&path, &["update-ref", "refs/tags/v1", &commit], None);

    let browser = Browser::new(git, storage, BrowseLimits::default());
    let probe: &dyn BranchProbe = &browser;
    let caller = "user:1";

    let head = probe
        .branch_head(id, &name("refs/heads/feature"), caller)
        .await
        .expect("the branch exists");
    assert_eq!(head.as_str(), commit, "the head is the branch tip");
    for missing in ["refs/heads/v1", "refs/heads/nope", "refs/heads/feature/x"] {
        let err = probe
            .branch_head(id, &name(missing), caller)
            .await
            .expect_err(missing);
        assert_eq!(
            err.code(),
            ErrorCode::RepositoryPathNotResolved,
            "{missing}: {err}"
        );
        assert!(err.to_string().contains("reason=ref_not_found"), "{err}");
    }

    // A repository that is not there: its own error, never "exists" and
    // never mistaken for a missing branch.
    let err = probe
        .branch_head(RepoId::new(999), &name("refs/heads/feature"), caller)
        .await
        .expect_err("no repository");
    assert_ne!(err.code(), ErrorCode::RepositoryPathNotResolved, "{err}");

    // A broken repository (HEAD and refs gone): the same.
    std::fs::remove_file(path.join("HEAD")).expect("break HEAD");
    std::fs::remove_dir_all(path.join("refs")).expect("break refs");
    browser.invalidate(id);
    let err = probe
        .branch_head(id, &name("refs/heads/feature"), caller)
        .await
        .expect_err("broken repository");
    assert_ne!(err.code(), ErrorCode::RepositoryPathNotResolved, "{err}");
}
