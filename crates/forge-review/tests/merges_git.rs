#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The merge (M3 R4, ADR-0024 §4) against a real bare repository and the
//! development database: the request (`Review::request_merge`), the job
//! (`Merger::execute`) and the reconciliation (`Merger::reconcile`).
//! Versions and mergeability are written by the worker's own functions
//! (`forge_review::refresh` / `check`).

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use forge_auth::{prove, Action, Credential, MergeRequestCtx, Principal, RefProtection};
use forge_core::{
    MergeMethod, MergeState, ObjectId, ProjectId, RefPattern, RequestId, Role, Scope, UserId,
};
use forge_db::repositories;
use forge_db::review::{merges, request_check, MergeabilityRequest, RefreshRequest};
use forge_db::test_support::FreshDb;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{
    check, refresh, CredentialRef, MergeAsk, MergeSettings, MergeTimeouts, Mergeability, Merger,
    NewFields, Review, StoredMerge, StoredMergeRequest, VersionRefs,
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
    mr: StoredMergeRequest,
    base: String,
    head: String,
}

/// `acme/web` with `main` at base and `feature` one commit ahead (a new
/// file); olga owns it, dave is a Developer and opens the merge request.
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
    let head = commit(
        &path,
        &[("README.md", "base\n"), ("feature.txt", "f\n")],
        Some(&base),
    );
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &head], None);
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let review = Review::new(t.db.clone(), Arc::new(browser.clone()));
    let merger =
        Merger::new(t.db.clone(), git.clone(), storage.clone()).with_browser(browser.clone());
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
        git,
        storage,
        browser,
        review,
        merger,
        owner,
        dev,
        project: web.project_id,
        repo: web.repository_id,
        mr,
        base,
        head,
    }
}

impl World {
    /// Record the version and run the mergeability check, as the worker
    /// does; the merge request's head is then `self.head`.
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
        self.mr = self
            .review
            .merge_request(self.project, self.mr.iid)
            .await
            .expect("read")
            .expect("mr");
        assert_eq!(self.mr.head_sha.as_deref(), Some(self.head.as_str()));
        self.browser.invalidate(self.repo);
    }

    /// The route's proof for `who` (the target's protection as loaded).
    async fn proof(
        &self,
        who: &Principal,
    ) -> Result<forge_auth::Authorized<MergeRequestCtx>, ForgeError> {
        let project = loaded(&self.t, who, self.project).await;
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
        prove(
            who,
            Action::MergeRequestMerge,
            MergeRequestCtx::merging(project, forge_review::loaded_merge_request(&mr), protection),
        )
    }

    async fn session_of(&self, user: UserId) -> i64 {
        forge_db::sessions::issue(
            &self.t.db,
            &system("req-session"),
            &forge_db::sessions::NewSession {
                user,
                idle: Duration::from_secs(3600),
                absolute: Duration::from_secs(8 * 3600),
                oidc: None,
            },
        )
        .await
        .expect("session")
        .session_id
    }

    /// dave asks for the merge in his web session.
    async fn ask(
        &self,
        method: Option<MergeMethod>,
        delete_source: bool,
    ) -> Result<StoredMerge, ForgeError> {
        let who = session(self.dev);
        let proof = self.proof(&who).await?;
        let credential = CredentialRef::Session(self.session_of(self.dev).await);
        self.review
            .request_merge(
                &request("req-merge"),
                credential,
                &proof,
                ready(self.repo),
                &MergeAsk {
                    sha: oid(&self.head),
                    method,
                    delete_source_branch: Some(delete_source),
                    message: None,
                },
            )
            .await
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

    async fn attempt(&self, id: i64) -> StoredMerge {
        merges::merge_attempt(&self.t.db, id)
            .await
            .expect("read")
            .expect("row")
    }

    async fn events(&self, kind: &str) -> Vec<serde_json::Value> {
        self.t
            .json_rows(&format!(
                "SELECT event_type, dedup_key, payload FROM forge.events \
                 WHERE event_type = '{kind}' ORDER BY id"
            ))
            .await
    }

    async fn browsed_tip(&self, branch: &str) -> Option<String> {
        let refs = self.browser.refs(self.repo).await.expect("refs");
        refs.refs
            .iter()
            .find(|r| r.name.as_str() == format!("refs/heads/{branch}"))
            .and_then(|r| r.commit.clone())
    }
}

fn code(e: &ForgeError) -> ErrorCode {
    e.code()
}

fn reason(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// ADR-0024 §4.1, §4.2: a merge commit lands — parents `[old target,
/// head]`, the trial merge's tree, author and committer from the database
/// (the merging user) — the attempt is `applied`, the merge request
/// `merged`, and `merge_request.merged` / `repository.pushed` are written
/// once each, keyed by the merge id. A browse right after the merge sees
/// the new tip (the cached ref list was invalidated).
#[tokio::test]
async fn a_merge_commit_lands_with_its_events_and_the_browse_is_not_stale() {
    let mut w = world().await;
    w.checked().await;
    // Warm the browse cache with the old tip.
    assert_eq!(
        w.browsed_tip("main").await.as_deref(),
        Some(w.base.as_str())
    );
    let attempt = w.ask(None, false).await.expect("queued");
    assert_eq!(attempt.state, MergeState::Queued);
    assert_eq!(attempt.method, MergeMethod::MergeCommit);
    let report = w.merger.execute(attempt.id).await.expect("merge");
    assert_eq!(report.outcome, "applied");
    let tip = w.tip("main").expect("main");
    assert_eq!(
        report.new_sha.as_ref().map(ObjectId::as_str),
        Some(tip.as_str())
    );
    let raw = plumb(&w.path, &["cat-file", "commit", &tip], None);
    assert!(
        raw.contains(&format!("parent {}\nparent {}\n", w.base, w.head)),
        "{raw}"
    );
    assert!(raw.contains("author dave <dave@example.test>"), "{raw}");
    assert!(raw.contains("committer dave <dave@example.test>"), "{raw}");
    assert!(raw.contains("Merge branch 'feature' into 'main'"), "{raw}");
    let files = plumb(&w.path, &["ls-tree", "--name-only", &tip], None);
    assert_eq!(files, "README.md\nfeature.txt");
    // Not stale: the browse sees the merge at once.
    assert_eq!(w.browsed_tip("main").await.as_deref(), Some(tip.as_str()));
    let done = w.attempt(attempt.id).await;
    assert_eq!(done.state, MergeState::Applied);
    assert_eq!(
        done.expected_old.as_ref().map(ObjectId::as_str),
        Some(w.base.as_str())
    );
    let mr = w
        .review
        .merge_request(w.project, w.mr.iid)
        .await
        .expect("read")
        .expect("mr");
    assert_eq!(mr.state, forge_core::MergeRequestState::Merged);
    let merged = w.events("merge_request.merged").await;
    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged[0]["dedup_key"],
        format!("merge_request_merged:{}", attempt.id)
    );
    assert_eq!(merged[0]["payload"]["merged_by"], w.dev.get());
    let pushed = w.events("repository.pushed").await;
    assert_eq!(pushed.len(), 1, "the target's push record");
    assert_eq!(
        pushed[0]["dedup_key"],
        format!("merge_pushed:{}", attempt.id)
    );
    // A second run of the same job changes nothing.
    let again = w.merger.execute(attempt.id).await.expect("again");
    assert_eq!(again.outcome, "finished");
    assert_eq!(w.events("merge_request.merged").await.len(), 1);
    w.t.drop_now().await;
}

/// Squash: one commit with parent `[old target]`, the trial merge's tree,
/// author = the merge request's author, committer = the merging user;
/// fast-forward (when the project allows it): the target moves to the head
/// itself.
#[tokio::test]
async fn squash_and_fast_forward_build_their_own_results() {
    let mut w = world().await;
    w.checked().await;
    // olga merges dave's merge request with a squash.
    let who = session(w.owner);
    let proof = w.proof(&who).await.expect("proof");
    let sid = w.session_of(w.owner).await;
    let attempt = w
        .review
        .request_merge(
            &request("req-squash"),
            CredentialRef::Session(sid),
            &proof,
            ready(w.repo),
            &MergeAsk {
                sha: oid(&w.head),
                method: Some(MergeMethod::Squash),
                delete_source_branch: None,
                message: Some("Squashed feature\n\nBody.".to_owned()),
            },
        )
        .await
        .expect("queued");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    let tip = w.tip("main").expect("main");
    let raw = plumb(&w.path, &["cat-file", "commit", &tip], None);
    assert_eq!(raw.matches("parent ").count(), 1, "{raw}");
    assert!(raw.contains(&format!("parent {}", w.base)), "{raw}");
    assert!(raw.contains("author dave <dave@example.test>"), "{raw}");
    assert!(raw.contains("committer olga <olga@example.test>"), "{raw}");
    assert!(raw.ends_with("Squashed feature\n\nBody."), "{raw}");
    w.t.drop_now().await;

    // Fast-forward, allowed by the project's settings.
    let mut w = world().await;
    w.checked().await;
    let settings = MergeSettings::allowing(&[MergeMethod::FastForward]).expect("settings");
    w.review = w.review.clone().with_merge_settings(settings.clone());
    w.merger = w.merger.clone().with_settings(settings);
    let attempt = w
        .ask(Some(MergeMethod::FastForward), false)
        .await
        .expect("queued");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    assert_eq!(w.tip("main").as_deref(), Some(w.head.as_str()));
    w.t.drop_now().await;
}

/// FG-REVIEW-0010: the default settings allow merge_commit and squash
/// only; nothing is written.
#[tokio::test]
async fn a_merge_method_the_project_does_not_allow_is_refused() {
    let mut w = world().await;
    w.checked().await;
    let err = w
        .ask(Some(MergeMethod::FastForward), false)
        .await
        .expect_err("not allowed");
    assert_eq!(code(&err), ErrorCode::MergeMethodNotAllowed);
    assert!(merges::latest_merge(&w.t.db, w.mr.id)
        .await
        .expect("read")
        .is_none());
    w.t.drop_now().await;
}

/// FG-REVIEW-0007 with every reason (ADR-0024 §7): a draft whose
/// mergeability is not decided for the live pair. The refusal requests a
/// check (so a retry can pass); nothing else is written.
#[tokio::test]
async fn a_merge_request_that_is_not_ready_is_refused_with_its_reasons() {
    let mut w = world().await;
    w.checked().await;
    // The target moves: the stored check no longer answers the live pair.
    let moved = commit(
        &w.path,
        &[("README.md", "base\n"), ("other.txt", "o\n")],
        Some(&w.base),
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &moved], None);
    w.browser.invalidate(w.repo);
    let draft = forge_db::review::MergeRequestPatch {
        draft: Some(true),
        ..forge_db::review::MergeRequestPatch::default()
    };
    let (_, _, ()) = forge_db::review::edit_mr_decided(
        &w.t.db,
        &system("req-draft"),
        w.project,
        w.mr.iid,
        w.dev,
        &draft,
        |_| Ok(()),
    )
    .await
    .expect("draft");
    let before: i64 = count(
        &w.t,
        "SELECT check_seq AS n FROM forge.merge_request_mergeability",
    )
    .await;
    let err = w.ask(None, false).await.expect_err("blocked");
    assert_eq!(code(&err), ErrorCode::MergeBlocked);
    let reasons = err
        .context()
        .iter()
        .find(|(k, _)| *k == "reasons")
        .map(|(_, v)| v.clone())
        .expect("reasons");
    assert_eq!(reasons, "draft,mergeability_unknown");
    let after: i64 = count(
        &w.t,
        "SELECT check_seq AS n FROM forge.merge_request_mergeability",
    )
    .await;
    assert_eq!(after, before + 1, "a new check was requested");
    assert!(merges::latest_merge(&w.t.db, w.mr.id)
        .await
        .expect("read")
        .is_none());
    w.t.drop_now().await;
}

/// FG-REVIEW-0008 `stale_view`: the requester did not see the current head.
#[tokio::test]
async fn a_merge_of_a_head_the_requester_did_not_see_is_refused() {
    let mut w = world().await;
    w.checked().await;
    let who = session(w.dev);
    let proof = w.proof(&who).await.expect("proof");
    let err = w
        .review
        .request_merge(
            &request("req-merge"),
            CredentialRef::Session(w.session_of(w.dev).await),
            &proof,
            ready(w.repo),
            &MergeAsk {
                sha: oid(&w.base),
                method: None,
                delete_source_branch: None,
                message: None,
            },
        )
        .await
        .expect_err("stale");
    assert_eq!(code(&err), ErrorCode::HeadChanged);
    assert_eq!(reason(&err), "stale_view");
    w.t.drop_now().await;
}

/// FG-REVIEW-0006: the target moved after the request to a commit that
/// conflicts; the job's own trial merge under the lock finds it, the attempt
/// is superseded and no ref moves.
#[tokio::test]
async fn a_merge_whose_trial_merge_conflicts_is_superseded() {
    let mut w = world().await;
    w.checked().await;
    let attempt = w.ask(None, false).await.expect("queued");
    let clash = commit(
        &w.path,
        &[("README.md", "base\n"), ("feature.txt", "clash\n")],
        Some(&w.base),
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &clash], None);
    let report = w.merger.execute(attempt.id).await.expect("decided");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::MergeRequestConflict));
    assert_eq!(w.tip("main").as_deref(), Some(clash.as_str()));
    let done = w.attempt(attempt.id).await;
    assert_eq!(done.state, MergeState::Superseded);
    assert_eq!(done.error_code.as_deref(), Some("FG-REVIEW-0006"));
    assert!(w.events("merge_request.merged").await.is_empty());
    w.t.drop_now().await;
}

/// FG-REVIEW-0008 `target_moved`: someone pushes to the target between the
/// trial merge and `update-ref`. The compare-and-swap loses; the attempt is
/// superseded (never stuck), the pushed tip stays, no event is written.
#[tokio::test]
async fn a_concurrent_push_to_the_target_loses_the_compare_and_swap() {
    let mut w = world().await;
    w.checked().await;
    let attempt = w.ask(None, false).await.expect("queued");
    let pushed = commit(
        &w.path,
        &[("README.md", "base\n"), ("late.txt", "l\n")],
        Some(&w.base),
    );
    let (path, to) = (w.path.clone(), pushed.clone());
    let merger = w.merger.clone().with_before_update_ref(Arc::new(move || {
        plumb(&path, &["update-ref", "refs/heads/main", &to], None);
    }));
    let report = merger.execute(attempt.id).await.expect("decided");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::HeadChanged));
    assert_eq!(w.tip("main").as_deref(), Some(pushed.as_str()));
    let done = w.attempt(attempt.id).await;
    assert_eq!(done.state, MergeState::Superseded);
    assert_eq!(done.reason.as_deref(), Some("target_moved"));
    assert!(w.events("merge_request.merged").await.is_empty());
    assert!(w.events("repository.pushed").await.is_empty());
    let mr = w
        .review
        .merge_request(w.project, w.mr.iid)
        .await
        .expect("read")
        .expect("mr");
    assert_eq!(mr.state, forge_core::MergeRequestState::Open);
    w.t.drop_now().await;
}

/// Two merges at once: two requests let exactly one through
/// (FG-REVIEW-0011 for the other); two runs of the one attempt move the
/// ref once and record it once (the lock and the fence).
#[tokio::test]
async fn two_merges_of_one_merge_request_at_once_let_exactly_one_win() {
    let mut w = world().await;
    w.checked().await;
    let (a, b) = tokio::join!(w.ask(None, false), w.ask(None, false));
    let attempt = match (a, b) {
        (Ok(m), Err(e)) | (Err(e), Ok(m)) => {
            assert_eq!(code(&e), ErrorCode::MergeInProgress);
            m
        }
        other => panic!("exactly one request must win: {other:?}"),
    };
    let (x, y) = tokio::join!(w.merger.execute(attempt.id), w.merger.execute(attempt.id));
    let outcomes = [x.expect("run a").outcome, y.expect("run b").outcome];
    assert_eq!(
        outcomes.iter().filter(|o| **o == "applied").count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(w.events("merge_request.merged").await.len(), 1);
    assert_eq!(w.events("repository.pushed").await.len(), 1);
    w.t.drop_now().await;
}

/// ADR-0024 §4.3: the merge path decides the target's protection too.
/// A Developer cannot even be proven for a Maintainer-protected target;
/// a protection raised after the request makes the job refuse (FG-AUTH-0003)
/// and the ref does not move. The direct-push ban is not consulted: a
/// Maintainer merges into the protected branch.
#[tokio::test]
async fn the_protected_ref_rule_applies_on_the_merge_path() {
    let mut w = world().await;
    w.checked().await;
    let attempt = w.ask(None, false).await.expect("queued");
    forge_db::protected_refs::create(
        &w.t.db,
        &system("req-protect"),
        w.project,
        &RefPattern::parse("refs/heads/main").expect("pattern"),
        Role::Maintainer,
    )
    .await
    .expect("protect");
    let report = w.merger.execute(attempt.id).await.expect("decided");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::Forbidden));
    assert_eq!(w.tip("main").as_deref(), Some(w.base.as_str()));
    // The route's proof refuses the Developer now.
    let err = w.proof(&session(w.dev)).await.expect_err("protected");
    assert_eq!(code(&err), ErrorCode::Forbidden);
    // The owner (above Maintainer) merges.
    let who = session(w.owner);
    let proof = w.proof(&who).await.expect("owner");
    let sid = w.session_of(w.owner).await;
    let ok = w
        .review
        .request_merge(
            &request("req-owner"),
            CredentialRef::Session(sid),
            &proof,
            ready(w.repo),
            &MergeAsk {
                sha: oid(&w.head),
                method: None,
                delete_source_branch: None,
                message: None,
            },
        )
        .await
        .expect("queued");
    assert_eq!(
        w.merger.execute(ok.id).await.expect("merge").outcome,
        "applied"
    );
    w.t.drop_now().await;
}

/// ADR-0024 §5: the job decides with the credential as it is now — a token
/// revoked after the request is refused (`credential_revoked`), the attempt
/// superseded, the ref untouched. A `merge_execute` token is what a token
/// merge needs (an `api` token is refused at the proof).
#[tokio::test]
async fn merge_job_with_revoked_token_is_denied() {
    let mut w = world().await;
    w.checked().await;
    let api_only = Principal::authenticated(
        w.dev,
        false,
        true,
        Credential::Token(vec![Scope::Api]),
        forge_auth::AccountKind::Human,
    );
    assert_eq!(
        code(&w.proof(&api_only).await.expect_err("api")),
        ErrorCode::InsufficientScope
    );
    let issued = forge_db::tokens::issue(
        &w.t.db,
        &system("req-token"),
        &forge_db::tokens::NewToken {
            user: w.dev,
            name: "merge".to_owned(),
            scopes: vec![Scope::MergeExecute],
            expires_in: None,
            is_agent: false,
        },
    )
    .await
    .expect("token");
    let who = Principal::authenticated(
        w.dev,
        false,
        true,
        Credential::Token(vec![Scope::MergeExecute]),
        forge_auth::AccountKind::Human,
    );
    let proof = w.proof(&who).await.expect("merge_execute");
    let attempt = w
        .review
        .request_merge(
            &request("req-token-merge"),
            CredentialRef::Token(issued.token_id),
            &proof,
            ready(w.repo),
            &MergeAsk {
                sha: oid(&w.head),
                method: None,
                delete_source_branch: None,
                message: None,
            },
        )
        .await
        .expect("queued");
    let _revoked = forge_db::tokens::revoke(&w.t.db, &system("req-revoke"), issued.token_id)
        .await
        .expect("revoke");
    let report = w.merger.execute(attempt.id).await.expect("decided");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::Forbidden));
    let done = w.attempt(attempt.id).await;
    assert_eq!(done.reason.as_deref(), Some("credential_revoked"));
    assert_eq!(w.tip("main").as_deref(), Some(w.base.as_str()));
    w.t.drop_now().await;
}

/// ADR-0024 §4.2 step 10: the source branch is deleted after the merge
/// (compare-and-swap on the merged head), recorded with
/// `merge_request.source_branch_deleted` and the delete's
/// `repository.pushed`, and a browse right after does not list it. A
/// protected source is kept (`kept_refused`), decided as a branch delete.
#[tokio::test]
async fn the_source_branch_is_deleted_after_the_merge_unless_protected() {
    let mut w = world().await;
    w.checked().await;
    assert!(w.browsed_tip("feature").await.is_some());
    let attempt = w.ask(None, true).await.expect("queued");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    assert_eq!(w.tip("feature"), None, "deleted");
    assert_eq!(
        w.browsed_tip("feature").await,
        None,
        "the browse is not stale"
    );
    let done = w.attempt(attempt.id).await;
    assert_eq!(
        done.source_outcome,
        Some(forge_core::SourceOutcome::Deleted)
    );
    let deleted = w.events("merge_request.source_branch_deleted").await;
    assert_eq!(deleted.len(), 1);
    assert_eq!(
        deleted[0]["dedup_key"],
        format!("merge_source_deleted:{}", attempt.id)
    );
    assert_eq!(
        w.events("repository.pushed").await.len(),
        2,
        "merge + delete"
    );
    w.t.drop_now().await;

    let mut w = world().await;
    w.checked().await;
    forge_db::protected_refs::create(
        &w.t.db,
        &system("req-protect"),
        w.project,
        &RefPattern::parse("refs/heads/feature").expect("pattern"),
        Role::Developer,
    )
    .await
    .expect("protect");
    let attempt = w.ask(None, true).await.expect("queued");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    assert_eq!(w.tip("feature").as_deref(), Some(w.head.as_str()), "kept");
    let done = w.attempt(attempt.id).await;
    assert_eq!(
        done.source_outcome,
        Some(forge_core::SourceOutcome::KeptRefused)
    );
    w.t.drop_now().await;
}

/// A git timeout before the fence: the run fails (the job retries) and the
/// attempt stays `queued` — never stuck: when the job gives up, the attempt
/// ends `superseded reason=job_failed`; no ref moved. Objects the run made
/// may stay unreferenced (pruned by O3's maintenance).
#[tokio::test]
async fn a_timeout_before_the_fence_is_retried_then_cleaned_up() {
    let mut w = world().await;
    w.checked().await;
    let attempt = w.ask(None, false).await.expect("queued");
    let slow = w
        .merger
        .clone()
        .with_timeouts(MergeTimeouts {
            commit: Duration::from_nanos(1),
            ..MergeTimeouts::default()
        })
        .expect("timeouts");
    let err = slow.execute(attempt.id).await.expect_err("timeout");
    assert_eq!(code(&err), ErrorCode::GitTimeout);
    assert_eq!(w.attempt(attempt.id).await.state, MergeState::Queued);
    assert!(slow.give_up(attempt.id, &err).await.expect("give up"));
    let done = w.attempt(attempt.id).await;
    assert_eq!(done.state, MergeState::Superseded);
    assert_eq!(done.reason.as_deref(), Some("job_failed"));
    assert_eq!(done.error_code.as_deref(), Some("FG-GIT-0002"));
    assert_eq!(w.tip("main").as_deref(), Some(w.base.as_str()));
    // The merge request can be merged again.
    let again = w.ask(None, false).await.expect("queued again");
    assert_eq!(
        w.merger.execute(again.id).await.expect("merge").outcome,
        "applied"
    );
    w.t.drop_now().await;
}

/// Puts an attempt into `executing` as a worker that stopped right after the
/// fence would leave it; `moved_to` is where the target is now. The
/// reconciler of `an_executing_merge_whose_ref_state_is_unknown_is_failed` has no grace, so the attempt
/// is past its bound at once (the guard keeps `executing_at` set once).
async fn stuck_executing(w: &mut World, moved_to: Option<&str>) -> (StoredMerge, String) {
    stuck_executing_with(w, moved_to, false).await
}

async fn stuck_executing_with(
    w: &mut World,
    moved_to: Option<&str>,
    delete_source: bool,
) -> (StoredMerge, String) {
    let attempt = w.ask(None, delete_source).await.expect("queued");
    // A commit the worker would have made (the merge's tree is irrelevant).
    let new_sha = commit(&w.path, &[("README.md", "merged\n")], Some(&w.base));
    let step = merges::GateStep::Prepare {
        expected_old: oid(&w.base),
        new_sha: oid(&new_sha),
    };
    let prepared =
        merges::merge_gate_decided(&w.t.db, &system("req-gate"), attempt.id, step, |_| Ok(()))
            .await
            .expect("prepare");
    assert!(matches!(prepared, merges::GateOutcome::Decided(())));
    assert!(merges::fence_merge(&w.t.db, attempt.id)
        .await
        .expect("fence"));
    if let Some(to) = moved_to {
        let to = if to == "new" {
            new_sha.clone()
        } else {
            to.to_owned()
        };
        plumb(&w.path, &["update-ref", "refs/heads/main", &to], None);
    }
    (attempt, new_sha)
}

/// The reconciliation asks git (ADR-0024 §4.2 照合): an `executing` attempt
/// whose target is neither `new_sha` (nor a descendant) nor `expected_old`
/// is `failed` with FG-REVIEW-0009 `ref_state_unknown` for a person; one at
/// `new_sha` is recorded `applied` (events once); one still at
/// `expected_old` is `superseded`; a `queued` one idle too long is
/// superseded as `stale`.
#[tokio::test]
async fn an_executing_merge_whose_ref_state_is_unknown_is_failed() {
    let reconciler = |w: &World| {
        w.merger
            .clone()
            .with_timeouts(MergeTimeouts {
                update_ref: Duration::ZERO,
                grace: Duration::from_nanos(1),
                idle: Duration::ZERO,
                ..MergeTimeouts::default()
            })
            .expect("timeouts")
    };
    let rid = RequestId::parse("req-reconcile").expect("id");
    // Unknown: the target went elsewhere.
    let mut w = world().await;
    w.checked().await;
    let elsewhere = commit(&w.path, &[("README.md", "elsewhere\n")], None);
    let (attempt, _) = stuck_executing(&mut w, Some(&elsewhere)).await;
    let report = reconciler(&w).reconcile(&rid).await.expect("reconcile");
    assert_eq!(report.failed, 1, "{report:?}");
    let done = w.attempt(attempt.id).await;
    assert_eq!(done.state, MergeState::Failed);
    assert_eq!(done.reason.as_deref(), Some("ref_state_unknown"));
    assert_eq!(done.error_code.as_deref(), Some("FG-REVIEW-0009"));
    w.t.drop_now().await;

    // Landed: the target is new_sha — recorded applied, once.
    let mut w = world().await;
    w.checked().await;
    let (attempt, new_sha) = stuck_executing(&mut w, Some("new")).await;
    let report = reconciler(&w).reconcile(&rid).await.expect("reconcile");
    assert_eq!(report.applied, 1, "{report:?}");
    assert_eq!(w.attempt(attempt.id).await.state, MergeState::Applied);
    assert_eq!(w.tip("main").as_deref(), Some(new_sha.as_str()));
    assert_eq!(w.events("merge_request.merged").await.len(), 1);
    let again = reconciler(&w).reconcile(&rid).await.expect("again");
    assert_eq!(again.looked, 0);
    w.t.drop_now().await;

    // Not applied: the target is still expected_old.
    let mut w = world().await;
    w.checked().await;
    let (attempt, _) = stuck_executing(&mut w, None).await;
    let report = reconciler(&w).reconcile(&rid).await.expect("reconcile");
    assert_eq!(report.superseded, 1, "{report:?}");
    let done = w.attempt(attempt.id).await;
    assert_eq!(done.state, MergeState::Superseded);
    assert_eq!(done.reason.as_deref(), Some("not_applied"));
    assert!(w.events("merge_request.merged").await.is_empty());

    // Stale: a queued attempt nobody ran.
    let stale = w.ask(None, false).await.expect("queued");
    let report = reconciler(&w).reconcile(&rid).await.expect("reconcile");
    assert_eq!(report.superseded, 1, "{report:?}");
    assert_eq!(w.attempt(stale.id).await.reason.as_deref(), Some("stale"));
    w.t.drop_now().await;
}

/// A job that stopped after the ref moved but before tx2 and the source
/// deletion: its retry asks git, records the merge and then deletes the
/// source branch the request asked to delete (step 10 after the record);
/// another retry finds it finished and deletes nothing twice.
#[tokio::test]
async fn a_retry_records_a_landed_merge_and_finishes_the_source_deletion() {
    let mut w = world().await;
    w.checked().await;
    // The browse cache holds the old tip (review round 1: the resume path
    // invalidates it).
    assert_eq!(
        w.browsed_tip("main").await.as_deref(),
        Some(w.base.as_str())
    );
    let (attempt, new_sha) = stuck_executing_with(&mut w, Some("new"), true).await;
    let report = w.merger.execute(attempt.id).await.expect("retry");
    assert_eq!(report.outcome, "applied");
    assert_eq!(w.tip("main").as_deref(), Some(new_sha.as_str()));
    assert_eq!(
        w.browsed_tip("main").await.as_deref(),
        Some(new_sha.as_str())
    );
    let done = w.attempt(attempt.id).await;
    assert_eq!(done.state, MergeState::Applied);
    assert_eq!(
        done.source_outcome,
        Some(forge_core::SourceOutcome::Deleted)
    );
    assert_eq!(w.tip("feature"), None);
    let again = w.merger.execute(attempt.id).await.expect("again");
    assert_eq!(again.outcome, "finished");
    assert_eq!(
        w.events("merge_request.source_branch_deleted").await.len(),
        1
    );
    assert_eq!(w.events("merge_request.merged").await.len(), 1);
    w.t.drop_now().await;
}

/// A run that stopped after the fence (`executing`, inside the window) is
/// resumed by the job's retry: it asks git first, then moves the ref after
/// deciding again.
#[tokio::test]
async fn a_retry_after_the_fence_moves_the_ref_once() {
    let mut w = world().await;
    w.checked().await;
    let attempt = w.ask(None, false).await.expect("queued");
    // A first run whose update-ref times out: FG-REVIEW-0009, the attempt
    // stays `executing` (the ref did not move).
    let first = w
        .merger
        .clone()
        .with_timeouts(MergeTimeouts {
            update_ref: Duration::from_nanos(1),
            ..MergeTimeouts::default()
        })
        .expect("timeouts");
    let err = first
        .execute(attempt.id)
        .await
        .expect_err("update-ref timed out");
    assert_eq!(code(&err), ErrorCode::MergeExecutionFailed);
    assert_eq!(w.attempt(attempt.id).await.state, MergeState::Executing);
    let report = w.merger.execute(attempt.id).await.expect("retry");
    assert_eq!(report.outcome, "applied");
    assert_eq!(w.events("merge_request.merged").await.len(), 1);
    w.t.drop_now().await;
}

/// Review round 1 (security): the default commit message is frozen when the
/// merge is requested — a description edited while the merge is queued
/// (title and description edits are allowed then) never reaches the commit.
#[tokio::test]
async fn the_commit_message_is_frozen_when_the_merge_is_requested() {
    let mut w = world().await;
    w.checked().await;
    let attempt = w.ask(None, false).await.expect("queued");
    assert!(attempt
        .message
        .as_deref()
        .is_some_and(|m| m.contains("Synthetic description.")));
    let edit = forge_db::review::MergeRequestPatch {
        description: Some("Injected later.".to_owned()),
        title: Some("Renamed later".to_owned()),
        ..forge_db::review::MergeRequestPatch::default()
    };
    let (_, _, ()) = forge_db::review::edit_mr_decided(
        &w.t.db,
        &system("req-edit"),
        w.project,
        w.mr.iid,
        w.dev,
        &edit,
        |_| Ok(()),
    )
    .await
    .expect("edit while queued");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    let tip = w.tip("main").expect("main");
    let raw = plumb(&w.path, &["cat-file", "commit", &tip], None);
    assert!(raw.contains("Synthetic description."), "{raw}");
    assert!(raw.contains("Add the feature"), "{raw}");
    assert!(!raw.contains("Injected later."), "{raw}");
    assert!(!raw.contains("Renamed later"), "{raw}");
    w.t.drop_now().await;
}

/// What changes between the request and the job, each in a world of its own.
#[derive(Debug, Clone, Copy)]
enum Change {
    Removed,
    Demoted,
    Disabled,
    BecameBot,
    SessionRevoked,
}

/// Review round 1 / ADR-0024 §5: the job decides on the requester as the
/// database says now — removed from the project, demoted to Reporter,
/// disabled, turned into a bot account, or the session revoked — each is
/// refused at job time; the attempt is superseded and the ref never moves.
#[tokio::test]
async fn the_job_decides_on_the_requester_as_they_are_now() {
    for change in [
        Change::Removed,
        Change::Demoted,
        Change::Disabled,
        Change::BecameBot,
        Change::SessionRevoked,
    ] {
        let mut w = world().await;
        w.checked().await;
        let sid = w.session_of(w.dev).await;
        let proof = w.proof(&session(w.dev)).await.expect("proof");
        let attempt = w
            .review
            .request_merge(
                &request("req-merge"),
                CredentialRef::Session(sid),
                &proof,
                ready(w.repo),
                &MergeAsk {
                    sha: oid(&w.head),
                    method: None,
                    delete_source_branch: None,
                    message: None,
                },
            )
            .await
            .expect("queued");
        match change {
            Change::Removed => forge_db::projects::remove_member(
                &w.t.db,
                &system("req-change"),
                w.project,
                w.dev,
                Role::Developer,
            )
            .await
            .expect("removed"),
            Change::Demoted => {
                let _changed = forge_db::projects::change_member(
                    &w.t.db,
                    &system("req-change"),
                    w.project,
                    w.dev,
                    Role::Developer,
                    Role::Reporter,
                )
                .await
                .expect("demoted");
            }
            Change::Disabled => {
                let _disabled = forge_db::users::disable(&w.t.db, &system("req-change"), w.dev)
                    .await
                    .expect("disabled");
            }
            Change::BecameBot => {
                w.t.commit_audited(
                    ("user", w.dev.get()),
                    &[format!(
                        "UPDATE forge.users SET kind = 'bot' WHERE id = {}",
                        w.dev.get()
                    )],
                )
                .await;
            }
            Change::SessionRevoked => {
                let _revoked = forge_db::sessions::revoke(
                    &w.t.db,
                    &system("req-change"),
                    sid,
                    forge_core::SessionRevokeReason::Admin,
                )
                .await
                .expect("revoked");
            }
        }
        let report = w.merger.execute(attempt.id).await.expect("decided");
        assert_eq!(report.outcome, "superseded", "{change:?}");
        assert!(
            report.error_code.is_some_and(|c| c.http_status() < 500),
            "{change:?}: {report:?}"
        );
        assert_eq!(w.attempt(attempt.id).await.state, MergeState::Superseded);
        assert_eq!(
            w.tip("main").as_deref(),
            Some(w.base.as_str()),
            "{change:?}"
        );
        w.t.drop_now().await;
    }
}

/// Review round 1: a busy merge lock is the only failure the
/// reconciliation skips; the attempt keeps its state and goes to the back
/// of the scan, and the next pass settles it.
#[tokio::test]
async fn the_reconciliation_skips_only_a_busy_lock_and_settles_later() {
    let rid = RequestId::parse("req-reconcile").expect("id");
    let mut w = world().await;
    w.checked().await;
    let (attempt, new_sha) = stuck_executing(&mut w, Some("new")).await;
    let reconciler = w
        .merger
        .clone()
        .with_timeouts(MergeTimeouts {
            update_ref: Duration::ZERO,
            grace: Duration::from_nanos(1),
            idle: Duration::ZERO,
            ..MergeTimeouts::default()
        })
        .expect("timeouts");
    let held = merges::lock_merges(&w.t.db, w.repo, Duration::from_secs(5))
        .await
        .expect("a merge holds the lock");
    let report = reconciler.reconcile(&rid).await.expect("reconcile");
    assert_eq!((report.looked, report.skipped), (1, 1), "{report:?}");
    assert_eq!(w.attempt(attempt.id).await.state, MergeState::Executing);
    held.release().await;
    let report = reconciler.reconcile(&rid).await.expect("reconcile");
    assert_eq!(report.applied, 1, "{report:?}");
    assert_eq!(w.tip("main").as_deref(), Some(new_sha.as_str()));
    w.t.drop_now().await;
}

/// Review round 1 (LOW): a fast-forward whose target is at the head already
/// has nothing to merge (MergeBlocked `nothing_to_merge`), never a record
/// of an unchanged ref.
#[tokio::test]
async fn a_fast_forward_of_a_target_already_at_the_head_has_nothing_to_merge() {
    let mut w = world().await;
    w.checked().await;
    let settings = MergeSettings::allowing(&[MergeMethod::FastForward]).expect("settings");
    w.review = w.review.clone().with_merge_settings(settings.clone());
    w.merger = w.merger.clone().with_settings(settings);
    let attempt = w
        .ask(Some(MergeMethod::FastForward), false)
        .await
        .expect("queued");
    plumb(&w.path, &["update-ref", "refs/heads/main", &w.head], None);
    let report = w.merger.execute(attempt.id).await.expect("decided");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::MergeBlocked));
    assert!(w.events("repository.pushed").await.is_empty());
    w.t.drop_now().await;
}
