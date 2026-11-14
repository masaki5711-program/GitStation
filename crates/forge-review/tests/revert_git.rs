#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! One-step revert (M4 C10, plan 04 M-4, ADR-0024 Amendment C10) against a
//! real bare repository and the development database: a merge request is
//! merged by the real merge job (merge commit or squash), then reverted by
//! a person (`Reverter::revert`) or drafted by the system after the target
//! branch's pipeline failed (`revert-drafts` subscriber → job →
//! `Reverter::draft_after_failure`).

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use forge_auth::{
    prove, Action, Authorized, MergeRequestCtx, Principal, ProjectCtx, RefProtection,
};
use forge_core::checks::{CheckConclusion, CheckStatus};
use forge_core::{CheckName, MergeMethod, ObjectId, ProjectId, RequestId, Role, UserId};
use forge_db::checks::NewCheckRun;
use forge_db::repositories;
use forge_db::review::reverts::{revert_draft_payload, RevertDrafts, REVERT_DRAFT_JOB};
use forge_db::review::{request_check, MergeabilityRequest, RefreshRequest};
use forge_db::test_support::FreshDb;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::approvals::ReviewGit;
use forge_review::{
    check, refresh, revert_ctx, CredentialRef, DraftReport, MergeAsk, Mergeability, Merger,
    NewFields, Reverter, Review, StoredMergeRequest, VersionRefs,
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

/// A commit whose tree holds `files` on top of `parent`.
fn commit(repo: &Path, files: &[(&str, &str)], parent: Option<&str>) -> String {
    let mut listing = String::new();
    let mut sorted: Vec<&(&str, &str)> = files.iter().collect();
    sorted.sort_by_key(|(name, _)| *name);
    for (name, content) in sorted {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
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

fn reason(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    path: PathBuf,
    browser: Browser,
    git: Git,
    storage: StorageRoot,
    review: Review,
    merger: Merger,
    reverter: Reverter,
    owner: UserId,
    dev: UserId,
    reporter: UserId,
    project: ProjectId,
    repo: forge_core::RepoId,
    mr: StoredMergeRequest,
    base: String,
    head: String,
}

/// `acme/web`: `main` at base (README.md), `feature` adds feature.txt;
/// olga owns it, dave (Developer) opens the merge request, rita is a
/// Reporter.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let dev = user(&t, "dave").await;
    let reporter = user(&t, "rita").await;
    let web = project(
        &t,
        owner,
        &[(dev, Role::Developer), (reporter, Role::Reporter)],
    )
    .await;
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
    let head = commit(
        &path,
        &[("README.md", "base\n"), ("feature.txt", "f\n")],
        Some(&base),
    );
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &head], None);
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let review = Review::new(t.db.clone(), Arc::new(browser.clone()))
        .with_review_git(ReviewGit::new(git.clone(), storage.clone()));
    let merger =
        Merger::new(t.db.clone(), git.clone(), storage.clone()).with_browser(browser.clone());
    let reverter = review
        .reverter(git.clone(), storage.clone())
        .with_browser(browser.clone());
    let proof = open_proof(
        &t,
        &session(dev),
        web.project_id,
        ("feature", "main"),
        false,
    )
    .await
    .expect("open proof");
    let mr = review
        .open(
            &request("req-open"),
            &proof,
            ready(web.repository_id),
            &NewFields {
                title: "Add the feature".to_owned(),
                description: "Synthetic description.".to_owned(),
            },
        )
        .await
        .expect("open");
    World {
        t,
        _home: home,
        _root: root,
        path,
        browser,
        git,
        storage,
        review,
        merger,
        reverter,
        owner,
        dev,
        reporter,
        project: web.project_id,
        repo: web.repository_id,
        mr,
        base,
        head,
    }
}

impl World {
    /// The version and the mergeability check, as the worker writes them.
    async fn checked(&mut self) {
        let refs = VersionRefs::new(self.git.clone(), self.storage.clone());
        refresh(
            &self.t.db,
            &refs,
            RefreshRequest {
                merge_request: self.mr.id,
                pushed_by: None,
            },
            &RequestId::parse("req-refresh").expect("id"),
        )
        .await
        .expect("version");
        let seq = request_check(
            &self.t.db,
            self.mr.id,
            RequestId::parse("req-q").expect("id"),
        )
        .await
        .expect("request")
        .expect("open");
        let checker = Mergeability::new(self.git.clone(), self.storage.clone());
        check(
            &self.t.db,
            &checker,
            MergeabilityRequest {
                merge_request: self.mr.id,
                check_seq: seq,
            },
            &RequestId::parse("req-check").expect("id"),
        )
        .await
        .expect("check");
        self.browser.invalidate(self.repo);
    }

    /// olga merges dave's merge request with `method` (the real job); the
    /// new tip of main.
    async fn merged(&mut self, method: MergeMethod) -> String {
        self.checked().await;
        let who = session(self.owner);
        let project = loaded(&self.t, &who, self.project).await;
        let mr = self
            .review
            .merge_request(self.project, self.mr.iid)
            .await
            .expect("read")
            .expect("mr");
        let rules = forge_db::protected_refs::rules_for(&self.t.db, self.project)
            .await
            .expect("rules");
        let protection = RefProtection::effective(rules.levels(&mr.target_branch));
        let proof = prove(
            &who,
            Action::MergeRequestMerge,
            MergeRequestCtx::merging(project, forge_review::loaded_merge_request(&mr), protection),
        )
        .expect("merge proof");
        let sid = forge_db::sessions::issue(
            &self.t.db,
            &system("req-session"),
            &forge_db::sessions::NewSession {
                user: self.owner,
                idle: Duration::from_secs(3600),
                absolute: Duration::from_secs(8 * 3600),
                oidc: None,
            },
        )
        .await
        .expect("session")
        .session_id;
        let attempt = self
            .review
            .request_merge(
                &request("req-merge"),
                CredentialRef::Session(sid),
                &proof,
                ready(self.repo),
                &MergeAsk {
                    sha: oid(&self.head),
                    method: Some(method),
                    delete_source_branch: Some(false),
                    message: None,
                },
            )
            .await
            .expect("queued");
        let report = self.merger.execute(attempt.id).await.expect("merge");
        assert_eq!(report.outcome, "applied");
        self.mr = self
            .review
            .merge_request(self.project, self.mr.iid)
            .await
            .expect("read")
            .expect("mr");
        self.tip("main").expect("main")
    }

    fn tip(&self, branch: &str) -> Option<String> {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&self.path)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .output()
            .expect("git");
        out.status.success().then(|| {
            String::from_utf8(out.stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        })
    }

    /// The route's two proofs of `who`'s revert of the merge request.
    async fn proofs(
        &self,
        who: &Principal,
    ) -> Result<(Authorized<MergeRequestCtx>, Authorized<ProjectCtx>), ForgeError> {
        let project = loaded(&self.t, who, self.project).await;
        let mr = self
            .review
            .merge_request(self.project, self.mr.iid)
            .await
            .expect("read")
            .expect("mr");
        let author = who.user().expect("signed in");
        let create = prove(
            who,
            Action::MergeRequestCreate,
            revert_ctx(project, (mr.iid, &mr.target_branch), author, false)?,
        )?;
        let push = prove(who, Action::RepoPush, ProjectCtx::new(project))?;
        Ok((create, push))
    }

    async fn revert_as(
        &self,
        user: UserId,
        id: &str,
    ) -> Result<forge_review::RevertOpened, ForgeError> {
        let who = session(user);
        let (create, push) = self.proofs(&who).await?;
        self.reverter
            .revert(&request(id), &create, &push, ready(self.repo), self.mr.iid)
            .await
    }

    async fn events(&self, kind: &str) -> Vec<serde_json::Value> {
        self.t
            .json_rows(&format!(
                "SELECT event_type, dedup_key, aggregate_id, actor_kind, payload \
                 FROM forge.events WHERE event_type = '{kind}' ORDER BY id"
            ))
            .await
    }

    async fn merge_request_count(&self) -> i64 {
        count(&self.t, "SELECT count(*) AS n FROM forge.merge_requests").await
    }

    /// A push pipeline of `branch` at `sha` that ended `status`.
    async fn push_pipeline(&self, branch: &str, sha: &str, status: &str) {
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, status, \
                   request_id, protected, copy_mark) \
                 VALUES ({}, 'refs/heads/{branch}', '{sha}', 'push', '{status}', 'req-pipe', \
                   true, 'open')",
                self.project.get()
            ))
            .await;
    }

    /// forge's `ci/pipeline` on `sha` concluding `conclusion` (the worker's
    /// write), then the `revert-drafts` subscriber over every event.
    async fn ci_pipeline_concluded(&self, sha: &str, conclusion: CheckConclusion) {
        let run = NewCheckRun {
            name: CheckName::parse("ci/pipeline").expect("name"),
            status: CheckStatus::Completed,
            conclusion: Some(conclusion),
            summary: "synthetic".to_owned(),
            details_url: None,
            annotations: Vec::new(),
        };
        forge_db::checks::record_forge_check_run(
            &self.t.db,
            &system("req-ci-check"),
            (self.project, &oid(sha)),
            &run,
        )
        .await
        .expect("check");
        // The worker registers its subscribers at start; here, once.
        forge_db::subscribers::register(
            &self.t.db,
            &RevertDrafts,
            forge_db::subscribers::Start::Beginning,
        )
        .await
        .expect("register");
        forge_db::sequencer::run_once(
            &self.t.db,
            forge_db::sequencer::BatchSize::MAX,
            forge_db::sequencer::AuditSequencing::Skip,
        )
        .await
        .expect("sequencer");
        loop {
            match forge_db::subscribers::run_once(&self.t.db, &RevertDrafts)
                .await
                .expect("step")
            {
                forge_db::subscribers::Progress::Handled { .. } => {}
                forge_db::subscribers::Progress::CaughtUp => break,
                other @ (forge_db::subscribers::Progress::Busy
                | forge_db::subscribers::Progress::Disabled
                | forge_db::subscribers::Progress::Failed(_)
                | forge_db::subscribers::Progress::Stuck(_)
                | forge_db::subscribers::Progress::Quarantined(_)) => {
                    panic!("unexpected {other:?}")
                }
            }
        }
    }

    /// The `merge_request.revert_draft` jobs' payloads.
    async fn draft_jobs(&self) -> Vec<serde_json::Value> {
        self.t
            .json_rows(&format!(
                "SELECT payload FROM forge.jobs WHERE kind = '{REVERT_DRAFT_JOB}' ORDER BY id"
            ))
            .await
            .into_iter()
            .map(|r| r["payload"].clone())
            .collect()
    }
}

/// `git revert -m 1` of a merge commit, in one action: the branch
/// `revert/mr-1` holds one commit on main's tip whose tree is main before
/// the merge; the revert merge request (dave's, not a draft) targets main;
/// the branch is recorded as a push. A second revert is `branch_exists`.
#[tokio::test]
async fn a_merge_commit_is_reverted_by_one_revert_merge_request() {
    let mut w = world().await;
    let merged = w.merged(MergeMethod::MergeCommit).await;
    let opened = w.revert_as(w.dev, "req-revert").await.expect("reverted");
    let mr = &opened.merge_request;
    assert_eq!(mr.source_branch.as_str(), "refs/heads/revert/mr-1");
    assert_eq!(mr.target_branch.as_str(), "refs/heads/main");
    assert_eq!(mr.author, w.dev);
    assert!(!mr.draft);
    assert_eq!(mr.title, "Revert \"Add the feature\"");
    assert_eq!(opened.reverted_iid, 1);
    assert_eq!(opened.parent.as_str(), merged);
    let tip = w.tip("revert/mr-1").expect("branch");
    assert_eq!(tip, opened.commit.as_str());
    let raw = plumb(&w.path, &["cat-file", "commit", &tip], None);
    assert_eq!(raw.matches("parent ").count(), 1, "{raw}");
    assert!(raw.contains(&format!("parent {merged}")), "{raw}");
    assert!(raw.contains("author dave <dave@example.test>"), "{raw}");
    assert!(
        raw.contains(&format!("This reverts commit {merged}.")),
        "{raw}"
    );
    let files = plumb(&w.path, &["ls-tree", "--name-only", &tip], None);
    assert_eq!(files, "README.md");
    // main itself is untouched: the revert lands by its own merge.
    assert_eq!(w.tip("main").as_deref(), Some(merged.as_str()));
    let pushed = w.events("repository.pushed").await;
    assert!(
        pushed
            .iter()
            .any(|e| e["payload"].to_string().contains("refs/heads/revert/mr-1")),
        "{pushed:?}"
    );
    let again = w
        .revert_as(w.dev, "req-revert-2")
        .await
        .expect_err("refused");
    assert_eq!(again.code(), ErrorCode::RevertRefused);
    assert_eq!(reason(&again), "branch_exists");
    w.t.drop_now().await;
}

/// A squash merge is reverted to the squash commit's parent.
#[tokio::test]
async fn a_squash_merge_is_reverted() {
    let mut w = world().await;
    let merged = w.merged(MergeMethod::Squash).await;
    let raw = plumb(&w.path, &["cat-file", "commit", &merged], None);
    assert_eq!(raw.matches("parent ").count(), 1, "squash: {raw}");
    let opened = w.revert_as(w.owner, "req-revert").await.expect("reverted");
    let tip = opened.commit.as_str().to_owned();
    let tree = plumb(&w.path, &["rev-parse", &format!("{tip}^{{tree}}")], None);
    let base_tree = plumb(
        &w.path,
        &["rev-parse", &format!("{}^{{tree}}", w.base)],
        None,
    );
    assert_eq!(tree, base_tree);
    assert_eq!(opened.merge_request.author, w.owner);
    w.t.drop_now().await;
}

/// FG-REVIEW-0030: main changed the merged file again after the merge; the
/// revert conflicts and NOTHING is opened — no branch, no merge request.
#[tokio::test]
async fn a_revert_that_conflicts_opens_nothing() {
    let mut w = world().await;
    let merged = w.merged(MergeMethod::MergeCommit).await;
    let later = commit(
        &w.path,
        &[("README.md", "base\n"), ("feature.txt", "changed later\n")],
        Some(&merged),
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &later], None);
    w.browser.invalidate(w.repo);
    let before = w.merge_request_count().await;
    let err = w
        .revert_as(w.dev, "req-revert")
        .await
        .expect_err("conflict");
    assert_eq!(err.code(), ErrorCode::RevertConflict);
    assert!(w.tip("revert/mr-1").is_none(), "no branch");
    assert_eq!(w.merge_request_count().await, before, "no merge request");
    w.t.drop_now().await;
}

/// FG-REVIEW-0029 `not_merged`: an open merge request has nothing to
/// revert; nothing is written.
#[tokio::test]
async fn a_merge_request_that_is_not_merged_cannot_be_reverted() {
    let w = world().await;
    let err = w.revert_as(w.dev, "req-revert").await.expect_err("refused");
    assert_eq!(err.code(), ErrorCode::RevertRefused);
    assert_eq!(reason(&err), "not_merged");
    assert!(w.tip("revert/mr-1").is_none());
    w.t.drop_now().await;
}

/// Who may revert: decided as opening a merge request and pushing the new
/// branch. A Reporter is refused by `authorize()` (FG-AUTH-0003) before
/// anything is written; a protected `revert/*` refuses a Developer's
/// branch as the push path would (FG-GIT-0008).
#[tokio::test]
async fn only_who_may_open_and_push_the_branch_may_revert() {
    let mut w = world().await;
    w.merged(MergeMethod::MergeCommit).await;
    let err = w
        .revert_as(w.reporter, "req-reporter")
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(w.tip("revert/mr-1").is_none());
    // `revert/*` protected at Maintainer: dave may not create the branch.
    forge_db::protected_refs::create(
        &w.t.db,
        &system("req-protect"),
        w.project,
        &forge_core::RefPattern::parse("refs/heads/revert/*").expect("pattern"),
        Role::Maintainer,
    )
    .await
    .expect("protect");
    let err = w.revert_as(w.dev, "req-dev").await.expect_err("refused");
    assert_eq!(err.code(), ErrorCode::ProtectedRefPushNotAllowed);
    assert!(w.tip("revert/mr-1").is_none());
    // The owner may.
    w.revert_as(w.owner, "req-owner").await.expect("reverted");
    assert!(w.tip("revert/mr-1").is_some());
    w.t.drop_now().await;
}

/// plan 14 C10 "main の失敗で下書き": main's push pipeline fails on the
/// merge commit (main passed before the merge). The `ci/pipeline` failure
/// enqueues one draft job; the job opens a DRAFT revert authored by the
/// reverted merge request's author, audited as the system, and records
/// `merge_request.revert_drafted` (opened) on the reverted merge request,
/// once.
#[tokio::test]
async fn a_failed_target_pipeline_drafts_a_revert_once() {
    let mut w = world().await;
    let merged = w.merged(MergeMethod::MergeCommit).await;
    w.push_pipeline("main", &w.base, "succeeded").await;
    w.push_pipeline("main", &merged, "failed").await;
    w.ci_pipeline_concluded(&merged, CheckConclusion::Failure)
        .await;
    let jobs = w.draft_jobs().await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let asked = revert_draft_payload(&jobs[0]).expect("payload");
    assert_eq!(asked.sha.as_str(), merged);
    let rid = RequestId::parse("req-draft").expect("id");
    let report = w
        .reverter
        .draft_after_failure(asked.project, &asked.sha, &rid)
        .await
        .expect("draft");
    let DraftReport::Opened { iid } = report else {
        panic!("not opened: {report:?}");
    };
    let draft = w
        .review
        .merge_request(w.project, iid)
        .await
        .expect("read")
        .expect("draft");
    assert!(draft.draft);
    assert_eq!(draft.author, w.dev, "the reverted merge request's author");
    assert_eq!(draft.source_branch.as_str(), "refs/heads/revert/mr-1");
    let raw = plumb(
        &w.path,
        &["cat-file", "commit", "refs/heads/revert/mr-1"],
        None,
    );
    assert!(
        raw.contains("author GitStation <revert@gitstation.invalid>"),
        "{raw}"
    );
    let drafted = w.events("merge_request.revert_drafted").await;
    assert_eq!(drafted.len(), 1, "{drafted:?}");
    assert_eq!(drafted[0]["aggregate_id"], w.mr.id.get());
    assert_eq!(drafted[0]["actor_kind"], "system");
    assert_eq!(drafted[0]["payload"]["outcome"], "opened");
    assert_eq!(drafted[0]["payload"]["revert_iid"], iid);
    // The same failure again (a retried job): nothing more.
    let again = w
        .reverter
        .draft_after_failure(asked.project, &asked.sha, &rid)
        .await
        .expect("again");
    assert_eq!(again, DraftReport::Skipped("branch_exists"));
    w.ci_pipeline_concluded(&merged, CheckConclusion::Success)
        .await;
    w.ci_pipeline_concluded(&merged, CheckConclusion::Failure)
        .await;
    assert_eq!(w.draft_jobs().await.len(), 1, "one job per commit");
    assert_eq!(w.events("merge_request.revert_drafted").await.len(), 1);
    w.t.drop_now().await;
}

/// A conflicting draft opens nothing and records `outcome=conflicted` (the
/// author is told); a target that was failing before the merge is not the
/// merge's fault (nothing drafted); a failure of another branch's or a
/// merge request's pipeline enqueues nothing.
#[tokio::test]
async fn a_draft_conflict_is_recorded_and_unrelated_failures_draft_nothing() {
    let mut w = world().await;
    let merged = w.merged(MergeMethod::Squash).await;
    // A merge request's pipeline (merge result) or a feature branch failing
    // on another commit: no job.
    w.ci_pipeline_concluded(&w.head.clone(), CheckConclusion::Failure)
        .await;
    assert!(w.draft_jobs().await.is_empty());
    // main was already failing before the merge.
    w.push_pipeline("main", &w.base, "failed").await;
    w.push_pipeline("main", &merged, "failed").await;
    let rid = RequestId::parse("req-draft").expect("id");
    let report = w
        .reverter
        .draft_after_failure(w.project, &oid(&merged), &rid)
        .await
        .expect("draft");
    assert_eq!(report, DraftReport::Skipped("target_already_failing"));
    w.t.drop_now().await;

    let mut w = world().await;
    let merged = w.merged(MergeMethod::MergeCommit).await;
    w.push_pipeline("main", &merged, "failed").await;
    let later = commit(
        &w.path,
        &[("README.md", "base\n"), ("feature.txt", "changed later\n")],
        Some(&merged),
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &later], None);
    let before = w.merge_request_count().await;
    let report = w
        .reverter
        .draft_after_failure(w.project, &oid(&merged), &rid)
        .await
        .expect("draft");
    assert_eq!(report, DraftReport::Conflicted);
    assert_eq!(w.merge_request_count().await, before, "nothing opened");
    assert!(w.tip("revert/mr-1").is_none());
    let drafted = w.events("merge_request.revert_drafted").await;
    assert_eq!(drafted.len(), 1, "{drafted:?}");
    assert_eq!(drafted[0]["payload"]["outcome"], "conflicted");
    assert_eq!(drafted[0]["payload"]["revert_iid"], serde_json::Value::Null);
    w.t.drop_now().await;
}
