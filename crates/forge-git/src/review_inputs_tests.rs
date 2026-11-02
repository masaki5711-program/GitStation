#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use forge_core::{ObjectId, RepoId};

use super::*;
use crate::GitConfig;

const T: Duration = Duration::from_secs(20);

struct Fixture {
    _home: tempfile::TempDir,
    _root_dir: tempfile::TempDir,
    scratch: tempfile::TempDir,
    git: Git,
    root: StorageRoot,
    id: RepoId,
    path: PathBuf,
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root_dir = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let root = StorageRoot::open(root_dir.path()).expect("root");
    let id = RepoId::new(9);
    let path = root
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    Fixture {
        _home: home,
        _root_dir: root_dir,
        scratch: tempfile::tempdir().expect("scratch"),
        git,
        root,
        id,
        path,
    }
}

/// Plumbing on the bare repository (test setup only; synthetic content).
fn plumb(repo: &Path, index: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_INDEX_FILE", index)
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
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl Fixture {
    /// A commit whose tree holds `files` (nested paths allowed; `mode`
    /// 100644 unless given) on top of `parent`.
    fn commit(&self, files: &[(&str, &[u8])], parent: Option<&ObjectId>) -> ObjectId {
        self.commit_modes(
            &files
                .iter()
                .map(|(p, c)| (*p, *c, "100644"))
                .collect::<Vec<_>>(),
            parent,
        )
    }

    fn commit_modes(&self, files: &[(&str, &[u8], &str)], parent: Option<&ObjectId>) -> ObjectId {
        let index = self
            .scratch
            .path()
            .join(format!("index-{}", uuid::Uuid::new_v4()));
        for (path, content, mode) in files {
            let blob = plumb(
                &self.path,
                &index,
                &["hash-object", "-w", "--stdin"],
                Some(content),
            );
            plumb(
                &self.path,
                &index,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("{mode},{blob},{path}"),
                ],
                None,
            );
        }
        let tree = plumb(&self.path, &index, &["write-tree"], None);
        let mut args = vec!["commit-tree", tree.as_str(), "-m", "c"];
        let p;
        if let Some(parent) = parent {
            p = parent.as_str().to_owned();
            args.push("-p");
            args.push(&p);
        }
        ObjectId::parse(&plumb(&self.path, &index, &args, None)).expect("oid")
    }
}

/// ADR-0024 §9: renames are a deletion and an addition (both owners are
/// needed), nested paths are listed in full.
#[tokio::test]
async fn changed_paths_count_a_rename_as_both_paths() {
    let f = fixture().await;
    let base = f.commit(&[("src/a.rs", b"one\n"), ("docs/x.md", b"x\n")], None);
    let head = f.commit(
        &[("src/b.rs", b"one\n"), ("docs/x.md", b"y\n")],
        Some(&base),
    );
    let paths = f
        .git
        .changed_paths(&f.root, f.id, &base, &head, 100, T)
        .await
        .expect("diff");
    assert_eq!(
        paths,
        ChangedPaths::Complete(vec![
            "docs/x.md".to_owned(),
            "src/a.rs".to_owned(),
            "src/b.rs".to_owned()
        ])
    );
    let capped = f
        .git
        .changed_paths(&f.root, f.id, &base, &head, 2, T)
        .await
        .expect("diff");
    assert_eq!(capped, ChangedPaths::TooMany);
}

#[tokio::test]
async fn range_commits_are_capped() {
    let f = fixture().await;
    let base = f.commit(&[("a", b"0\n")], None);
    let one = f.commit(&[("a", b"1\n")], Some(&base));
    let two = f.commit(&[("a", b"2\n")], Some(&one));
    let range = f
        .git
        .range_commits(&f.root, f.id, Some(&base), &two, 10, T)
        .await
        .expect("rev-list");
    assert_eq!(range, Some(vec![two.clone(), one.clone()]));
    let all = f
        .git
        .range_commits(&f.root, f.id, None, &two, 10, T)
        .await
        .expect("rev-list");
    assert_eq!(all.map(|c| c.len()), Some(3));
    let over = f
        .git
        .range_commits(&f.root, f.id, Some(&base), &two, 1, T)
        .await
        .expect("rev-list");
    assert_eq!(over, None);
}

/// `.forge/CODEOWNERS` wins over `CODEOWNERS`; a file past the cap and a
/// symlink are reported, never read as empty.
#[tokio::test]
async fn codeowners_file_is_read_from_the_commit() {
    let f = fixture().await;
    let none = f.commit(&[("README", b"r\n")], None);
    assert_eq!(
        f.git
            .codeowners_file(&f.root, f.id, &none, 1024, T)
            .await
            .expect("read"),
        CodeownersFile::Absent
    );
    let root_only = f.commit(&[("CODEOWNERS", b"* @a\n")], None);
    assert_eq!(
        f.git
            .codeowners_file(&f.root, f.id, &root_only, 1024, T)
            .await
            .expect("read"),
        CodeownersFile::Found {
            path: "CODEOWNERS",
            bytes: b"* @a\n".to_vec()
        }
    );
    let both = f.commit(
        &[("CODEOWNERS", b"* @a\n"), (".forge/CODEOWNERS", b"* @b\n")],
        None,
    );
    assert_eq!(
        f.git
            .codeowners_file(&f.root, f.id, &both, 1024, T)
            .await
            .expect("read"),
        CodeownersFile::Found {
            path: ".forge/CODEOWNERS",
            bytes: b"* @b\n".to_vec()
        }
    );
    assert_eq!(
        f.git
            .codeowners_file(&f.root, f.id, &both, 3, T)
            .await
            .expect("read"),
        CodeownersFile::TooLarge {
            path: ".forge/CODEOWNERS"
        }
    );
    let link = f.commit_modes(&[("CODEOWNERS", b"elsewhere", "120000")], None);
    assert_eq!(
        f.git
            .codeowners_file(&f.root, f.id, &link, 1024, T)
            .await
            .expect("read"),
        CodeownersFile::NotAFile { path: "CODEOWNERS" }
    );
}
