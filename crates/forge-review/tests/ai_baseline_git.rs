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
    let _on = forge_db::ai_assets::set_enabled_decided(
        &t.db,
        &system("req-ai-assets"),
        assets.project_id,
        owner,
        true,
        |_| Ok(()),
    )
    .await
    .expect("ai_assets on");
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

    /// `sql` as an audited fixture (a person's change behind the commands).
    async fn audited(&self, sql: String) {
        self.t
            .commit_audited(("project", self.project.get()), &[sql])
            .await;
    }

    fn tip(&self) -> String {
        plumb(
            &self.path,
            &["rev-parse", "refs/heads/ai-eval/baseline-qwen3-14b"],
            None,
        )
    }

    /// A commit on `parent` holding README.md and the baseline `content`,
    /// authored by `email`.
    fn commit_with_baseline(&self, parent: &str, content: &str, email: &str) -> String {
        let readme = plumb(
            &self.path,
            &["hash-object", "-w", "--stdin"],
            Some(b"assets\n"),
        );
        let blob = plumb(
            &self.path,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        let baselines = plumb(
            &self.path,
            &["mktree"],
            Some(format!("100644 blob {blob}\tqwen3-14b.json\n").as_bytes()),
        );
        let evals = plumb(
            &self.path,
            &["mktree"],
            Some(format!("040000 tree {baselines}\tbaselines\n").as_bytes()),
        );
        let tree = plumb(
            &self.path,
            &["mktree"],
            Some(
                format!("100644 blob {readme}\tREADME.md\n040000 tree {evals}\tevals\n").as_bytes(),
            ),
        );
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&self.path)
            .args(["commit-tree", &tree, "-p", parent, "-m", "synthetic"])
            .env("GIT_AUTHOR_NAME", "x")
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", "x")
            .env("GIT_COMMITTER_EMAIL", email)
            .output()
            .expect("git");
        assert!(out.status.success());
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
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

/// Review HIGH: the job was cancelled (or the server stopped) after the
/// branch moved and before the merge request was opened. The next run
/// finds the system's branch without any merge request and opens it —
/// proposals never stop silently. A person's branch of that name is left
/// alone.
#[tokio::test]
async fn an_interrupted_proposal_is_resumed_not_stuck() {
    let w = world().await;
    let orphan = w.commit_with_baseline(&w.main, "{\"night\":1}\n", "ai-eval@gitstation.invalid");
    plumb(
        &w.path,
        &[
            "update-ref",
            "refs/heads/ai-eval/baseline-qwen3-14b",
            &orphan,
        ],
        None,
    );
    // The same content: only the merge request was missing.
    let resumed = w.propose(w.owner, "{\"night\":1}\n").await;
    let BaselineReport::Opened { iid } = resumed else {
        panic!("not resumed: {resumed:?}");
    };
    assert_eq!(w.tip(), orphan, "nothing new to commit");
    assert_eq!(w.open_candidates().await.len(), 1);
    // The next night updates it as usual.
    assert_eq!(
        w.propose(w.owner, "{\"night\":2}\n").await,
        BaselineReport::Updated { iid }
    );

    let w2 = world().await;
    let persons = w2.commit_with_baseline(&w2.main, "{}\n", "someone@example.invalid");
    plumb(
        &w2.path,
        &[
            "update-ref",
            "refs/heads/ai-eval/baseline-qwen3-14b",
            &persons,
        ],
        None,
    );
    assert_eq!(
        w2.propose(w2.owner, "{\"night\":1}\n").await,
        BaselineReport::Skipped("branch_exists")
    );
    assert!(w2.open_candidates().await.is_empty());
    w.t.drop_now().await;
    w2.t.drop_now().await;
}

/// Review MEDIUM: a project that did not opt in (ai_assets.enabled off)
/// gets nothing.
#[tokio::test]
async fn a_project_without_ai_assets_gets_no_candidate() {
    let w = world().await;
    let _off = forge_db::ai_assets::set_enabled_decided(
        &w.t.db,
        &system("req-ai-off"),
        w.project,
        w.owner,
        false,
        |_| Ok(()),
    )
    .await
    .expect("off");
    assert_eq!(
        w.propose(w.owner, "{\"night\":1}\n").await,
        BaselineReport::Skipped("ai_assets_disabled")
    );
    assert!(w.open_candidates().await.is_empty());
    w.t.drop_now().await;
}

/// Review MEDIUM: at most MAX_OPEN_CANDIDATES open candidates; another
/// models name is skipped, the open ones are still updated.
#[tokio::test]
async fn open_candidates_are_capped_per_project() {
    let w = world().await;
    for i in 0..forge_review::ai_baseline::MAX_OPEN_CANDIDATES {
        let mut ask = w.ask(w.owner, "{\"night\":1}\n");
        ask.models = format!("m{i}");
        let r = w
            .proposer
            .propose(&ask, &RequestId::parse("req-cap").expect("id"))
            .await
            .expect("propose");
        assert!(matches!(r, BaselineReport::Opened { .. }), "{r:?}");
    }
    assert_eq!(
        w.propose(w.owner, "{\"night\":1}\n").await,
        BaselineReport::Skipped("too_many_candidates")
    );
    let mut first = w.ask(w.owner, "{\"night\":2}\n");
    first.models = "m0".to_owned();
    let r = w
        .proposer
        .propose(&first, &RequestId::parse("req-cap").expect("id"))
        .await
        .expect("propose");
    assert!(matches!(r, BaselineReport::Updated { .. }), "{r:?}");
    w.t.drop_now().await;
}

/// A merged candidate is followed by a new one on the default tip; an
/// open candidate of another author is left alone; a baseline a person
/// changed after the bench's commit is not overwritten.
#[tokio::test]
async fn merged_foreign_and_moved_baselines() {
    let w = world().await;
    let BaselineReport::Opened { iid } = w.propose(w.owner, "{\"night\":1}\n").await else {
        panic!("not opened");
    };
    // Merged (fast-forward of main to the candidate).
    let tip = w.tip();
    plumb(&w.path, &["update-ref", "refs/heads/main", &tip], None);
    w.audited(format!(
        "UPDATE forge.merge_requests SET state = 'merged', revision = revision + 1 \
         WHERE project_id = {} AND iid = {iid}",
        w.project.get()
    ))
    .await;
    let mut next = w.ask(w.owner, "{\"night\":2}\n");
    next.start = ObjectId::parse(&tip).expect("sha");
    let r = w
        .proposer
        .propose(&next, &RequestId::parse("req-next").expect("id"))
        .await
        .expect("propose");
    let BaselineReport::Opened { iid: second } = r else {
        panic!("not reopened: {r:?}");
    };
    assert_ne!(second, iid);
    assert_eq!(w.file_on_branch(), "{\"night\":2}");

    // Another author's open candidate: not moved.
    let w3 = world().await;
    let theirs = w3.commit_with_baseline(&w3.main, "{}\n", "ai-eval@gitstation.invalid");
    plumb(
        &w3.path,
        &[
            "update-ref",
            "refs/heads/ai-eval/baseline-qwen3-14b",
            &theirs,
        ],
        None,
    );
    w3.audited(format!(
        "INSERT INTO forge.merge_requests (project_id, iid, title, description, author_id, \
         source_branch, target_branch) VALUES ({}, 99, 'theirs', '', {}, \
         'refs/heads/ai-eval/baseline-qwen3-14b', 'refs/heads/main')",
        w3.project.get(),
        w3.reporter.get()
    ))
    .await;
    assert_eq!(
        w3.propose(w3.owner, "{\"night\":3}\n").await,
        BaselineReport::Skipped("branch_exists")
    );
    assert_eq!(w3.tip(), theirs);
    w3.t.drop_now().await;

    // A person changed the baseline on main after the bench's commit.
    let w2 = world().await;
    let changed = w2.commit_with_baseline(&w2.main, "{\"by\":\"hand\"}\n", "p@example.invalid");
    plumb(&w2.path, &["update-ref", "refs/heads/main", &changed], None);
    assert_eq!(
        w2.propose(w2.owner, "{\"night\":1}\n").await,
        BaselineReport::Skipped("baseline_moved")
    );
    w.t.drop_now().await;
    w2.t.drop_now().await;
}
