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
    /// A commit of `files` (path, content, mode); a `160000` entry's
    /// content is the commit id it points at.
    fn commit(&self, files: &[(&str, &[u8], &str)], parent: Option<&ObjectId>) -> ObjectId {
        let index = self
            .scratch
            .path()
            .join(format!("index-{}", uuid::Uuid::new_v4()));
        for (path, content, mode) in files {
            let object = if *mode == "160000" {
                String::from_utf8(content.to_vec()).expect("id")
            } else {
                plumb(
                    &self.path,
                    &index,
                    &["hash-object", "-w", "--stdin"],
                    Some(content),
                )
            };
            plumb(
                &self.path,
                &index,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("{mode},{object},{path}"),
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

/// Modes and line counts of every kind of change: an edit, an exec bit, a
/// symlink, a submodule, a binary file, a deletion.
#[tokio::test]
async fn changed_entries_carry_modes_and_line_counts() {
    let f = fixture().await;
    let base = f.commit(
        &[
            ("a.txt", b"one\n", "100644"),
            ("run.sh", b"echo\n", "100644"),
            ("gone.txt", b"x\n", "100644"),
        ],
        None,
    );
    let head = f.commit(
        &[
            ("a.txt", b"one\ntwo\nthree\n", "100644"),
            ("run.sh", b"echo\n", "100755"),
            ("link", b"a.txt", "120000"),
            ("sub", base.as_str().as_bytes(), "160000"),
            ("blob.bin", b"\x00\x01\x02", "100644"),
        ],
        Some(&base),
    );
    let ChangeListing::Complete(entries) = f
        .git
        .changed_entries(&f.root, f.id, &base, &head, 100, T)
        .await
        .expect("diff")
    else {
        panic!("complete");
    };
    let get = |p: &str| entries.iter().find(|e| e.path == p).expect(p).clone();
    let a = get("a.txt");
    assert_eq!(
        (a.old_mode, a.new_mode, a.added, a.deleted),
        (0o100644, 0o100644, Some(2), Some(0))
    );
    let run = get("run.sh");
    assert_eq!((run.old_mode, run.new_mode), (0o100644, 0o100755));
    assert_eq!(get("link").new_mode, 0o120000);
    assert_eq!(get("sub").new_mode, 0o160000);
    assert_eq!(get("blob.bin").added, None, "binary");
    let gone = get("gone.txt");
    assert_eq!((gone.new_mode, gone.deleted), (0, Some(1)));
    let capped = f
        .git
        .changed_entries(&f.root, f.id, &base, &head, 3, T)
        .await
        .expect("diff");
    assert_eq!(capped, ChangeListing::TooMany);
}
