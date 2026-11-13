#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The merge queue (M4 C8, ADR-0030) against a real bare repository and the
//! development database: "merge when ready" (`Review::enqueue`), the worker
//! (`MergeQueue::tick`) building trains, the bisection of a red batch, the
//! landing (one compare-and-swap to the tested commit, the normal merge
//! record), a target moved by someone else, the pause, the entries that
//! run alone (a migration, a `forge/risk` stop), two workers and a worker
//! that died mid-landing. The train's pipeline is the test's: it writes the
//! `merge_queue` pipeline row the runner would have finished.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use forge_auth::{prove, Action, MergeRequestCtx, Principal, ProjectCtx, RefProtection};
use forge_core::merge_queue::{BatchState, EntryState, Isolation};
use forge_core::{
    CheckConclusion, CheckName, CheckStatus, MergeRequestId, MergeState, ObjectId, ProjectId,
    RequestId, Role, UserId,
};
use forge_db::repositories;
use forge_db::review::merge_queue as qdb;
use forge_db::review::{merges, request_check, MergeabilityRequest, RefreshRequest};
use forge_db::test_support::FreshDb;
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::approvals::ReviewGit;
use forge_review::{
    check, refresh, CredentialRef, MergeQueue, Mergeability, Merger, NewFields, QueueAsk,
    QueueLimits, Review, StoredMergeRequest, StoredQueueEntry, VersionRefs,
};

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

/// A commit whose tree holds `files` (paths may have one directory level)
/// on top of `parent`.
fn commit(repo: &Path, files: &[(&str, &str)], parent: Option<&str>) -> String {
    let mut top: Vec<String> = Vec::new();
    let mut dirs: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    for (name, content) in files {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        match name.rsplit_once('/') {
            Some((dir, file)) => dirs
                .entry(dir.to_owned())
                .or_default()
                .push((file.to_owned(), blob)),
            None => top.push(format!("100644 blob {blob}\t{name}")),
        }
    }
    for (dir, entries) in dirs {
        // One level per call (enough for these tests: db/migrations is
        // built as db -> migrations).
        let mut listing = String::new();
        let mut sorted = entries;
        sorted.sort();
        for (file, blob) in sorted {
            listing.push_str(&format!("100644 blob {blob}\t{file}\n"));
        }
        let mut tree = plumb(repo, &["mktree"], Some(listing.as_bytes()));
        let parts: Vec<&str> = dir.split('/').collect();
        for part in parts.iter().skip(1).rev() {
            let wrapped = format!("040000 tree {tree}\t{part}\n");
            tree = plumb(repo, &["mktree"], Some(wrapped.as_bytes()));
        }
        top.push(format!("040000 tree {tree}\t{}", parts[0]));
    }
    top.sort_by(|a, b| a.split('\t').nth(1).cmp(&b.split('\t').nth(1)));
    let listing: String = top.iter().map(|l| format!("{l}\n")).collect();
    let tree = plumb(repo, &["mktree"], Some(listing.as_bytes()));
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

fn oid(raw: &str) -> ObjectId {
    ObjectId::parse(raw).expect("oid")
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    path: PathBuf,
    git: Git,
    storage: StorageRoot,
    browser: Browser,
    review: Review,
    merger: Merger,
    owner: UserId,
    dev: UserId,
    project: ProjectId,
    repo: forge_core::RepoId,
    base: String,
    session: i64,
}

/// `acme/web` with `main` at a base commit; olga owns it, dave is a
/// Developer.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let dev = user(&t, "dave").await;
    let web = project(&t, owner, &[(dev, Role::Developer)]).await;
    let _marked = repositories::mark_ready(&t.db, &system("req-ready"), web.repository_id)
        .await
        .expect("ready");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let base = commit(&path, &[("README.md", "base\n")], None);
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let review = Review::new(t.db.clone(), Arc::new(browser.clone()))
        .with_review_git(ReviewGit::new(git.clone(), storage.clone()));
    let merger =
        Merger::new(t.db.clone(), git.clone(), storage.clone()).with_browser(browser.clone());
    let session = forge_db::sessions::issue(
        &t.db,
        &system("req-session"),
        &forge_db::sessions::NewSession {
            user: dev,
            idle: Duration::from_secs(3600),
            absolute: Duration::from_secs(8 * 3600),
            oidc: None,
        },
    )
    .await
    .expect("session")
    .session_id;
    World {
        t,
        _home: home,
        _root: root,
        path,
        git,
        storage,
        browser,
        review,
        merger,
        owner,
        dev,
        project: web.project_id,
        repo: web.repository_id,
        base,
        session,
    }
}

impl World {
    /// The queue's worker; `poll` zero so a waiting queue is due at once.
    fn worker(&self, name: &str) -> MergeQueue {
        MergeQueue::new(self.merger.clone(), name).with_limits(QueueLimits {
            poll: Duration::ZERO,
            ..QueueLimits::default()
        })
    }

    fn tip(&self, branch: &str) -> String {
        plumb(
            &self.path,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
            None,
        )
    }

    /// Whether `file` is in `main`'s tree.
    fn main_has(&self, file: &str) -> bool {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&self.path)
            .args(["cat-file", "-e", &format!("refs/heads/main:{file}")])
            .output()
            .expect("cat-file");
        out.status.success()
    }

    /// Branch `name` one commit ahead of the base with `files`, and its
    /// merge request (checked as the worker does), by dave.
    async fn mr(&mut self, name: &str, files: &[(&str, &str)]) -> StoredMergeRequest {
        let mut all = vec![("README.md", "base\n")];
        all.extend_from_slice(files);
        let head = commit(&self.path, &all, Some(&self.base));
        plumb(
            &self.path,
            &["update-ref", &format!("refs/heads/{name}"), &head],
            None,
        );
        self.browser.invalidate(self.repo);
        let proof = open_proof(
            &self.t,
            &session(self.dev),
            self.project,
            (name, "main"),
            false,
        )
        .await
        .expect("open proof");
        let mr = self
            .review
            .open(
                &request(&format!("req-open-{name}")),
                &proof,
                ready(self.repo),
                &NewFields {
                    title: format!("Change {name}"),
                    description: String::new(),
                },
            )
            .await
            .expect("open");
        self.checked(mr.id).await;
        self.review
            .merge_request(self.project, mr.iid)
            .await
            .expect("read")
            .expect("mr")
    }

    async fn checked(&self, mr: MergeRequestId) {
        let refs = VersionRefs::new(self.git.clone(), self.storage.clone());
        refresh(
            &self.t.db,
            &refs,
            RefreshRequest {
                merge_request: mr,
                pushed_by: None,
            },
            &RequestId::parse("req-refresh").expect("id"),
        )
        .await
        .expect("version");
        let seq = request_check(&self.t.db, mr, RequestId::parse("req-q").expect("id"))
            .await
            .expect("request")
            .expect("open");
        let checker = Mergeability::new(self.git.clone(), self.storage.clone());
        check(
            &self.t.db,
            &checker,
            MergeabilityRequest {
                merge_request: mr,
                check_seq: seq,
            },
            &RequestId::parse("req-check").expect("id"),
        )
        .await
        .expect("check");
        self.browser.invalidate(self.repo);
    }

    /// The route's proof of `action` on `mr` for `who`.
    async fn mr_proof(
        &self,
        who: &Principal,
        action: Action,
        mr: &StoredMergeRequest,
    ) -> Result<forge_auth::Authorized<MergeRequestCtx>, forge_errors::ForgeError> {
        let project = loaded(&self.t, who, self.project).await;
        let rules = forge_db::protected_refs::rules_for(&self.t.db, self.project)
            .await
            .expect("rules");
        let protection = RefProtection::effective(rules.levels(&mr.target_branch));
        prove(
            who,
            action,
            MergeRequestCtx::merging(project, forge_review::loaded_merge_request(mr), protection),
        )
    }

    /// dave puts `mr` in the queue.
    async fn enqueue(
        &self,
        mr: &StoredMergeRequest,
    ) -> Result<StoredQueueEntry, forge_errors::ForgeError> {
        let who = session(self.dev);
        let proof = self.mr_proof(&who, Action::MergeQueueEnqueue, mr).await?;
        self.review
            .enqueue(
                &request(&format!("req-enqueue-{}", mr.iid)),
                CredentialRef::Session(self.session),
                &proof,
                ready(self.repo),
                &QueueAsk {
                    sha: oid(mr.head_sha.as_deref().expect("head")),
                    method: None,
                },
            )
            .await
    }

    async fn queue_id(&self) -> i64 {
        let view = self
            .review
            .merge_queue(self.project, &branch("main"), 50)
            .await
            .expect("view");
        view.queue.expect("a queue").id
    }

    /// Write the train pipeline of the live batch (as the runner would
    /// finish it): red when the batch holds one of `bad`.
    async fn finish_pipeline(&self, queue: i64, bad: &BTreeSet<i64>) -> Option<(i64, bool)> {
        let batch = qdb::live_batch(&self.t.db, queue).await.expect("batch")?;
        if batch.state != BatchState::Testing {
            return None;
        }
        let found = self
            .t
            .json_rows(&format!(
                "SELECT count(*) AS n FROM forge.ci_pipelines WHERE sha = '{}' \
                 AND source = 'merge_queue'",
                batch.train_sha.as_str()
            ))
            .await;
        if found[0]["n"].as_i64() != Some(0) {
            return None;
        }
        let entries = qdb::batch_entries(&self.t.db, batch.id)
            .await
            .expect("entries");
        let red = entries.iter().any(|e| bad.contains(&e.iid));
        self.pipeline(&batch.train_sha, if red { "failed" } else { "succeeded" })
            .await;
        Some((batch.id, red))
    }

    async fn pipeline(&self, sha: &ObjectId, status: &str) {
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, copy_mark, \
                 status, request_id) VALUES ({}, 'refs/heads/main', '{}', 'merge_queue', \
                 'open', '{status}', 'req-test-pipeline')",
                self.project.get(),
                sha.as_str()
            ))
            .await;
    }

    /// Tick until no queue is due; the pipelines finish as `bad` says.
    /// Returns the size of every batch built, in order.
    async fn drive(&self, worker: &MergeQueue, bad: &BTreeSet<i64>) -> Vec<usize> {
        let queue = self.queue_id().await;
        let mut sizes = Vec::new();
        for _ in 0..80 {
            let report = worker.tick().await.expect("tick");
            if let Some(r) = &report {
                if r.outcome == "built" {
                    let b = qdb::batch(&self.t.db, r.batch_id.expect("id"))
                        .await
                        .expect("read")
                        .expect("batch");
                    sizes.push(b.size);
                }
            }
            self.finish_pipeline(queue, bad).await;
            if report.is_none() {
                return sizes;
            }
        }
        panic!("the queue never settled: {sizes:?}");
    }

    async fn entry_of(&self, mr: &StoredMergeRequest) -> StoredQueueEntry {
        self.review
            .merge_queue_entry(mr.id)
            .await
            .expect("read")
            .expect("an entry")
            .0
    }

    async fn state_of(&self, mr: &StoredMergeRequest) -> forge_core::MergeRequestState {
        self.review
            .merge_request(self.project, mr.iid)
            .await
            .expect("read")
            .expect("mr")
            .state
    }
}

async fn project_proof(
    w: &World,
    who: &Principal,
    action: Action,
) -> forge_auth::Authorized<ProjectCtx> {
    let project = loaded(&w.t, who, w.project).await;
    prove(who, action, ProjectCtx::new(project)).expect("allowed")
}

async fn four(w: &mut World) -> Vec<StoredMergeRequest> {
    let mut mrs = Vec::new();
    for name in ["fa", "fb", "fc", "fd"] {
        let mr = w.mr(name, &[(&format!("{name}.txt"), "x\n")]).await;
        mrs.push(mr);
    }
    for mr in &mrs {
        w.enqueue(mr).await.expect("enqueue");
    }
    mrs
}

/// Every merge request of `mrs` is merged exactly when it is not in `bad`;
/// the bad ones were removed with FG-CI-0023.
async fn assert_outcome(w: &World, mrs: &[StoredMergeRequest], bad: &BTreeSet<i64>) {
    for mr in mrs {
        let entry = w.entry_of(mr).await;
        let file = format!(
            "{}.txt",
            mr.source_branch.branch().expect("a branch").to_owned()
        );
        if bad.contains(&mr.iid) {
            assert_eq!(entry.state, EntryState::Removed, "!{}", mr.iid);
            assert_eq!(entry.reason.as_deref(), Some("checks_failed"));
            assert_eq!(
                entry.error_code.as_deref(),
                Some(ErrorCode::MergeQueueChecksFailed.id())
            );
            assert_eq!(w.state_of(mr).await, forge_core::MergeRequestState::Open);
            assert!(!w.main_has(&file), "{file} must not land");
        } else {
            assert_eq!(entry.state, EntryState::Merged, "!{}", mr.iid);
            assert_eq!(w.state_of(mr).await, forge_core::MergeRequestState::Merged);
            let attempt = merges::merge_attempt(&w.t.db, entry.merge_id.expect("merge"))
                .await
                .expect("read")
                .expect("attempt");
            assert_eq!(attempt.state, MergeState::Applied);
            assert!(w.main_has(&file), "{file} must land");
        }
    }
}

#[tokio::test]
async fn a_green_batch_of_four_lands_with_one_compare_and_swap() {
    let mut w = world().await;
    let mrs = four(&mut w).await;
    let worker = w.worker("w1");
    let sizes = w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [4]);
    assert_outcome(&w, &mrs, &BTreeSet::new()).await;
    // The target is the tested train: its first parent chain is the four
    // merge commits on the base.
    let chain = plumb(
        &w.path,
        &["rev-list", "--first-parent", "refs/heads/main"],
        None,
    );
    assert_eq!(chain.lines().count(), 5, "{chain}");
    let batch_train = w
        .t
        .json_rows("SELECT train_sha AS t FROM forge.merge_queue_batches WHERE state = 'landed'")
        .await;
    assert_eq!(batch_train[0]["t"].as_str(), Some(w.tip("main").as_str()));
    // The normal merge record: one merged event per merge request.
    let merged = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.events WHERE event_type = 'merge_request.merged'",
    )
    .await;
    assert_eq!(merged, 4);
    // Review MEDIUM (git): the train ref is deleted once the batch landed.
    let refs = plumb(&w.path, &["for-each-ref", "refs/merge-queue/"], None);
    assert!(refs.is_empty(), "{refs}");
    // Like a direct merge without delete_source_branch: the sources stay.
    for name in ["fa", "fb", "fc", "fd"] {
        assert!(!w.tip(name).is_empty(), "{name} kept");
    }
}

#[tokio::test]
async fn a_testing_train_is_pinned_by_its_hidden_ref() {
    let mut w = world().await;
    let a = w
        .mr(
            "fa",
            &[(
                "fa.txt", "x
",
            )],
        )
        .await;
    w.enqueue(&a).await.expect("enqueue");
    let built = w.worker("w1").tick().await.expect("tick").expect("built");
    let batch = qdb::batch(&w.t.db, built.batch_id.expect("id"))
        .await
        .expect("read")
        .expect("batch");
    let queue = w.queue_id().await;
    let at = plumb(
        &w.path,
        &["rev-parse", &format!("refs/merge-queue/{queue}/train")],
        None,
    );
    assert_eq!(at, batch.train_sha.as_str());
}

#[tokio::test]
async fn one_bad_entry_of_four_is_found_by_bisection() {
    let mut w = world().await;
    let mrs = four(&mut w).await;
    let bad: BTreeSet<i64> = [mrs[2].iid].into();
    let sizes = w.drive(&w.worker("w1"), &bad).await;
    // 4 red → 2 green (lands) → 2 red → 1 red (removed) → 1 green.
    assert_eq!(sizes, [4, 2, 2, 1, 1]);
    assert_outcome(&w, &mrs, &bad).await;
}

#[tokio::test]
async fn two_bad_entries_are_both_found() {
    let mut w = world().await;
    let mrs = four(&mut w).await;
    let bad: BTreeSet<i64> = [mrs[1].iid, mrs[3].iid].into();
    w.drive(&w.worker("w1"), &bad).await;
    assert_outcome(&w, &mrs, &bad).await;
}

#[tokio::test]
async fn all_bad_entries_are_removed_and_the_target_never_moves() {
    let mut w = world().await;
    let mrs = four(&mut w).await;
    let bad: BTreeSet<i64> = mrs.iter().map(|m| m.iid).collect();
    w.drive(&w.worker("w1"), &bad).await;
    assert_outcome(&w, &mrs, &bad).await;
    assert_eq!(w.tip("main"), w.base);
}

#[tokio::test]
async fn a_target_moved_while_testing_rebuilds_the_train() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let worker = w.worker("w1");
    let built = worker.tick().await.expect("tick").expect("a queue");
    assert_eq!(built.outcome, "built");
    // Someone pushes to main while the train is tested.
    let moved = commit(
        &w.path,
        &[("README.md", "base\n"), ("other.txt", "o\n")],
        Some(&w.base),
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &moved], None);
    let queue = w.queue_id().await;
    w.finish_pipeline(queue, &BTreeSet::new()).await;
    let next = worker.tick().await.expect("tick").expect("due");
    assert_eq!(next.outcome, "abandoned");
    let old = qdb::batch(&w.t.db, built.batch_id.expect("id"))
        .await
        .expect("read")
        .expect("batch");
    assert_eq!(old.state, BatchState::Abandoned);
    assert_eq!(old.reason.as_deref(), Some("target_moved"));
    // Rebuilt on the new tip, tested again, landed on top of the push.
    let sizes = w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [1]);
    assert!(w.main_has("fa.txt") && w.main_has("other.txt"));
    assert_eq!(w.entry_of(&a).await.state, EntryState::Merged);
}

#[tokio::test]
async fn a_paused_queue_builds_and_lands_nothing_until_resumed() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let worker = w.worker("w1");
    worker.tick().await.expect("tick").expect("built");
    let queue = w.queue_id().await;
    w.finish_pipeline(queue, &BTreeSet::new()).await;
    // dave (Developer) stops the queue: the green batch does not land.
    let pause = project_proof(&w, &session(w.dev), Action::MergeQueuePause).await;
    let (outcome, stored) = w
        .review
        .set_queue_paused(&request("req-pause"), &pause, &branch("main"), true)
        .await
        .expect("pause");
    assert_eq!(outcome, forge_review::PauseOutcome::Changed);
    assert!(stored.paused);
    let paused = worker.tick().await.expect("tick").expect("due");
    assert_eq!(paused.outcome, "paused");
    assert_eq!(w.tip("main"), w.base);
    // A Developer may not resume (Maintainer); the owner does.
    let denied = prove(
        &session(w.dev),
        Action::MergeQueueManage,
        ProjectCtx::new(loaded(&w.t, &session(w.dev), w.project).await),
    );
    assert!(denied.is_err());
    let resume = project_proof(&w, &session(w.owner), Action::MergeQueueManage).await;
    w.review
        .set_queue_paused(&request("req-resume"), &resume, &branch("main"), false)
        .await
        .expect("resume");
    w.drive(&worker, &BTreeSet::new()).await;
    assert!(w.main_has("fa.txt"));
    let audited = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.audit_log WHERE action IN \
         ('merge_queue.paused', 'merge_queue.resumed', 'merge_queue.enqueued')",
    )
    .await;
    assert_eq!(audited, 3);
}

#[tokio::test]
async fn a_migration_runs_alone_and_the_batch_stops_before_it() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let mig = w
        .mr(
            "fm",
            &[("db/migrations/0001_add.sql", "CREATE TABLE t ();\n")],
        )
        .await;
    let b = w.mr("fb", &[("fb.txt", "x\n")]).await;
    for mr in [&a, &mig, &b] {
        w.enqueue(mr).await.expect("enqueue");
    }
    let sizes = w.drive(&w.worker("w1"), &BTreeSet::new()).await;
    assert_eq!(sizes, [1, 1, 1]);
    assert_eq!(w.entry_of(&mig).await.isolate, Some(Isolation::Migration));
    assert_eq!(w.entry_of(&a).await.isolate, None);
    for mr in [&a, &mig, &b] {
        assert_eq!(w.entry_of(mr).await.state, EntryState::Merged);
    }
}

#[tokio::test]
async fn a_risk_stop_entry_is_never_batched_with_others() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let stop = w.mr("fs", &[("fs.txt", "x\n")]).await;
    let b = w.mr("fb", &[("fb.txt", "x\n")]).await;
    // Through forge-db's test helper: only forge-checks calls the check
    // commands (forge-checks tests/decided_callers.rs).
    forge_db::test_support::record_forge_check(
        &w.t.db,
        w.project,
        &oid(stop.head_sha.as_deref().expect("head")),
        &forge_db::checks::NewCheckRun {
            name: CheckName::parse(forge_core::checks::RISK_CHECK).expect("name"),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            summary: "1 stop".to_owned(),
            details_url: None,
            annotations: Vec::new(),
        },
    )
    .await;
    for mr in [&a, &stop, &b] {
        w.enqueue(mr).await.expect("enqueue");
    }
    let sizes = w.drive(&w.worker("w1"), &BTreeSet::new()).await;
    assert_eq!(sizes, [1, 1, 1], "the stop entry splits the queue");
    assert_eq!(w.entry_of(&stop).await.isolate, Some(Isolation::RiskStop));
}

#[tokio::test]
async fn a_merge_request_already_in_the_queue_is_refused() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let again = w.enqueue(&a).await.expect_err("refused");
    assert_eq!(again.code(), ErrorCode::MergeQueueEntryRefused);
    assert!(again
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "already_queued"));
    // Taking it out, then nothing to take out.
    let who = session(w.dev);
    let proof = w
        .mr_proof(&who, Action::MergeQueueDequeue, &a)
        .await
        .expect("proof");
    let removed = w
        .review
        .dequeue(&request("req-deq"), &proof)
        .await
        .expect("dequeue");
    assert_eq!(removed.state, EntryState::Removed);
    let none = w
        .review
        .dequeue(&request("req-deq2"), &proof)
        .await
        .expect_err("none");
    assert_eq!(none.code(), ErrorCode::MergeQueueEntryRefused);
    // An agent's token is refused before anything (as a merge).
    let agent = forge_auth::Principal::authenticated(
        w.dev,
        false,
        true,
        forge_auth::Credential::Token(vec![forge_core::Scope::MergeExecute]),
        forge_auth::AccountKind::Human,
    )
    .as_agent(true);
    let denied = w.mr_proof(&agent, Action::MergeQueueEnqueue, &a).await;
    assert_eq!(denied.expect_err("agent").code(), ErrorCode::Forbidden);
}

#[tokio::test]
async fn a_batch_past_its_deadline_is_reaped_and_requeued() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let worker = w.worker("w1");
    for round in 1..=3 {
        let built = worker.tick().await.expect("tick").expect("due");
        assert_eq!(built.outcome, "built", "round {round}");
        w.t.exec_superuser(
            "UPDATE forge.merge_queue_batches SET deadline_at = now() - interval '1 second' \
             WHERE state = 'testing'",
        )
        .await;
        let reaped = worker.tick().await.expect("tick").expect("due");
        assert_eq!(reaped.outcome, "reaped", "round {round}");
        let batch = qdb::batch(&w.t.db, built.batch_id.expect("id"))
            .await
            .expect("read")
            .expect("batch");
        assert_eq!(batch.state, BatchState::Abandoned);
        assert_eq!(
            batch.error_code.as_deref(),
            Some(ErrorCode::MergeQueueStalled.id())
        );
    }
    // The third reap removes the entry for good.
    let entry = w.entry_of(&a).await;
    assert_eq!(entry.state, EntryState::Removed);
    assert_eq!(entry.reason.as_deref(), Some("stalled"));
    assert_eq!(
        entry.error_code.as_deref(),
        Some(ErrorCode::MergeQueueStalled.id())
    );
    assert_eq!(w.tip("main"), w.base);
}

#[tokio::test]
async fn two_workers_never_hold_one_queue() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let (w1, w2) = (w.worker("w1"), w.worker("w2"));
    let (r1, r2) = tokio::join!(w1.tick(), w2.tick());
    let worked = [r1.expect("w1"), r2.expect("w2")]
        .into_iter()
        .flatten()
        .count();
    assert_eq!(worked, 1, "exactly one worker claimed the queue");
    // A lease held by another worker is never taken before it expires.
    let held = qdb::claim(&w.t.db, "other", Duration::from_secs(600))
        .await
        .expect("claim");
    let queue = w.queue_id().await;
    if held.is_none() {
        // Not due (just released): make it due, then hold it.
        w.t.exec_superuser("UPDATE forge.merge_queues SET next_run_at = now()")
            .await;
        qdb::claim(&w.t.db, "other", Duration::from_secs(600))
            .await
            .expect("claim")
            .expect("held");
    }
    assert!(w1.tick().await.expect("tick").is_none());
    // An expired lease is taken over (a worker that died).
    w.t.exec_superuser(&format!(
        "UPDATE forge.merge_queues SET lease_until = now() - interval '1 second' WHERE id = {queue}"
    ))
    .await;
    assert!(w2.tick().await.expect("tick").is_some());
}

#[tokio::test]
async fn a_worker_that_died_mid_landing_is_recovered() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let b = w.mr("fb", &[("fb.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    w.enqueue(&b).await.expect("enqueue");
    let dying = w.worker("dying").with_crash_after_cas();
    let built = dying.tick().await.expect("tick").expect("built");
    let queue = w.queue_id().await;
    w.finish_pipeline(queue, &BTreeSet::new()).await;
    let crashed = dying
        .tick()
        .await
        .expect_err("the landing stops after the swap");
    assert_eq!(crashed.code(), ErrorCode::Internal);
    let batch = qdb::batch(&w.t.db, built.batch_id.expect("id"))
        .await
        .expect("read")
        .expect("batch");
    assert_eq!(batch.state, BatchState::Landing);
    assert_eq!(w.tip("main"), batch.train_sha.as_str(), "the swap happened");
    for mr in [&a, &b] {
        let entry = w.entry_of(mr).await;
        let attempt = merges::merge_attempt(&w.t.db, entry.merge_id.expect("written"))
            .await
            .expect("read")
            .expect("attempt");
        assert_eq!(attempt.state, MergeState::Executing);
    }
    // A fresh worker (the restart) records what is left.
    let restarted = w.worker("restarted");
    let recovered = restarted.tick().await.expect("tick").expect("due");
    assert_eq!(recovered.outcome, "recovered");
    for mr in [&a, &b] {
        assert_eq!(w.entry_of(mr).await.state, EntryState::Merged);
        assert_eq!(w.state_of(mr).await, forge_core::MergeRequestState::Merged);
    }
    let landed = qdb::batch(&w.t.db, batch.id)
        .await
        .expect("read")
        .expect("batch");
    assert_eq!(landed.state, BatchState::Landed);
}

#[tokio::test]
async fn a_landing_interrupted_before_the_swap_rebuilds() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let worker = w.worker("w1");
    let built = worker.tick().await.expect("tick").expect("built");
    // A worker that stopped right after `testing → landing`.
    assert!(qdb::begin_landing(&w.t.db, built.batch_id.expect("id"))
        .await
        .expect("landing"));
    let recovered = worker.tick().await.expect("tick").expect("due");
    assert_eq!(recovered.outcome, "recovered");
    let old = qdb::batch(&w.t.db, built.batch_id.expect("id"))
        .await
        .expect("read")
        .expect("batch");
    assert_eq!(old.reason.as_deref(), Some("landing_interrupted"));
    assert_eq!(w.tip("main"), w.base);
    w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(w.entry_of(&a).await.state, EntryState::Merged);
}

#[tokio::test]
async fn a_new_version_pushed_while_queued_leaves_the_queue() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    // A new version of the merge request before its batch is built.
    let newer = commit(
        &w.path,
        &[("README.md", "base\n"), ("fa.txt", "y\n")],
        Some(&w.base),
    );
    plumb(&w.path, &["update-ref", "refs/heads/fa", &newer], None);
    w.checked(a.id).await;
    let report = w.worker("w1").tick().await.expect("tick").expect("due");
    assert_eq!(report.outcome, "removed");
    let entry = w.entry_of(&a).await;
    assert_eq!(entry.state, EntryState::Removed);
    assert_eq!(entry.reason.as_deref(), Some("head_changed"));
    assert_eq!(
        entry.error_code.as_deref(),
        Some(ErrorCode::HeadChanged.id())
    );
}

#[tokio::test]
async fn the_queue_view_lists_entries_in_order() {
    let mut w = world().await;
    let mrs = four(&mut w).await;
    let view = w
        .review
        .merge_queue(w.project, &branch("main"), 20)
        .await
        .expect("view");
    let iids: Vec<i64> = view.live.iter().map(|e| e.iid).collect();
    assert_eq!(iids, mrs.iter().map(|m| m.iid).collect::<Vec<_>>());
    let (entry, position) = w
        .review
        .merge_queue_entry(mrs[2].id)
        .await
        .expect("read")
        .expect("entry");
    assert_eq!(entry.state, EntryState::Queued);
    assert_eq!(position, Some(3));
    assert!(view.batch.is_none());
    let _ = &w.owner;
}

/// Review HIGH 1 / HIGH 2: an entry whose batch is landing cannot be
/// dequeued, a worker that only knows the batch as `testing` cannot end
/// it, and the recovery records every entry the swap landed.
#[tokio::test]
async fn a_landing_batch_is_neither_dequeued_nor_ended_by_a_stale_worker() {
    let mut w = world().await;
    let a = w
        .mr(
            "fa",
            &[(
                "fa.txt", "x
",
            )],
        )
        .await;
    let b = w
        .mr(
            "fb",
            &[(
                "fb.txt", "x
",
            )],
        )
        .await;
    w.enqueue(&a).await.expect("enqueue");
    w.enqueue(&b).await.expect("enqueue");
    let dying = w.worker("dying").with_crash_after_cas();
    let built = dying.tick().await.expect("tick").expect("built");
    let queue = w.queue_id().await;
    w.finish_pipeline(queue, &BTreeSet::new()).await;
    dying.tick().await.expect_err("stops after the swap");
    let batch_id = built.batch_id.expect("id");
    // A person cannot take an entry out of a landing batch.
    let proof = w
        .mr_proof(&session(w.dev), Action::MergeQueueDequeue, &b)
        .await
        .expect("proof");
    let refused = w
        .review
        .dequeue(&request("req-deq-landing"), &proof)
        .await
        .expect_err("landing");
    assert_eq!(refused.code(), ErrorCode::MergeQueueEntryRefused);
    assert!(refused
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "landing"));
    // A stale worker's poll (`testing`) cannot end it.
    let stale = qdb::BatchEnd {
        from: BatchState::Testing,
        state: BatchState::Abandoned,
        reason: "target_moved",
        error_code: None,
        removed: Vec::new(),
        reaped: false,
        batch_limit: None,
    };
    assert!(!qdb::end_batch(&w.t.db, batch_id, &stale)
        .await
        .expect("end"));
    // The recovery records both.
    let recovered = w
        .worker("restarted")
        .tick()
        .await
        .expect("tick")
        .expect("due");
    assert_eq!(recovered.outcome, "recovered");
    for mr in [&a, &b] {
        assert_eq!(w.entry_of(mr).await.state, EntryState::Merged);
        assert_eq!(w.state_of(mr).await, forge_core::MergeRequestState::Merged);
    }
}

/// Review MEDIUM (git): the landing's update-ref runs without hooks, so the
/// target's protection is decided again per entry: a target protected
/// above the enqueuer's role after the enqueue refuses the landing.
#[tokio::test]
async fn a_protected_target_refuses_a_landing_below_its_role() {
    let mut w = world().await;
    let a = w
        .mr(
            "fa",
            &[(
                "fa.txt", "x
",
            )],
        )
        .await;
    w.enqueue(&a).await.expect("enqueue");
    forge_db::protected_refs::create(
        &w.t.db,
        &system("req-protect"),
        w.project,
        &forge_core::RefPattern::parse("refs/heads/main").expect("pattern"),
        Role::Maintainer,
    )
    .await
    .expect("protect");
    w.drive(&w.worker("w1"), &BTreeSet::new()).await;
    let entry = w.entry_of(&a).await;
    assert_eq!(entry.state, EntryState::Removed);
    assert_eq!(entry.error_code.as_deref(), Some(ErrorCode::Forbidden.id()));
    assert_eq!(w.tip("main"), w.base, "the target never moved");
}

/// Review LOW: a merge request in the queue is not merged by hand.
#[tokio::test]
async fn a_queued_merge_request_cannot_be_merged_by_hand() {
    let mut w = world().await;
    let a = w
        .mr(
            "fa",
            &[(
                "fa.txt", "x
",
            )],
        )
        .await;
    w.enqueue(&a).await.expect("enqueue");
    let proof = w
        .mr_proof(&session(w.dev), Action::MergeRequestMerge, &a)
        .await
        .expect("proof");
    let refused = w
        .review
        .request_merge(
            &request("req-merge-queued"),
            CredentialRef::Session(w.session),
            &proof,
            ready(w.repo),
            &forge_review::MergeAsk {
                sha: oid(a.head_sha.as_deref().expect("head")),
                method: None,
                delete_source_branch: None,
                message: None,
            },
        )
        .await
        .expect_err("queued");
    assert_eq!(refused.code(), ErrorCode::MergeQueueEntryRefused);
    assert!(refused
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "already_queued"));
}

// ── M4 C9: auto-merge (ADR-0017 Amendment 2026-11-13) ──

impl World {
    /// The auto-merge worker; `recheck` zero so every tick looks again.
    fn auto(&self) -> forge_review::AutoMerger {
        forge_review::AutoMerger::new(self.review.clone(), self.merger.clone()).with_limits(
            forge_review::AutoMergeLimits {
                recheck: Duration::ZERO,
                ..forge_review::AutoMergeLimits::default()
            },
        )
    }

    /// forge's own `forge/risk` run on `mr`'s head.
    async fn risk(&self, mr: &StoredMergeRequest, conclusion: CheckConclusion, summary: &str) {
        forge_db::test_support::record_forge_check(
            &self.t.db,
            self.project,
            &oid(mr.head_sha.as_deref().expect("head")),
            &forge_db::checks::NewCheckRun {
                name: CheckName::parse(forge_core::checks::RISK_CHECK).expect("name"),
                status: CheckStatus::Completed,
                conclusion: Some(conclusion),
                summary: summary.to_owned(),
                details_url: None,
                annotations: Vec::new(),
            },
        )
        .await;
    }

    /// What auto-merge recorded for `mr` (state, class, reason, detail).
    async fn auto_state(
        &self,
        mr: &StoredMergeRequest,
    ) -> Option<(String, Option<String>, String, Option<String>)> {
        self.review
            .auto_merge_state(mr.id)
            .await
            .expect("read")
            .map(|s| {
                (
                    s.state,
                    s.class.map(|c| c.as_str().to_owned()),
                    s.reason,
                    s.detail,
                )
            })
    }

    async fn live_entry(&self, mr: &StoredMergeRequest) -> Option<StoredQueueEntry> {
        self.review
            .merge_queue_entry(mr.id)
            .await
            .expect("read")
            .map(|(e, _)| e)
            .filter(|e| e.state.is_live())
    }
}

/// A project created after C9 has no row: `full`. A mergeable merge
/// request whose forge/risk is clear is queued by the auto-merge principal
/// (credential `auto_merge`, on behalf of its author, audited
/// `merge_queue.auto_enqueued` with actor system) and lands through the
/// queue.
#[tokio::test]
async fn a_full_project_queues_and_lands_a_mergeable_merge_request() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let auto = w.auto();
    // No forge/risk answer yet: it waits, nothing is queued.
    auto.tick().await.expect("tick");
    assert_eq!(
        w.auto_state(&a).await,
        Some(("waiting".to_owned(), None, "risk_pending".to_owned(), None))
    );
    assert!(w.live_entry(&a).await.is_none());
    w.risk(&a, CheckConclusion::Success, "No risk found").await;
    let report = auto.tick().await.expect("tick");
    assert_eq!(report.queued, 1, "{report:?}");
    let entry = w.live_entry(&a).await.expect("queued");
    assert_eq!(entry.credential, CredentialRef::AutoMerge(w.project));
    assert_eq!(entry.enqueued_by, a.author);
    assert_eq!(
        count(
            &w.t,
            "SELECT count(*) AS n FROM forge.audit_log WHERE action = 'merge_queue.auto_enqueued' \
             AND actor_kind = 'system'",
        )
        .await,
        1
    );
    w.drive(&w.worker("w1"), &BTreeSet::new()).await;
    assert_eq!(w.state_of(&a).await, forge_core::MergeRequestState::Merged);
    assert!(w.main_has("fa.txt"));
    let attempt = merges::merge_attempt(&w.t.db, w.entry_of(&a).await.merge_id.expect("merge"))
        .await
        .expect("read")
        .expect("attempt");
    assert_eq!(attempt.credential, CredentialRef::AutoMerge(w.project));
    assert_eq!(attempt.requested_by, a.author);
    // Once merged, auto-merge forgets it.
    auto.tick().await.expect("tick");
    assert_eq!(w.auto_state(&a).await, None);
}

/// FG-REVIEW-0026 against real rows: each stop class of forge/risk keeps
/// the merge request out of the queue, with its class and rule.
#[tokio::test]
async fn every_stop_class_keeps_the_merge_request_out_of_the_queue() {
    let mut w = world().await;
    let cases = [
        ("fd", "sql_drop", "data_loss"),
        ("fl", "secret_file", "leak"),
        ("fp", "agent_config", "developer_pc"),
        ("fm", "mass_deletion_lines", "program_breakage"),
    ];
    let mut mrs = Vec::new();
    for (name, rule, _) in cases {
        let mr = w.mr(name, &[(&format!("{name}.txt"), "x\n")]).await;
        let summary = format!(
            "STOP: 1 stop, 0 warn [FG-REVIEW-0023].\ntarget main (1 changed file(s)):\n- stop {rule} {name}.txt [FG-REVIEW-0023]"
        );
        w.risk(&mr, CheckConclusion::Failure, &summary).await;
        mrs.push(mr);
    }
    // forge/risk errored: the head cannot be shown safe.
    let e = w.mr("fe", &[("fe.txt", "x\n")]).await;
    w.risk(&e, CheckConclusion::Error, "evaluation failed")
        .await;
    let report = w.auto().tick().await.expect("tick");
    assert_eq!(report.stopped, 5, "{report:?}");
    assert_eq!(report.queued, 0);
    for ((_, rule, class), mr) in cases.iter().zip(&mrs) {
        assert_eq!(
            w.auto_state(mr).await,
            Some((
                "stopped".to_owned(),
                Some((*class).to_owned()),
                "risk_stop".to_owned(),
                Some((*rule).to_owned())
            )),
            "{rule}"
        );
        assert!(w.live_entry(mr).await.is_none(), "{rule} never queued");
    }
    let (state, class, _, _) = w.auto_state(&e).await.expect("state");
    assert_eq!(
        (state.as_str(), class.as_deref()),
        ("stopped", Some("not_assessed"))
    );
    let codes = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.auto_merge_states WHERE error_code = 'FG-REVIEW-0026'",
    )
    .await;
    assert_eq!(codes, 5);
}

/// A warning never stops auto-merge: the merge request is queued with the
/// warning noted, and lands.
#[tokio::test]
async fn a_warning_does_not_stop_auto_merge() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.risk(
        &a,
        CheckConclusion::Neutral,
        "WARN: 0 stop, 1 warn [FG-REVIEW-0024].\n- warn dependency_change Cargo.toml",
    )
    .await;
    w.auto().tick().await.expect("tick");
    assert_eq!(
        w.auto_state(&a).await,
        Some((
            "queued".to_owned(),
            None,
            "queued_with_warning".to_owned(),
            None
        ))
    );
    w.drive(&w.worker("w1"), &BTreeSet::new()).await;
    assert_eq!(w.state_of(&a).await, forge_core::MergeRequestState::Merged);
}

/// FG-REVIEW-0027: the project's emergency stop (a Maintainer's, audited)
/// queues nothing and holds the auto-merge entries already queued — they
/// stay queued while a person's entry still lands; resuming lets them
/// land. The instance's stop holds them alike.
#[tokio::test]
async fn the_emergency_stop_pauses_auto_merge_and_its_queue_entries() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let b = w.mr("fb", &[("fb.txt", "x\n")]).await;
    let person = w.mr("fc", &[("fc.txt", "x\n")]).await;
    w.risk(&a, CheckConclusion::Success, "ok").await;
    let auto = w.auto();
    auto.tick().await.expect("tick");
    assert!(w.live_entry(&a).await.is_some(), "a queued by auto-merge");
    // olga (Owner) stops the project's auto-merge.
    let proof = project_proof(&w, &session(w.owner), Action::AutoMergeStopToggle).await;
    let (_, settings) = w
        .review
        .set_auto_merge_stop(&request("req-stop"), &proof, true)
        .await
        .expect("stop");
    assert!(settings.project_stop.is_some());
    assert_eq!(
        count(
            &w.t,
            "SELECT count(*) AS n FROM forge.audit_log WHERE action = 'auto_merge.stopped'",
        )
        .await,
        1
    );
    w.risk(&b, CheckConclusion::Success, "ok").await;
    w.enqueue(&person).await.expect("a person's entry");
    auto.tick().await.expect("tick");
    assert!(w.live_entry(&b).await.is_none(), "nothing new is queued");
    let (state, class, reason, _) = w.auto_state(&b).await.expect("state");
    assert_eq!(
        (state.as_str(), class.as_deref(), reason.as_str()),
        ("stopped", Some("emergency_stop"), "project_stopped")
    );
    // The queue stays due while the held entry waits: tick a bounded
    // number of times (the person's entry lands meanwhile).
    let worker = w.worker("w1");
    let queue = w.queue_id().await;
    for _ in 0..12 {
        worker.tick().await.expect("tick");
        w.finish_pipeline(queue, &BTreeSet::new()).await;
    }
    assert_eq!(
        w.state_of(&person).await,
        forge_core::MergeRequestState::Merged
    );
    assert_eq!(w.state_of(&a).await, forge_core::MergeRequestState::Open);
    assert_eq!(
        w.live_entry(&a).await.map(|e| e.state),
        Some(EntryState::Queued),
        "held, not removed"
    );
    // Resumed: both land.
    let (_, settings) = w
        .review
        .set_auto_merge_stop(&request("req-resume"), &proof, false)
        .await
        .expect("resume");
    assert!(settings.project_stop.is_none());
    // `main` moved (the person's merge): b's mergeability is checked again,
    // as the mergeability job does after a push to the target.
    w.checked(b.id).await;
    auto.tick().await.expect("tick");
    w.drive(&w.worker("w1"), &BTreeSet::new()).await;
    let (ea, eb) = (w.entry_of(&a).await, w.review.merge_queue_entry(b.id).await);
    assert_eq!(
        w.state_of(&a).await,
        forge_core::MergeRequestState::Merged,
        "{ea:?} {:?}",
        w.auto_state(&a).await
    );
    assert_eq!(
        w.state_of(&b).await,
        forge_core::MergeRequestState::Merged,
        "{eb:?} {:?}",
        w.auto_state(&b).await
    );
    // The instance's stop holds a new one too.
    let d = w.mr("fd", &[("fd.txt", "y\n")]).await;
    w.risk(&d, CheckConclusion::Success, "ok").await;
    w.t.exec_superuser("UPDATE forge.auto_merge_instance SET stopped_at = now() WHERE id = 1")
        .await;
    auto.tick().await.expect("tick");
    let (_, class, reason, _) = w.auto_state(&d).await.expect("state");
    assert_eq!(
        (class.as_deref(), reason.as_str()),
        (Some("emergency_stop"), "instance_stopped")
    );
    assert!(w.live_entry(&d).await.is_none());
}

/// A Developer can neither stop auto-merge nor change the mode; the Owner
/// turns it off and auto-merge leaves the project alone (no line).
#[tokio::test]
async fn only_the_owner_changes_the_mode_and_off_means_nothing_automatic() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.risk(&a, CheckConclusion::Success, "ok").await;
    let dev = session(w.dev);
    for action in [Action::AutoMergeStopToggle, Action::AutoMergeSettingsManage] {
        let project = loaded(&w.t, &dev, w.project).await;
        let denied = prove(&dev, action, ProjectCtx::new(project)).expect_err("denied");
        assert_eq!(denied.code(), ErrorCode::Forbidden, "{action:?}");
    }
    let before = w.review.auto_merge_settings(w.project).await.expect("read");
    assert_eq!(before.mode, forge_core::auto_merge::AutoMergeMode::Full);
    assert!(!before.stored, "a new project has no row: full");
    let proof = project_proof(&w, &session(w.owner), Action::AutoMergeSettingsManage).await;
    let (_, after) = w
        .review
        .set_auto_merge_mode(
            &request("req-mode"),
            &proof,
            forge_core::auto_merge::AutoMergeMode::Off,
        )
        .await
        .expect("mode");
    assert_eq!(after.mode, forge_core::auto_merge::AutoMergeMode::Off);
    w.auto().tick().await.expect("tick");
    assert_eq!(w.auto_state(&a).await, None);
    assert!(w.live_entry(&a).await.is_none());
}

/// A person who takes the auto-merge principal's entry out of the queue
/// keeps that head away from auto-merge (`held`).
#[tokio::test]
async fn a_person_who_dequeues_an_auto_merge_entry_holds_that_head() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.risk(&a, CheckConclusion::Success, "ok").await;
    let auto = w.auto();
    auto.tick().await.expect("tick");
    let proof = w
        .mr_proof(&session(w.dev), Action::MergeQueueDequeue, &a)
        .await
        .expect("proof");
    w.review
        .dequeue(&request("req-deq-auto"), &proof)
        .await
        .expect("dequeue");
    auto.tick().await.expect("tick");
    assert_eq!(
        w.auto_state(&a).await,
        Some((
            "held".to_owned(),
            None,
            "dequeued_by_person".to_owned(),
            None
        ))
    );
    assert!(w.live_entry(&a).await.is_none());
}
