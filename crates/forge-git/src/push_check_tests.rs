#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly (rust/testing.md)

use std::num::NonZeroU64;
use std::path::PathBuf;

use forge_core::RepoId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::smart_http::Phase;
use crate::GitConfig;

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
    let id = RepoId::new(1);
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

impl Fixture {
    async fn out(&self, cmd: crate::GitCommand) -> String {
        cmd.output()
            .await
            .expect("git")
            .stdout_str()
            .trim()
            .to_owned()
    }

    /// A commit with `content` in README on top of `parent` (synthetic).
    async fn commit(&self, content: &str, parent: Option<&str>) -> String {
        let git = &self.git;
        let blob = self
            .out(
                git.command("hash-object")
                    .repo(&self.path)
                    .arg("-w")
                    .arg("--stdin")
                    .stdin(content.as_bytes().to_vec()),
            )
            .await;
        let tree = self
            .out(
                git.command("mktree")
                    .repo(&self.path)
                    .stdin(format!("100644 blob {blob}\tREADME\n").into_bytes()),
            )
            .await;
        let mut cmd = git
            .command("commit-tree")
            .config_override("user.name", "forge-test")
            .config_override("user.email", "forge-test@example.invalid")
            .repo(&self.path)
            .arg("-m")
            .arg("synthetic");
        if let Some(p) = parent {
            cmd = cmd.arg("-p").arg_owned(p);
        }
        self.out(cmd.arg_owned(&tree)).await
    }

    async fn set_ref(&self, name: &str, id: &str) {
        self.git
            .command("update-ref")
            .repo(&self.path)
            .arg_owned(name)
            .arg_owned(id)
            .output()
            .await
            .expect("update-ref");
    }
}

fn q(name: &str, old: &str, new: &str) -> CommitQuery {
    CommitQuery {
        name: RefName::parse(name).expect("ref"),
        old: old.to_owned(),
        new: new.to_owned(),
    }
}

const ZERO: &str = "0000000000000000000000000000000000000000";
const T: Duration = Duration::from_secs(10);

#[tokio::test]
async fn fast_forwards_tell_ancestors_from_rewrites_and_unknown_objects() {
    let f = fixture().await;
    let c1 = f.commit("one\n", None).await;
    let c2 = f.commit("two\n", Some(&c1)).await;
    let other = f.commit("other\n", None).await;
    let missing = "1234567890123456789012345678901234567890".to_owned();
    let got = f
        .git
        .fast_forwards(
            &f.root,
            f.id,
            &[
                (c1.clone(), c2.clone()),
                (c2.clone(), c1.clone()),
                (c1.clone(), other),
                (c1.clone(), missing.clone()),
                (missing, c1.clone()),
            ],
            T,
        )
        .await
        .expect("check");
    assert_eq!(
        got,
        [
            Ancestry::FastForward,
            Ancestry::NotFastForward,
            Ancestry::NotFastForward,
            Ancestry::Unknown,
            Ancestry::Unknown
        ]
    );
    let err = f
        .git
        .fast_forwards(&f.root, f.id, &[("main".to_owned(), c1)], T)
        .await
        .expect_err("not an id");
    assert_eq!(err.code(), ErrorCode::Internal);
}

#[tokio::test]
async fn new_commits_list_what_each_update_brings() {
    let f = fixture().await;
    let c1 = f.commit("one\n", None).await;
    let c2 = f.commit("two\n", Some(&c1)).await;
    let c3 = f.commit("three\n", Some(&c2)).await;
    let c4 = f.commit("four\n", Some(&c3)).await;
    // After the push: main c1 → c3, feature created at c4, old deleted.
    f.set_ref("refs/heads/main", &c3).await;
    f.set_ref("refs/heads/feature", &c4).await;
    let updates = [
        q("refs/heads/main", &c1, &c3),
        q("refs/heads/feature", ZERO, &c4),
        q("refs/heads/old", &c1, ZERO),
    ];
    let got = f
        .git
        .new_commits(&f.root, f.id, &updates, "user:1", T)
        .await
        .expect("commits");
    assert_eq!(got[0].commits, [c3.clone(), c2.clone()]);
    assert_eq!(got[0].total, 2);
    // The create lists what the refs before the push did not reach: main
    // was at c1, so c2 and c3 count as new for the feature branch too.
    assert_eq!(got[1].commits, [c4.clone(), c3, c2]);
    assert_eq!(got[1].total, 3);
    assert!(!got[1].partial);
    assert_eq!(got[2], CommitList::default());

    // No time left: every ref still to count is partial.
    let got = f
        .git
        .new_commits(&f.root, f.id, &updates[..1], "user:1", Duration::ZERO)
        .await
        .expect("commits");
    assert!(got[0].partial);
    assert_eq!(got[0].total, 0);
}

#[tokio::test]
async fn the_commit_list_keeps_twenty_and_counts_all() {
    let f = fixture().await;
    let base = f.commit("base\n", None).await;
    let mut tip = base.clone();
    for i in 0..25 {
        tip = f.commit(&format!("c{i}\n"), Some(&tip)).await;
    }
    f.set_ref("refs/heads/main", &tip).await;
    let got = f
        .git
        .new_commits(
            &f.root,
            f.id,
            &[q("refs/heads/main", &base, &tip)],
            "user:1",
            T,
        )
        .await
        .expect("commits");
    assert_eq!(got[0].total, 25);
    assert_eq!(got[0].commits.len(), MAX_LISTED_COMMITS);
    assert_eq!(got[0].commits[0], tip);
}

/// `deny_non_fast_forwards` makes receive-pack refuse a rewrite
/// (`ng <ref> non-fast-forward`) and keeps the ref.
#[tokio::test]
async fn deny_non_fast_forwards_refuses_a_rewrite() {
    let f = fixture().await;
    let c1 = f.commit("one\n", None).await;
    let c2 = f.commit("two\n", Some(&c1)).await;
    f.set_ref("refs/heads/main", &c2).await;
    let empty_pack = f
        .git
        .command("pack-objects")
        .repo(&f.path)
        .arg("--stdout")
        .arg("--quiet")
        .stdin(Vec::new())
        .output()
        .await
        .expect("pack")
        .stdout;
    let mut body = Vec::new();
    crate::pkt_line::encode_data(
        &mut body,
        format!("{c2} {c1} refs/heads/main\0report-status\n").as_bytes(),
    )
    .expect("command");
    body.extend_from_slice(crate::pkt_line::FLUSH);
    body.extend_from_slice(&empty_pack);
    let cmd = f
        .git
        .receive_pack(
            &f.root,
            f.id,
            Phase::Rpc,
            None,
            NonZeroU64::new(1 << 20).expect("non-zero"),
        )
        .expect("cmd")
        .deny_non_fast_forwards();
    let argv: Vec<String> = cmd
        .argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        argv.contains(&"receive.denyNonFastForwards=true".to_owned()),
        "{argv:?}"
    );
    let mut stream = cmd.spawn_stream("user:1").await.expect("spawn");
    let mut stdin = stream.take_stdin().expect("stdin");
    let mut stdout = stream.take_stdout().expect("stdout");
    stdin.write_all(&body).await.expect("write");
    stdin.shutdown().await.expect("close");
    let mut out = Vec::new();
    stdout.read_to_end(&mut out).await.expect("read");
    drop(stdout);
    stream.finish().await.expect("exit 0");
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("ng refs/heads/main non-fast-forward"),
        "{text}"
    );
    let head = f
        .out(
            f.git
                .command("rev-parse")
                .repo(&f.path)
                .arg("refs/heads/main"),
        )
        .await;
    assert_eq!(head, c2);
}
