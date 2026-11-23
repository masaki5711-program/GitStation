#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The nightly AI evaluation baseline candidate (plan 17 P17-6) against a
//! real bare repository and the development database: the system keeps ONE
//! draft merge request per models, authored by the schedule's owner, and
//! moves its branch forward night after night. All data is synthetic.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use forge_core::{ObjectId, ProjectId, RepoId, RequestId, Role, UserId};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{BaselineAsk, BaselineProposer, BaselineReport};

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

/// A commit holding `README.md` only, on `parent`.
fn commit(repo: &Path, parent: Option<&str>) -> String {
    let blob = plumb(repo, &["hash-object", "-w", "--stdin"], Some(b"assets\n"));
    let tree = plumb(
        repo,
        &["mktree"],
        Some(format!("100644 blob {blob}\tREADME.md\n").as_bytes()),
    );
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    path: std::path::PathBuf,
    proposer: BaselineProposer,
    owner: UserId,
    reporter: UserId,
    project: ProjectId,
    repo: RepoId,
    main: String,
}

/// `pfs-ai/assets`-like project: `main` holds README.md; olga owns it
/// (Maintainer, the schedule's owner), rita is a Reporter.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let reporter = user(&t, "rita").await;
    let assets = project(&t, owner, &[(reporter, Role::Reporter)]).await;
    let _marked = repositories::mark_ready(&t.db, &system("req-ready"), assets.repository_id)
        .await
        .expect("ready");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, assets.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let main = commit(&path, None);
    plumb(&path, &["update-ref", "refs/heads/main", &main], None);
    let proposer = BaselineProposer::new(t.db.clone(), git, storage);
    World {
        t,
        _home: home,
        _root: root,
        path,
        proposer,
        owner,
        reporter,
        project: assets.project_id,
        repo: assets.repository_id,
        main,
    }
}

impl World {
    fn ask(&self, owner: UserId, content: &str) -> BaselineAsk {
        BaselineAsk {
            project: self.project,
            repo: self.repo,
            pipeline_id: 41,
            owner,
            target: branch("main"),
            start: ObjectId::parse(&self.main).expect("sha"),
            models: "qwen3-14b".to_owned(),
            content: content.as_bytes().to_vec(),
        }
    }

    async fn propose(&self, owner: UserId, content: &str) -> BaselineReport {
        self.proposer
            .propose(
                &self.ask(owner, content),
                &RequestId::parse("req-nightly").expect("id"),
            )
            .await
            .expect("propose")
    }

    async fn open_candidates(&self) -> Vec<serde_json::Value> {
        self.t
            .json_rows(&format!(
                "SELECT iid, author_id, draft, source_branch, target_branch \
                 FROM forge.merge_requests WHERE project_id = {} AND state = 'open'",
                self.project.get()
            ))
            .await
    }

    fn file_on_branch(&self) -> String {
        plumb(
            &self.path,
            &[
                "cat-file",
                "blob",
                "refs/heads/ai-eval/baseline-qwen3-14b:evals/baselines/qwen3-14b.json",
            ],
            None,
        )
    }
}

/// Plan 17 P17-6: night after night, ONE draft merge request of the
/// candidate branch, the schedule owner's, moved forward (never a second
/// one); the same content again changes nothing; the commit is the
/// server's.
#[tokio::test]
async fn nightly_opens_one_baseline_mr() {
    let w = world().await;
    let first = w.propose(w.owner, "{\"night\":1}\n").await;
    let BaselineReport::Opened { iid } = first else {
        panic!("not opened: {first:?}");
    };
    let second = w.propose(w.owner, "{\"night\":2}\n").await;
    assert_eq!(second, BaselineReport::Updated { iid });
    let same = w.propose(w.owner, "{\"night\":2}\n").await;
    assert_eq!(same, BaselineReport::Skipped("unchanged"));

    let open = w.open_candidates().await;
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0]["iid"], iid);
    assert_eq!(open[0]["author_id"], w.owner.get());
    assert_eq!(open[0]["draft"], true);
    assert_eq!(
        open[0]["source_branch"],
        "refs/heads/ai-eval/baseline-qwen3-14b"
    );
    assert_eq!(open[0]["target_branch"], "refs/heads/main");
    assert_eq!(w.file_on_branch(), "{\"night\":2}");
    let commits = plumb(
        &w.path,
        &[
            "rev-list",
            "--count",
            "refs/heads/main..refs/heads/ai-eval/baseline-qwen3-14b",
        ],
        None,
    );
    assert_eq!(commits, "2", "one commit a night on the same branch");
    let raw = plumb(
        &w.path,
        &[
            "cat-file",
            "commit",
            "refs/heads/ai-eval/baseline-qwen3-14b",
        ],
        None,
    );
    assert!(
        raw.contains("author GitStation <ai-eval@gitstation.invalid>"),
        "{raw}"
    );
    // Audited as the system; the branch moves were recorded as pushes.
    let pushes = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.events WHERE event_type = 'repository.pushed' \
         AND actor_kind = 'system'",
    )
    .await;
    assert_eq!(pushes, 2);
    w.t.drop_now().await;
}

/// A person closed the candidate (the branch is not merged): the system
/// does not reopen it. A Reporter as the schedule's owner may not push:
/// nothing is written.
#[tokio::test]
async fn a_closed_candidate_or_an_owner_without_push_proposes_nothing() {
    let w = world().await;
    let refused = w.propose(w.reporter, "{\"night\":1}\n").await;
    assert_eq!(refused, BaselineReport::Skipped("owner_not_allowed"));
    assert!(w.open_candidates().await.is_empty());

    let BaselineReport::Opened { iid } = w.propose(w.owner, "{\"night\":1}\n").await else {
        panic!("not opened");
    };
    // Closed by a person (behind the commands' back: audited fixture).
    w.t.commit_audited(
        ("project", w.project.get()),
        &[format!(
            "UPDATE forge.merge_requests SET state = 'closed', closed_at = now(), \
             closed_by = {}, revision = revision + 1 WHERE project_id = {} AND iid = {iid}",
            w.owner.get(),
            w.project.get()
        )],
    )
    .await;
    let again = w.propose(w.owner, "{\"night\":2}\n").await;
    assert_eq!(again, BaselineReport::Skipped("branch_exists"));
    assert!(w.open_candidates().await.is_empty());
    w.t.drop_now().await;
}
