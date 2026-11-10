#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use forge_core::{ObjectId, RefName, RepoId};
use forge_errors::ErrorCode;

use super::*;
use crate::hidden_refs::{merge_request_head_ref, merge_request_version_ref};
use crate::GitConfig;

const T: Duration = Duration::from_secs(20);

struct Fixture {
    _home: tempfile::TempDir,
    _root_dir: tempfile::TempDir,
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
    let id = RepoId::new(5);
    let path = root
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    Fixture {
        _home: home,
        _root_dir: root_dir,
        git,
        root,
        id,
        path,
    }
}

/// Plumbing on the bare repository (test setup only; synthetic content).
fn plumb(repo: &Path, args: &[&str], input: Option<&str>) -> String {
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
        stdin.write_all(input.as_bytes()).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// A commit with one file holding `content` (and `parent`).
fn commit(repo: &Path, content: &str, parent: Option<&str>) -> ObjectId {
    let blob = plumb(repo, &["hash-object", "-w", "--stdin"], Some(content));
    let tree = plumb(
        repo,
        &["mktree"],
        Some(&format!("100644 blob {blob}\tREADME\n")),
    );
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    ObjectId::parse(&plumb(repo, &args, None)).expect("oid")
}

fn name(raw: &str) -> RefName {
    RefName::parse(raw).expect("ref")
}

impl Fixture {
    async fn refs(&self, pattern: &str) -> BTreeMap<RefName, ObjectId> {
        self.git
            .read_refs(&self.root, self.id, &name(pattern), T)
            .await
            .expect("read")
    }

    async fn apply(&self, changes: &[RefChange]) -> Result<RefUpdateResult, ForgeError> {
        self.git.update_refs(&self.root, self.id, changes, T).await
    }
}

#[test]
fn update_ref_input_is_one_explicit_transaction() {
    let a = ObjectId::parse(&"a".repeat(40)).expect("oid");
    let b = ObjectId::parse(&"b".repeat(40)).expect("oid");
    let input = transaction_input(&[
        RefChange::Create {
            name: name("refs/merge-requests/1/v2"),
            new: b.clone(),
        },
        RefChange::Update {
            name: name("refs/merge-requests/1/head"),
            new: b.clone(),
            old: a.clone(),
        },
        RefChange::Delete {
            name: name("refs/merge-requests/1/v1"),
            old: a,
        },
    ]);
    let text = String::from_utf8(input).expect("utf8");
    let fields: Vec<&str> = text.split('\0').collect();
    assert_eq!(fields.first(), Some(&"start"));
    assert_eq!(fields[1], "create refs/merge-requests/1/v2");
    assert_eq!(fields[3], "update refs/merge-requests/1/head");
    assert_eq!(fields[6], "delete refs/merge-requests/1/v1");
    assert_eq!(&fields[8..], ["prepare", "commit", ""]);
}

#[tokio::test]
async fn update_ref_argv_pins_empty_hooks_and_carries_no_values() {
    let f = fixture().await;
    let cmd = f
        .git
        .with_empty_hooks(f.git.command("update-ref"))
        .expect("hooks")
        .arg("-z")
        .arg("--stdin");
    let argv: Vec<String> = cmd
        .argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let hooks = format!(
        "core.hooksPath={}",
        f.git.config().empty_hooks_dir().to_string_lossy()
    );
    assert_eq!(argv, ["-c", hooks.as_str(), "update-ref", "-z", "--stdin"]);
}

/// Version and head are written together; reading by one merge request's
/// prefix never returns another's (`refs/merge-requests/1` vs `/10`).
#[tokio::test]
async fn version_and_head_refs_are_written_in_one_transaction() {
    let f = fixture().await;
    let c1 = commit(&f.path, "one\n", None);
    let v1 = merge_request_version_ref(1, 1).expect("ref");
    let head = merge_request_head_ref(1).expect("ref");
    let done = f
        .apply(&[
            RefChange::Create {
                name: v1.clone(),
                new: c1.clone(),
            },
            RefChange::Create {
                name: head.clone(),
                new: c1.clone(),
            },
            RefChange::Create {
                name: merge_request_version_ref(10, 1).expect("ref"),
                new: c1.clone(),
            },
        ])
        .await
        .expect("applied");
    assert_eq!(done, RefUpdateResult::Applied { changes: 3 });
    let refs = f.refs("refs/merge-requests/1").await;
    assert_eq!(refs.len(), 2, "{refs:?}");
    assert_eq!(refs.get(&v1), Some(&c1));
    assert_eq!(refs.get(&head), Some(&c1));
    assert_eq!(
        f.git
            .read_ref(&f.root, f.id, &name("refs/heads/missing"), T)
            .await
            .expect("read"),
        None
    );
    assert_eq!(
        f.apply(&[]).await.expect("nothing"),
        RefUpdateResult::Nothing
    );
}

/// A refused change (an existing ref created again, a lost CAS) refuses
/// the whole transaction: nothing of it applies.
#[tokio::test]
async fn a_refused_change_leaves_every_ref_as_it_was() {
    let f = fixture().await;
    let c1 = commit(&f.path, "one\n", None);
    let c2 = commit(&f.path, "two\n", Some(c1.as_str()));
    let v1 = merge_request_version_ref(2, 1).expect("ref");
    let v2 = merge_request_version_ref(2, 2).expect("ref");
    let head = merge_request_head_ref(2).expect("ref");
    f.apply(&[
        RefChange::Create {
            name: v1.clone(),
            new: c1.clone(),
        },
        RefChange::Create {
            name: head.clone(),
            new: c1.clone(),
        },
    ])
    .await
    .expect("first");
    // v1 exists: creating it again refuses v2 and head too.
    let err = f
        .apply(&[
            RefChange::Create {
                name: v2.clone(),
                new: c2.clone(),
            },
            RefChange::Create {
                name: v1.clone(),
                new: c2.clone(),
            },
            RefChange::Update {
                name: head.clone(),
                new: c2.clone(),
                old: c1.clone(),
            },
        ])
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::GitProcessFailed);
    // A wrong old value (a lost compare-and-swap) is refused as well.
    let err = f
        .apply(&[RefChange::Update {
            name: head.clone(),
            new: c2.clone(),
            old: c2.clone(),
        }])
        .await
        .expect_err("cas");
    assert_eq!(err.code(), ErrorCode::GitProcessFailed);
    let refs = f.refs("refs/merge-requests/2").await;
    assert_eq!(refs.get(&head), Some(&c1));
    assert!(!refs.contains_key(&v2));
    // Deleting needs the current value too.
    let err = f
        .apply(&[RefChange::Delete {
            name: v1.clone(),
            old: c2.clone(),
        }])
        .await
        .expect_err("cas delete");
    assert_eq!(err.code(), ErrorCode::GitProcessFailed);
    f.apply(&[RefChange::Delete {
        name: v1.clone(),
        old: c1.clone(),
    }])
    .await
    .expect("deleted");
    assert!(!f.refs("refs/merge-requests/2").await.contains_key(&v1));
}

/// ADR-0024 §2: after a force push replaced version 1's commit on the
/// branch, `v1` keeps it alive through `git gc --prune=now`.
#[tokio::test]
async fn a_version_ref_keeps_a_force_pushed_commit_through_gc() {
    let f = fixture().await;
    let c1 = commit(&f.path, "reviewed\n", None);
    plumb(
        &f.path,
        &["update-ref", "refs/heads/feature", c1.as_str()],
        None,
    );
    f.apply(&[RefChange::Create {
        name: merge_request_version_ref(3, 1).expect("ref"),
        new: c1.clone(),
    }])
    .await
    .expect("v1");
    // Force push: an unrelated commit replaces the branch.
    let other = commit(&f.path, "rewritten\n", None);
    plumb(
        &f.path,
        &["update-ref", "refs/heads/feature", other.as_str()],
        None,
    );
    plumb(
        &f.path,
        &["reflog", "expire", "--expire=now", "--all"],
        None,
    );
    plumb(&f.path, &["gc", "--prune=now", "--quiet"], None);
    assert_eq!(
        plumb(&f.path, &["cat-file", "-t", c1.as_str()], None),
        "commit"
    );
    // Without the ref the same gc removes it (the test can see a loss).
    let gone = commit(&f.path, "never referenced\n", None);
    plumb(&f.path, &["gc", "--prune=now", "--quiet"], None);
    let status = Command::new("git")
        .arg("--git-dir")
        .arg(&f.path)
        .args(["cat-file", "-e", gone.as_str()])
        .status()
        .expect("git");
    assert!(!status.success(), "an unreferenced commit is pruned");
}

/// M3 O3: a ref transaction that needs `packed-refs.lock` while the
/// maintenance's pack-refs holds it (here: a test holding the lock for
/// 1.5 s, longer than git's default 1 s) waits for it
/// (`core.packedRefsTimeout`, 10 s) instead of failing: the purge of a
/// packed version ref goes through once pack-refs is done.
#[tokio::test]
async fn version_ref_write_during_pack_refs_is_retried() {
    let f = fixture().await;
    let c1 = commit(&f.path, "one\n", None);
    let v1 = merge_request_version_ref(2, 1).expect("ref");
    f.apply(&[RefChange::Create {
        name: v1.clone(),
        new: c1.clone(),
    }])
    .await
    .expect("v1");
    plumb(&f.path, &["pack-refs", "--all", "--prune"], None);
    let lock = f.path.join("packed-refs.lock");
    std::fs::write(&lock, b"").expect("hold packed-refs.lock");
    let release = {
        let lock = lock.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            std::fs::remove_file(&lock).expect("release the lock");
        })
    };
    let done = f
        .apply(&[RefChange::Delete {
            name: v1.clone(),
            old: c1,
        }])
        .await;
    release.await.expect("released");
    assert_eq!(
        done.expect("waited for the lock"),
        RefUpdateResult::Applied { changes: 1 }
    );
    assert!(f.refs("refs/merge-requests/2").await.is_empty());
}

/// M4 C4: the pipeline ref is created once and kept; a pipeline ref at
/// another commit is a defect, never moved.
#[tokio::test]
async fn the_pipeline_ref_is_created_once_and_never_moved() {
    let f = fixture().await;
    let c1 = commit(&f.path, "one\n", None);
    let c2 = commit(&f.path, "two\n", Some(c1.as_str()));
    let made = f
        .git
        .ensure_pipeline_ref(&f.root, f.id, 3, &c1, T)
        .await
        .expect("created");
    assert_eq!(made.as_str(), "refs/pipelines/3/head");
    // Again: kept as is.
    f.git
        .ensure_pipeline_ref(&f.root, f.id, 3, &c1, T)
        .await
        .expect("kept");
    assert_eq!(f.refs("refs/pipelines").await.get(&made), Some(&c1));
    let err = f
        .git
        .ensure_pipeline_ref(&f.root, f.id, 3, &c2, T)
        .await
        .expect_err("moved");
    assert_eq!(err.code(), ErrorCode::Internal);
    assert_eq!(f.refs("refs/pipelines").await.get(&made), Some(&c1));
}

/// M4 C4 review: a finished pipeline's ref is deleted (a second delete is
/// a no-op); the others are kept.
#[tokio::test]
async fn a_pipeline_ref_is_deleted_once_its_pipeline_ends() {
    let f = fixture().await;
    let c1 = commit(&f.path, "one\n", None);
    let three = f
        .git
        .ensure_pipeline_ref(&f.root, f.id, 3, &c1, T)
        .await
        .expect("created");
    let four = f
        .git
        .ensure_pipeline_ref(&f.root, f.id, 4, &c1, T)
        .await
        .expect("created");
    assert!(f
        .git
        .delete_pipeline_ref(&f.root, f.id, 3, T)
        .await
        .expect("deleted"));
    assert!(!f
        .git
        .delete_pipeline_ref(&f.root, f.id, 3, T)
        .await
        .expect("already gone"));
    let left = f.refs("refs/pipelines").await;
    assert_eq!(left.get(&three), None);
    assert_eq!(left.get(&four), Some(&c1));
}

/// M4 C4 review, the sweep's backstop: every pipeline ref with its id;
/// `delete_pipeline_refs` removes exactly the given ones in one
/// transaction (CAS on the listed value).
#[tokio::test]
async fn pipeline_refs_are_listed_by_id_and_deleted_in_one_transaction() {
    let f = fixture().await;
    let c1 = commit(&f.path, "one\n", None);
    for id in [3, 4, 12] {
        f.git
            .ensure_pipeline_ref(&f.root, f.id, id, &c1, T)
            .await
            .expect("created");
    }
    let listed = f.git.pipeline_refs(&f.root, f.id, T).await.expect("list");
    let ids: Vec<i64> = listed.iter().map(|r| r.pipeline_id).collect();
    assert_eq!(ids, [3, 4, 12]);
    assert!(listed.iter().all(|r| r.at == c1));
    let gone: Vec<PipelineRef> = listed.into_iter().filter(|r| r.pipeline_id != 4).collect();
    assert_eq!(
        f.git
            .delete_pipeline_refs(&f.root, f.id, &gone, T)
            .await
            .expect("deleted"),
        RefUpdateResult::Applied { changes: 2 }
    );
    let left = f.git.pipeline_refs(&f.root, f.id, T).await.expect("list");
    assert_eq!(left.iter().map(|r| r.pipeline_id).collect::<Vec<_>>(), [4]);
}
