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
/// the bad ones were removed with FG-CI-0018.
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
    // The train ref is hidden under refs/merge-queue/.
    let refs = plumb(&w.path, &["for-each-ref", "refs/merge-queue/"], None);
    assert!(refs.contains("/train"), "{refs}");
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
    forge_db::checks::record_forge_check_run(
        &w.t.db,
        &system("req-risk"),
        (w.project, &oid(stop.head_sha.as_deref().expect("head"))),
        &forge_db::checks::NewCheckRun {
            name: CheckName::parse(forge_core::checks::RISK_CHECK).expect("name"),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Failure),
            summary: "1 stop".to_owned(),
            details_url: None,
            annotations: Vec::new(),
        },
    )
    .await
    .expect("risk");
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
