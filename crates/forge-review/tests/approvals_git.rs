#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Approvals, approval rules and CODEOWNERS (M3 R5, ADR-0024 §8, §9)
//! against a real bare repository and the development database, end to
//! end: approve → merge succeeds; a push after the approval → the merge is
//! blocked; a committer's approval does not count; CODEOWNERS is read from
//! the target and an unusable one blocks; the merge job decides again at
//! execution time.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use forge_auth::{prove, Action, MergeRequestCtx, Principal, ProjectCtx, RefProtection};
use forge_core::{
    ObjectId, ProjectId, PushRecord, PushUpdateKind, PushedRef, RefName, RequestId, Role,
    UncountedReason, UserId,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::repositories;
use forge_db::review::{request_check, ApproveOutcome, MergeabilityRequest, RefreshRequest};
use forge_db::test_support::FreshDb;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::approvals::{ReviewGit, RuleAsk};
use forge_review::{
    check, refresh, CredentialRef, MergeAsk, Mergeability, Merger, NewFields, Review,
    StoredMergeRequest, VersionRefs,
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

/// A commit whose tree holds `files` (flat names) on top of `parent`.
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

/// A merge commit of `parents` whose tree holds `files`.
fn merge_commit(repo: &Path, files: &[(&str, &str)], parents: &[&str]) -> String {
    let single = commit(repo, files, None);
    let tree = plumb(repo, &["rev-parse", &format!("{single}^{{tree}}")], None);
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "merge"];
    for p in parents {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

fn oid(raw: &str) -> ObjectId {
    ObjectId::parse(raw).expect("oid")
}

fn reasons(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reasons")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn detail(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "details")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
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
    /// The author (a Developer).
    dave: UserId,
    /// A reviewer (a Developer).
    rita: UserId,
    /// A Maintainer.
    mara: UserId,
    project: ProjectId,
    repo: forge_core::RepoId,
    mr: StoredMergeRequest,
    base: String,
    head: String,
}

/// `acme/web`: `main` at `base` (with `main_files`), `feature` one commit
/// ahead (`head_files`), pushed by dave (a push record, so the committers
/// are known); dave opens the merge request.
async fn world(main_files: &[(&str, &str)], head_files: &[(&str, &str)]) -> World {
    let t = FreshDb::migrated().await;
    let owner = user(&t, "olga").await;
    let dave = user(&t, "dave").await;
    let rita = user(&t, "rita").await;
    let mara = user(&t, "mara").await;
    let web = project(
        &t,
        owner,
        &[
            (dave, Role::Developer),
            (rita, Role::Developer),
            (mara, Role::Maintainer),
        ],
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
    let base = commit(&path, main_files, None);
    let head = commit(&path, head_files, Some(&base));
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &head], None);
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let review = Review::new(t.db.clone(), Arc::new(browser.clone()))
        .with_review_git(ReviewGit::new(git.clone(), storage.clone()));
    let merger =
        Merger::new(t.db.clone(), git.clone(), storage.clone()).with_browser(browser.clone());
    let w0 = World {
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
        dave,
        rita,
        mara,
        project: web.project_id,
        repo: web.repository_id,
        mr: placeholder_mr(),
        base,
        head,
    };
    w0.record_push(dave, "feature", None, &w0.head.clone(), "req-push-dave")
        .await;
    let proof = open_proof(
        &w0.t,
        &session(dave),
        w0.project,
        ("feature", "main"),
        false,
    )
    .await
    .expect("open proof");
    let mr = w0
        .review
        .open(
            &request("req-open"),
            &proof,
            ready(w0.repo),
            &NewFields {
                title: "Add the feature".to_owned(),
                description: String::new(),
            },
        )
        .await
        .expect("open");
    let mut w = World { mr, ..w0 };
    w.checked().await;
    w
}

/// A stand-in until the merge request is opened.
fn placeholder_mr() -> StoredMergeRequest {
    StoredMergeRequest {
        id: forge_core::MergeRequestId::new(0),
        project_id: ProjectId::new(0),
        iid: 0,
        title: String::new(),
        description: String::new(),
        author: UserId::new(0),
        source_branch: branch("feature"),
        target_branch: branch("main"),
        state: forge_core::MergeRequestState::Open,
        draft: false,
        head_sha: None,
        revision: 0,
        created_at: String::new(),
        updated_at: String::new(),
        closed_at: None,
        closed_by: None,
    }
}

impl World {
    /// A `repository.pushed` record of `who` moving `branch` to `new` (the
    /// commit `new` listed as new).
    async fn record_push(
        &self,
        who: UserId,
        branch_name: &str,
        old: Option<&str>,
        new: &str,
        request_id: &str,
    ) {
        self.record_push_listing(who, branch_name, (old, new), (&[new], 1), request_id)
            .await;
    }

    /// A push record listing `listed` of `total` new commits (more than
    /// listed: the record's list is cut, as for a push of 21+ commits).
    async fn record_push_listing(
        &self,
        who: UserId,
        branch_name: &str,
        (old, new): (Option<&str>, &str),
        (listed, total): (&[&str], u64),
        request_id: &str,
    ) {
        let zero = "0".repeat(40);
        let push = PushRecord {
            repository_id: self.repo,
            updates: vec![PushedRef {
                name: RefName::parse(&format!("refs/heads/{branch_name}")).expect("ref"),
                old: oid(old.unwrap_or(&zero)),
                new: oid(new),
                kind: if old.is_some() {
                    PushUpdateKind::FastForward
                } else {
                    PushUpdateKind::Create
                },
                commits: listed.iter().map(|c| oid(c)).collect(),
                total_commits: total,
                total_commits_partial: false,
            }],
        };
        let ctx = ChangeContext::new(Actor::User(who), RequestId::parse(request_id).expect("id"));
        let recorded = repositories::record_push(&self.t.db, &ctx, &push)
            .await
            .expect("push record");
        assert_eq!(recorded, forge_core::PushRecorded::Recorded);
    }

    /// Record the version and run the mergeability check, as the worker
    /// does.
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

    async fn mr_proof(
        &self,
        who: &Principal,
        action: Action,
    ) -> Result<forge_auth::Authorized<MergeRequestCtx>, ForgeError> {
        let project = loaded(&self.t, who, self.project).await;
        let mr = forge_review::loaded_merge_request(&self.mr);
        if action == Action::MergeRequestMerge {
            let rules = forge_db::protected_refs::rules_for(&self.t.db, self.project)
                .await
                .expect("rules");
            let protection = RefProtection::effective(rules.levels(&self.mr.target_branch));
            return prove(
                who,
                action,
                MergeRequestCtx::merging(project, mr, protection),
            );
        }
        prove(who, action, MergeRequestCtx::new(project, mr))
    }

    async fn approve(
        &self,
        who: UserId,
    ) -> Result<forge_review::approvals::ApprovalAnswer, ForgeError> {
        let principal = session(who);
        let proof = self
            .mr_proof(&principal, Action::MergeRequestApprove)
            .await?;
        self.review
            .approve(
                &request("req-approve"),
                &proof,
                ready(self.repo),
                &oid(&self.head),
            )
            .await
    }

    async fn rule(&self, required: i32, users: &[&str]) {
        let principal = session(self.owner);
        let project = loaded(&self.t, &principal, self.project).await;
        let proof = prove(
            &principal,
            Action::ApprovalRuleManage,
            ProjectCtx::new(project),
        )
        .expect("proof");
        let (_, stored) = self
            .review
            .put_approval_rule(
                &request("req-rule"),
                &proof,
                &RuleAsk {
                    name: "review".to_owned(),
                    target_pattern: Some("refs/heads/main".to_owned()),
                    required_approvals: required,
                    users: users.iter().map(|u| (*u).to_owned()).collect(),
                    groups: Vec::new(),
                },
            )
            .await
            .expect("rule");
        assert_eq!(stored.required_approvals, required);
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

    /// olga (Owner) asks for the merge in her web session.
    async fn ask(&self) -> Result<forge_review::StoredMerge, ForgeError> {
        let who = session(self.owner);
        let proof = self.mr_proof(&who, Action::MergeRequestMerge).await?;
        let credential = CredentialRef::Session(self.session_of(self.owner).await);
        self.review
            .request_merge(
                &request("req-merge"),
                credential,
                &proof,
                ready(self.repo),
                &MergeAsk {
                    sha: oid(&self.head),
                    method: None,
                    delete_source_branch: Some(false),
                    message: None,
                },
            )
            .await
    }

    fn tip(&self, branch_name: &str) -> String {
        plumb(
            &self.path,
            &["rev-parse", &format!("refs/heads/{branch_name}")],
            None,
        )
    }
}

/// ADR-0024 §8.3, §12: the author is refused, administrators included
/// (FG-REVIEW-0013), before anything is read or written.
#[tokio::test]
async fn the_author_cannot_approve_their_own_merge_request() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let e = w.approve(w.dave).await.expect_err("self approval");
    assert_eq!(e.code(), ErrorCode::SelfApprovalRefused);
    w.t.drop_now().await;
}

/// The real-git end to end of plan 11 §5.3 R5: with a rule needing one
/// approval, the merge is blocked until rita approves; then it lands.
#[tokio::test]
async fn an_approval_lets_the_merge_land() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.rule(1, &[]).await;
    let blocked = w.ask().await.expect_err("blocked");
    assert_eq!(blocked.code(), ErrorCode::MergeBlocked);
    assert_eq!(reasons(&blocked), "approvals_missing");
    assert!(
        detail(&blocked).contains("rule=review required=1 counted=0"),
        "{}",
        detail(&blocked)
    );
    let answer = w.approve(w.rita).await.expect("approve");
    assert_eq!(
        answer.outcome,
        ApproveOutcome::Approved {
            counted: true,
            uncounted_reason: None
        }
    );
    let state = w
        .review
        .approval_state(
            &RequestId::parse("req-state").expect("id"),
            &w.mr_proof(&session(w.rita), Action::MergeRequestRead)
                .await
                .expect("proof"),
            ready(w.repo),
        )
        .await
        .expect("state");
    let report = state.report.expect("report");
    assert!(report.blocks.is_empty(), "{:?}", report.blocks);
    let attempt = w.ask().await.expect("queued");
    let done = w.merger.execute(attempt.id).await.expect("merge");
    assert_eq!(done.outcome, "applied");
    assert_ne!(w.tip("main"), w.base);
    w.t.drop_now().await;
}

/// Plan 11 Q4 / ADR-0024 §8.2: a push after the approval ends it; the
/// merge is blocked again.
#[tokio::test]
async fn a_push_after_the_approval_blocks_the_merge() {
    let mut w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.rule(1, &[]).await;
    w.approve(w.rita).await.expect("approve");
    let next = commit(
        &w.path,
        &[("README.md", "base\n"), ("f.txt", "f2\n")],
        Some(&w.head),
    );
    plumb(&w.path, &["update-ref", "refs/heads/feature", &next], None);
    w.record_push(
        w.dave,
        "feature",
        Some(&w.head.clone()),
        &next,
        "req-push-2",
    )
    .await;
    w.head = next;
    w.checked().await;
    let blocked = w.ask().await.expect_err("blocked");
    assert_eq!(blocked.code(), ErrorCode::MergeBlocked);
    assert!(
        reasons(&blocked).contains("approvals_missing"),
        "{}",
        reasons(&blocked)
    );
    let revoked: i64 = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.events WHERE event_type = 'merge_request.unapproved' \
         AND payload ->> 'reason' = 'new_commits'",
    )
    .await;
    assert_eq!(revoked, 1);
    w.t.drop_now().await;
}

/// ADR-0024 §8.3: rita pushed a commit of the range (to another branch,
/// before the merge request was opened); her approval is accepted but not
/// counted (`committer`), so the merge stays blocked.
#[tokio::test]
async fn a_committer_approval_is_not_counted() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.record_push(w.rita, "rita-copy", None, &w.head.clone(), "req-push-rita")
        .await;
    w.rule(1, &[]).await;
    let answer = w.approve(w.rita).await.expect("approve");
    assert_eq!(
        answer.outcome,
        ApproveOutcome::Approved {
            counted: false,
            uncounted_reason: Some(UncountedReason::Committer)
        }
    );
    let blocked = w.ask().await.expect_err("blocked");
    assert_eq!(reasons(&blocked), "approvals_missing");
    // Mara pushed nothing: hers counts.
    w.approve(w.mara).await.expect("approve");
    w.ask().await.expect("queued");
    w.t.drop_now().await;
}

/// ADR-0024 §9: an unknown owner on the target's CODEOWNERS blocks every
/// merge with its line (FG-REVIEW-0014 in the log).
#[tokio::test]
async fn a_codeowners_file_with_an_unknown_owner_blocks_the_merge() {
    let w = world(
        &[
            ("README.md", "base\n"),
            ("CODEOWNERS", "# owners\n* @ghost\n"),
        ],
        &[
            ("README.md", "base\n"),
            ("CODEOWNERS", "# owners\n* @ghost\n"),
            ("f.txt", "f\n"),
        ],
    )
    .await;
    w.approve(w.rita).await.expect("approve");
    let blocked = w.ask().await.expect_err("blocked");
    assert_eq!(reasons(&blocked), "codeowners_invalid");
    assert!(
        detail(&blocked).contains("line=2 reason=unknown_owner"),
        "{}",
        detail(&blocked)
    );
    w.t.drop_now().await;
}

/// ADR-0024 §9, §8.7: CODEOWNERS comes from the target's tip, never from
/// the merge request (which rewrites it to name rita), and changing it
/// needs a Maintainer.
#[tokio::test]
async fn codeowners_are_read_from_the_target_never_the_head() {
    let w = world(
        &[("README.md", "base\n"), ("CODEOWNERS", "*.txt @mara\n")],
        &[
            ("README.md", "base\n"),
            ("CODEOWNERS", "*.txt @rita\n"),
            ("f.txt", "f\n"),
        ],
    )
    .await;
    w.approve(w.rita).await.expect("approve");
    let blocked = w.ask().await.expect_err("blocked");
    assert_eq!(
        reasons(&blocked),
        "codeowner_approval_missing,config_change_needs_maintainer_approval"
    );
    assert!(
        detail(&blocked).contains("pattern=*.txt owners=@mara"),
        "{}",
        detail(&blocked)
    );
    w.approve(w.mara).await.expect("approve");
    let attempt = w.ask().await.expect("queued");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    w.t.drop_now().await;
}

/// ADR-0024 §4.2 steps 3 and 5: the job decides again at execution time —
/// an approval withdrawn after the request supersedes the attempt and no
/// ref moves.
#[tokio::test]
async fn the_merge_job_evaluates_the_approvals_again() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.rule(1, &[]).await;
    w.approve(w.rita).await.expect("approve");
    let attempt = w.ask().await.expect("queued");
    let proof = w
        .mr_proof(&session(w.rita), Action::MergeRequestUnapprove)
        .await
        .expect("proof");
    let answer = w
        .review
        .unapprove(&request("req-unapprove"), &proof)
        .await
        .expect("unapprove");
    assert_eq!(answer.outcome, ApproveOutcome::Withdrawn);
    let report = w.merger.execute(attempt.id).await.expect("job");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::MergeBlocked));
    assert_eq!(w.tip("main"), w.base);
    w.t.drop_now().await;
}

/// A rule change changes the decision at once (ADR-0024 §8.6): a rule
/// naming only mara makes rita's approval not count.
#[tokio::test]
async fn a_rule_change_changes_evaluate_merge() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.approve(w.rita).await.expect("approve");
    w.rule(1, &[]).await;
    assert!(w.ask().await.is_ok(), "rita counts for an open rule");
    w.t.drop_now().await;
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.approve(w.rita).await.expect("approve");
    w.rule(1, &["mara"]).await;
    let blocked = w.ask().await.expect_err("blocked");
    assert_eq!(reasons(&blocked), "approvals_missing");
    w.t.drop_now().await;
}

/// ADR-0024 §8.4: an agent's token never approves (the people-only arm).
#[tokio::test]
async fn an_agent_token_cannot_approve() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let agent = forge_auth::Principal::authenticated(
        w.rita,
        false,
        true,
        forge_auth::Credential::Token(vec![forge_core::Scope::MergeApprove]),
        forge_auth::AccountKind::Human,
    )
    .as_agent(true);
    let e = w
        .mr_proof(&agent, Action::MergeRequestApprove)
        .await
        .expect_err("agent");
    assert_eq!(e.code(), ErrorCode::Forbidden);
    w.t.drop_now().await;
}

/// R5 review HIGH: rita (playing Bob) pushes 21 commits in one push whose
/// list holds only the 20 newest; the oldest is the merge request's head,
/// which dave's own record names. Rita's cut push reaches it, so she is a
/// committer and her approval does not count.
#[tokio::test]
async fn a_cut_push_reaching_the_range_makes_its_pusher_a_committer() {
    let w = world(
        &[(
            "README.md",
            "base
",
        )],
        &[
            (
                "README.md",
                "base
",
            ),
            (
                "f.txt", "f
",
            ),
        ],
    )
    .await;
    let mut chain = vec![w.head.clone()];
    for n in 2..=21 {
        let parent = chain.last().expect("parent").clone();
        chain.push(commit(
            &w.path,
            &[
                (
                    "README.md",
                    "base
",
                ),
                (
                    "f.txt",
                    &format!(
                        "f{n}
"
                    ),
                ),
            ],
            Some(&parent),
        ));
    }
    let tip = chain.last().expect("tip").clone();
    plumb(&w.path, &["update-ref", "refs/heads/bob", &tip], None);
    let listed: Vec<&str> = chain.iter().rev().take(20).map(String::as_str).collect();
    w.record_push_listing(w.rita, "bob", (None, &tip), (&listed, 21), "req-push-bob")
        .await;
    w.rule(1, &[]).await;
    let answer = w.approve(w.rita).await.expect("approve");
    assert_eq!(
        answer.outcome,
        ApproveOutcome::Approved {
            counted: false,
            uncounted_reason: Some(UncountedReason::Committer)
        }
    );
    w.approve(w.mara).await.expect("approve");
    w.ask().await.expect("mara counts");
    w.t.drop_now().await;
}

/// R5 review MEDIUM: a criss-cross history has two merge bases; the paths
/// the merge changes are the target tip against the trial merge's tree,
/// so a path already on the target (b.txt) needs none of its owners.
#[tokio::test]
async fn criss_cross_histories_count_only_what_the_merge_changes() {
    let codeowners = (
        "CODEOWNERS",
        "b.txt @mara
c.txt @rita
",
    );
    let mut w = world(
        &[
            (
                "README.md",
                "base
",
            ),
            codeowners,
        ],
        &[
            (
                "README.md",
                "base
",
            ),
            codeowners,
            (
                "f.txt", "f
",
            ),
        ],
    )
    .await;
    let base = w.base.clone();
    let m1 = commit(
        &w.path,
        &[
            (
                "README.md",
                "base
",
            ),
            codeowners,
            (
                "a.txt", "a
",
            ),
        ],
        Some(&base),
    );
    let f1 = commit(
        &w.path,
        &[
            (
                "README.md",
                "base
",
            ),
            codeowners,
            (
                "b.txt", "b
",
            ),
        ],
        Some(&base),
    );
    let both = [
        (
            "README.md",
            "base
",
        ),
        codeowners,
        (
            "a.txt", "a
",
        ),
        (
            "b.txt", "b
",
        ),
    ];
    let m2 = merge_commit(&w.path, &both, &[&m1, &f1]);
    let f2_files = [
        (
            "README.md",
            "base
",
        ),
        codeowners,
        (
            "a.txt", "a
",
        ),
        (
            "b.txt", "b
",
        ),
        (
            "c.txt", "c
",
        ),
    ];
    let f2 = merge_commit(&w.path, &f2_files, &[&f1, &m1]);
    let bases = plumb(&w.path, &["merge-base", "--all", &m2, &f2], None);
    assert_eq!(bases.lines().count(), 2, "criss-cross: {bases}");
    plumb(&w.path, &["update-ref", "refs/heads/main", &m2], None);
    plumb(&w.path, &["update-ref", "refs/heads/feature", &f2], None);
    w.record_push(w.dave, "feature", None, &f2, "req-push-f2")
        .await;
    w.head = f2;
    w.checked().await;
    // dave wrote c.txt; rita (its owner) approves; nobody owning b.txt.
    w.approve(w.rita).await.expect("approve");
    let state = w
        .review
        .approval_state(
            &RequestId::parse("req-state").expect("id"),
            &w.mr_proof(&session(w.rita), Action::MergeRequestRead)
                .await
                .expect("proof"),
            ready(w.repo),
        )
        .await
        .expect("state");
    let report = state.report.expect("report");
    assert!(report.blocks.is_empty(), "{:?}", report.blocks);
    assert_eq!(
        report.codeowners.len(),
        1,
        "c.txt only: {:?}",
        report.codeowners
    );
    w.t.drop_now().await;
}

/// R5 review MEDIUM: a rule naming a missing user and one naming a disabled
/// user get the same answer (no existence oracle); a CODEOWNERS owner that
/// is disabled is `unknown_owner` like a missing one.
#[tokio::test]
async fn missing_and_disabled_accounts_answer_alike() {
    let w = world(
        &[("README.md", "base\n"), ("CODEOWNERS", "* @mara\n")],
        &[
            ("README.md", "base\n"),
            ("CODEOWNERS", "* @mara\n"),
            ("f.txt", "f\n"),
        ],
    )
    .await;
    let principal = session(w.owner);
    let project = loaded(&w.t, &principal, w.project).await;
    let proof = prove(
        &principal,
        Action::ApprovalRuleManage,
        ProjectCtx::new(project),
    )
    .expect("proof");
    let ask = |user: &str| RuleAsk {
        name: "review".to_owned(),
        target_pattern: None,
        required_approvals: 1,
        users: vec![user.to_owned()],
        groups: Vec::new(),
    };
    let missing = w
        .review
        .put_approval_rule(&request("req-rule-ghost"), &proof, &ask("ghost"))
        .await
        .expect_err("missing");
    let _disabled = forge_db::users::disable(&w.t.db, &system("req-disable"), w.mara)
        .await
        .expect("disable");
    let disabled = w
        .review
        .put_approval_rule(&request("req-rule-mara"), &proof, &ask("mara"))
        .await
        .expect_err("disabled");
    assert_eq!(missing.code(), ErrorCode::RequestInvalid);
    assert_eq!(missing.context(), disabled.context(), "the same answer");
    w.approve(w.rita).await.expect("approve");
    let blocked = w.ask().await.expect_err("blocked");
    assert!(
        detail(&blocked).contains("reason=unknown_owner"),
        "{}",
        detail(&blocked)
    );
    w.t.drop_now().await;
}

impl World {
    /// One push record of `updates` (ref, new) by `who`, each listing its
    /// new commit (`total` new commits each: more than one cuts the list).
    async fn record_push_refs(
        &self,
        who: UserId,
        updates: &[(String, String)],
        total: u64,
        request_id: &str,
    ) {
        let zero = "0".repeat(40);
        let push = PushRecord {
            repository_id: self.repo,
            updates: updates
                .iter()
                .map(|(name, new)| PushedRef {
                    name: RefName::parse(&format!("refs/heads/{name}")).expect("ref"),
                    old: oid(&zero),
                    new: oid(new),
                    kind: PushUpdateKind::Create,
                    commits: vec![oid(new)],
                    total_commits: total,
                    total_commits_partial: false,
                })
                .collect(),
        };
        let ctx = ChangeContext::new(Actor::User(who), RequestId::parse(request_id).expect("id"));
        let recorded = repositories::record_push(&self.t.db, &ctx, &push)
            .await
            .expect("push record");
        assert_eq!(recorded, forge_core::PushRecorded::Recorded);
    }

    async fn state(&self) -> forge_review::approvals::ApprovalState {
        self.review
            .approval_state(
                &RequestId::parse("req-state").expect("id"),
                &self
                    .mr_proof(&session(self.rita), Action::MergeRequestRead)
                    .await
                    .expect("proof"),
                ready(self.repo),
            )
            .await
            .expect("state")
    }
}

/// R5 re-review round 2 (HIGH): rita pushes 101 refs in one push; the last
/// one (by name) points at the merge request's head, so the record's list
/// of 100 refs leaves it out. dave's own record names the head. Rita's
/// approval must not count.
#[tokio::test]
async fn a_ref_beyond_the_listed_hundred_still_makes_a_committer() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let mut updates: Vec<(String, String)> = (0..100)
        .map(|n| (format!("a{n:03}"), w.base.clone()))
        .collect();
    updates.push(("zzz".to_owned(), w.head.clone()));
    w.record_push_refs(w.rita, &updates, 1, "req-push-many-refs")
        .await;
    w.rule(1, &[]).await;
    let answer = w.approve(w.rita).await.expect("approve");
    assert_eq!(
        answer.outcome,
        ApproveOutcome::Approved {
            counted: false,
            uncounted_reason: Some(UncountedReason::Committer)
        }
    );
    w.t.drop_now().await;
}

/// R5 re-review round 2 (HIGH): 41 large (cut) pushes of unrelated
/// branches cost one git call, not one per push: the state route's budget
/// (32 calls) holds and rita's approval counts.
#[tokio::test]
async fn many_large_pushes_do_not_exhaust_the_budget() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    for n in 0..41 {
        let side = commit(
            &w.path,
            &[("README.md", "base\n"), ("side.txt", &format!("{n}\n"))],
            Some(&w.base),
        );
        plumb(
            &w.path,
            &["update-ref", &format!("refs/heads/side{n}"), &side],
            None,
        );
        w.record_push_refs(
            w.dave,
            &[(format!("side{n}"), side)],
            25,
            &format!("req-push-side-{n}"),
        )
        .await;
    }
    w.rule(1, &[]).await;
    w.approve(w.rita).await.expect("approve");
    let report = w.state().await.report.expect("report");
    assert!(report.blocks.is_empty(), "{:?}", report.blocks);
    assert!(report.approvals[0].counted);
    w.t.drop_now().await;
}

/// R5 re-review round 2 (HIGH): a changed path over 4 KiB is never matched:
/// `changed_paths_unknown reason=path_too_long`.
#[tokio::test]
async fn a_changed_path_over_four_kib_fails_closed() {
    let long = "a".repeat(4097);
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), (long.as_str(), "x\n")],
    )
    .await;
    let report = w.state().await.report.expect("report");
    assert!(
        report
            .blocks
            .iter()
            .any(|b| b.detail().as_deref() == Some("reason=path_too_long")),
        "{:?}",
        report.blocks
    );
    w.t.drop_now().await;
}

/// R5 re-review round 2 (MEDIUM): the approval state says when the head no
/// longer merges cleanly, so a conflicting merge request never looks
/// approved.
#[tokio::test]
async fn the_approval_state_reports_a_conflict() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    assert!(!w.state().await.conflict);
    let clash = commit(
        &w.path,
        &[("README.md", "base\n"), ("f.txt", "clash\n")],
        Some(&w.base),
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &clash], None);
    w.browser.invalidate(w.repo);
    assert!(w.state().await.conflict);
    w.t.drop_now().await;
}
