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
    world_with(main_files, head_files, true).await
}

/// [`world`]; `record_head`: false leaves the head without a push record
/// (an `update-ref` import: no `push_commits` row).
async fn world_with(
    main_files: &[(&str, &str)],
    head_files: &[(&str, &str)],
    record_head: bool,
) -> World {
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
    if record_head {
        w0.record_push(dave, "feature", None, &w0.head.clone(), "req-push-dave")
            .await;
    }
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
        author_name: forge_review::UserName {
            username: String::new(),
            display_name: String::new(),
        },
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
        // U2 follow-up: the put names the rule as read now (create: none).
        let expected = forge_db::review::approval_rules(&self.t.db, self.project)
            .await
            .expect("rules")
            .into_iter()
            .find(|r| r.name == "review")
            .map(|r| r.updated_at);
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
                expected.as_deref(),
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
        .unapprove(&request("req-unapprove"), &proof, None)
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
        .put_approval_rule(&request("req-rule-ghost"), &proof, &ask("ghost"), None)
        .await
        .expect_err("missing");
    let _disabled = forge_db::users::disable(&w.t.db, &system("req-disable"), w.mara)
        .await
        .expect("disable");
    let disabled = w
        .review
        .put_approval_rule(&request("req-rule-mara"), &proof, &ask("mara"), None)
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

impl World {
    /// One push record of `updates`, each `(ref, old, new)` with `new` all
    /// zeros for a delete.
    async fn record_push_raw(
        &self,
        who: UserId,
        updates: &[(String, String, String)],
        request_id: &str,
    ) {
        let zero = "0".repeat(40);
        let push = PushRecord {
            repository_id: self.repo,
            updates: updates
                .iter()
                .map(|(name, old, new)| {
                    let delete = *new == zero;
                    PushedRef {
                        name: RefName::parse(name).expect("ref"),
                        old: oid(old),
                        new: oid(new),
                        kind: if delete {
                            PushUpdateKind::Delete
                        } else {
                            PushUpdateKind::Create
                        },
                        commits: Vec::new(),
                        total_commits: 0,
                        total_commits_partial: false,
                    }
                })
                .collect(),
        };
        let ctx = ChangeContext::new(Actor::User(who), RequestId::parse(request_id).expect("id"));
        let recorded = repositories::record_push(&self.t.db, &ctx, &push)
            .await
            .expect("push record");
        assert_eq!(recorded, forge_core::PushRecorded::Recorded);
    }
}

/// R5 round 3 (HIGH): deleting 101 branches in one push leaves the record's
/// ref list cut with nothing to keep (deletions reach nothing); it must not
/// make every merge request's committers unknown.
#[tokio::test]
async fn deleting_many_branches_does_not_block_approvals() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let zero = "0".repeat(40);
    let deletes: Vec<(String, String, String)> = (0..101)
        .map(|n| {
            (
                format!("refs/heads/old{n:03}"),
                w.base.clone(),
                zero.clone(),
            )
        })
        .collect();
    w.record_push_raw(w.dave, &deletes, "req-push-deletes")
        .await;
    w.rule(1, &[]).await;
    w.approve(w.rita).await.expect("approve");
    let report = w.state().await.report.expect("report");
    assert!(report.blocks.is_empty(), "{:?}", report.blocks);
    w.t.drop_now().await;
}

/// R5 round 3 (HIGH): twelve pushes of 1 000 tags each (unlisted rows past
/// 10 000) on commits of the target must not block approvals.
#[tokio::test]
async fn many_tags_do_not_block_approvals() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let zero = "0".repeat(40);
    for p in 0..12 {
        let tags: Vec<(String, String, String)> = (0..1000)
            .map(|n| (format!("refs/tags/v{p}.{n}"), zero.clone(), w.base.clone()))
            .collect();
        w.record_push_raw(w.dave, &tags, &format!("req-push-tags-{p}"))
            .await;
    }
    w.rule(1, &[]).await;
    w.approve(w.rita).await.expect("approve");
    let report = w.state().await.report.expect("report");
    assert!(report.blocks.is_empty(), "{:?}", report.blocks);
    w.t.drop_now().await;
}

// ── M3 R5 round 4: the committer rule reads push_commits ──

impl World {
    /// One push record of `updates` (`(ref, old, new)`) by `who`, with the
    /// commits git would list as introduced (`introduced`, `complete`):
    /// what the push path records (forge-git's listing is tested on its
    /// own).
    async fn record_introduced(
        &self,
        who: UserId,
        updates: &[(String, String, String)],
        introduced: &[String],
        complete: bool,
        request_id: &str,
    ) {
        let zero = "0".repeat(40);
        let push = PushRecord {
            repository_id: self.repo,
            updates: updates
                .iter()
                .map(|(name, old, new)| PushedRef {
                    name: RefName::parse(name).expect("ref"),
                    old: oid(old),
                    new: oid(new),
                    kind: if *new == zero {
                        PushUpdateKind::Delete
                    } else if *old == zero {
                        PushUpdateKind::Create
                    } else {
                        PushUpdateKind::FastForward
                    },
                    commits: Vec::new(),
                    total_commits: 0,
                    total_commits_partial: false,
                })
                .collect(),
        };
        let commits = forge_core::PushedCommits {
            commits: introduced.iter().map(|c| oid(c)).collect(),
            complete,
        };
        let ctx = ChangeContext::new(Actor::User(who), RequestId::parse(request_id).expect("id"));
        let recorded = repositories::record_push_with_commits(&self.t.db, &ctx, &push, &commits)
            .await
            .expect("push record");
        assert_eq!(recorded, forge_core::PushRecorded::Recorded);
    }

    /// B1 = the merge request's head; B2..B21 on top, pushed by `who` to
    /// `bob` in one push (introducing all 21).
    async fn bob_pushes_b1_to_b21(&self, who: UserId) -> Vec<String> {
        let mut chain = vec![self.head.clone()];
        for n in 2..=21 {
            let parent = chain.last().expect("parent").clone();
            chain.push(commit(
                &self.path,
                &[("README.md", "base\n"), ("f.txt", &format!("f{n}\n"))],
                Some(&parent),
            ));
        }
        let tip = chain.last().expect("tip").clone();
        plumb(&self.path, &["update-ref", "refs/heads/bob", &tip], None);
        let zero = "0".repeat(40);
        self.record_introduced(
            who,
            &[("refs/heads/bob".to_owned(), zero, tip)],
            &chain,
            true,
            "req-push-bob",
        )
        .await;
        chain
    }
}

/// R5 round 4: Bob pushes B1..B21 in one push; Alice creates `feature` at
/// B1 (no new commit, no row). B1's row names Bob: his approval does not
/// count. After the `bob` branch is deleted and its objects pruned, the
/// rows (and the answer) stay.
#[tokio::test]
async fn a_pusher_of_a_range_commit_is_a_committer_whatever_ref_names_it() {
    let w = world_with(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
        false,
    )
    .await;
    let _chain = w.bob_pushes_b1_to_b21(w.rita).await;
    let zero = "0".repeat(40);
    w.record_introduced(
        w.dave,
        &[(
            "refs/heads/feature".to_owned(),
            zero.clone(),
            w.head.clone(),
        )],
        &[],
        true,
        "req-push-alice",
    )
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
    // Prune: delete `bob`, drop its unreachable objects; the answer is the
    // same (no object is read).
    plumb(&w.path, &["update-ref", "-d", "refs/heads/bob"], None);
    plumb(
        &w.path,
        &["reflog", "expire", "--expire=now", "--all"],
        None,
    );
    plumb(&w.path, &["prune", "--expire=now"], None);
    let report = w.state().await.report.expect("report");
    assert_eq!(
        report.approvals[0].uncounted_reason,
        Some(UncountedReason::Committer)
    );
    w.t.drop_now().await;
}

/// R5 round 4: 101 refs in one push, the head on the last one (past the
/// audit row's 100-ref list): the push introduced the head, so its pusher
/// is a committer.
#[tokio::test]
async fn a_ref_past_the_listed_hundred_still_makes_a_committer() {
    let w = world_with(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
        false,
    )
    .await;
    let zero = "0".repeat(40);
    let mut updates: Vec<(String, String, String)> = (0..100)
        .map(|n| (format!("refs/heads/a{n:03}"), zero.clone(), w.base.clone()))
        .collect();
    updates.push(("refs/heads/zzz".to_owned(), zero, w.head.clone()));
    w.record_introduced(
        w.rita,
        &updates,
        std::slice::from_ref(&w.head),
        true,
        "req-push-many",
    )
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

/// R5 round 4: a commit no push recorded (an `update-ref` import, history
/// before push_commits) makes the committers unknown: no approval counts.
#[tokio::test]
async fn an_imported_commit_makes_the_committers_unknown() {
    let w = world_with(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
        false,
    )
    .await;
    w.rule(1, &[]).await;
    let answer = w.approve(w.rita).await.expect("approve");
    assert_eq!(
        answer.outcome,
        ApproveOutcome::Approved {
            counted: false,
            uncounted_reason: Some(UncountedReason::CommittersUnknown)
        }
    );
    w.t.drop_now().await;
}

/// R5 round 4: an unrelated orphan branch of 51 000 commits (rows only:
/// the rule never walks history) changes nothing for other merge requests.
#[tokio::test]
async fn a_huge_unrelated_push_does_not_block_other_merge_requests() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let orphan: Vec<String> = (0..51_000u64).map(|n| format!("{n:040x}")).collect();
    let tip = orphan.last().expect("tip").clone();
    let zero = "0".repeat(40);
    w.record_introduced(
        w.dave,
        &[("refs/heads/orphan".to_owned(), zero, tip)],
        &orphan,
        true,
        "req-push-orphan",
    )
    .await;
    w.rule(1, &[]).await;
    w.approve(w.rita).await.expect("approve");
    let report = w.state().await.report.expect("report");
    assert!(report.blocks.is_empty(), "{:?}", report.blocks);
    w.t.drop_now().await;
}

/// R5 round 4: a push past its per-push cap is incomplete — marked on its
/// record — and blocks only the merge requests whose range holds one of
/// its unlisted commits.
#[tokio::test]
async fn an_incomplete_push_blocks_only_ranges_holding_its_unlisted_commits() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let zero = "0".repeat(40);
    let other = commit(
        &w.path,
        &[("README.md", "base\n"), ("o.txt", "o\n")],
        Some(&w.base),
    );
    w.record_introduced(
        w.mara,
        &[("refs/heads/other".to_owned(), zero, other)],
        &[],
        false,
        "req-push-incomplete",
    )
    .await;
    let marked: i64 = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.audit_log WHERE request_id = 'req-push-incomplete' \
         AND (details ->> 'push_commits_incomplete')::boolean",
    )
    .await;
    assert_eq!(marked, 1);
    // This merge request's range is dave's commit, recorded: unaffected.
    w.rule(1, &[]).await;
    w.approve(w.rita).await.expect("approve");
    let report = w.state().await.report.expect("report");
    assert!(report.blocks.is_empty(), "{:?}", report.blocks);
    w.t.drop_now().await;
}

/// R5 round 4: tag churn — creating and deleting thousands of tags on
/// commits that already exist — writes no row and blocks nothing.
#[tokio::test]
async fn tag_churn_does_not_block_approvals() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let zero = "0".repeat(40);
    for round in 0..3 {
        let create: Vec<(String, String, String)> = (0..1000)
            .map(|n| {
                (
                    format!("refs/tags/c{round}.{n}"),
                    zero.clone(),
                    w.base.clone(),
                )
            })
            .collect();
        w.record_introduced(w.dave, &create, &[], true, &format!("req-tags-{round}"))
            .await;
        let delete: Vec<(String, String, String)> = (0..1000)
            .map(|n| {
                (
                    format!("refs/tags/c{round}.{n}"),
                    w.base.clone(),
                    zero.clone(),
                )
            })
            .collect();
        w.record_introduced(w.dave, &delete, &[], true, &format!("req-untags-{round}"))
            .await;
    }
    let rows: i64 = count(&w.t, "SELECT count(*) AS n FROM forge.push_commits").await;
    assert_eq!(rows, 1, "only dave's head");
    w.rule(1, &[]).await;
    w.approve(w.rita).await.expect("approve");
    let report = w.state().await.report.expect("report");
    assert!(report.blocks.is_empty(), "{:?}", report.blocks);
    w.t.drop_now().await;
}

impl World {
    /// olga asks for a merge by `method`.
    async fn ask_by(
        &self,
        method: forge_core::MergeMethod,
    ) -> Result<forge_review::StoredMerge, ForgeError> {
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
                    method: Some(method),
                    delete_source_branch: Some(false),
                    message: None,
                },
            )
            .await
    }

    /// The pushers recorded for `commit`, sorted.
    async fn pushers_of(&self, commit: &str) -> Vec<i64> {
        let rows = self
            .t
            .json_rows(&format!(
                "SELECT pusher_id AS n FROM forge.push_commits WHERE commit_sha = '{commit}' \
                 ORDER BY pusher_id"
            ))
            .await;
        rows.iter()
            .filter_map(|r| r.get("n").and_then(serde_json::Value::as_i64))
            .collect()
    }

    /// mara opens `source` → `target` as the world's merge request.
    async fn open_next(&mut self, source: &str, target: &str) {
        let proof = open_proof(
            &self.t,
            &session(self.mara),
            self.project,
            (source, target),
            false,
        )
        .await
        .expect("open proof");
        self.mr = self
            .review
            .open(
                &request("req-open-next"),
                &proof,
                ready(self.repo),
                &NewFields {
                    title: "Release".to_owned(),
                    description: String::new(),
                },
            )
            .await
            .expect("open");
        self.head = self.tip(source);
        self.checked().await;
    }
}

/// R5 review round 5: a squash commit is attributed to the requester and
/// to every pusher of the merged range — after olga squashes dave's merge
/// request, a main → release merge request holding the squash commit counts
/// dave as a committer (his approval does not count).
#[tokio::test]
async fn a_squash_commit_keeps_the_range_pushers_as_committers() {
    let mut w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    plumb(
        &w.path,
        &["update-ref", "refs/heads/release", &w.base],
        None,
    );
    let attempt = w
        .ask_by(forge_core::MergeMethod::Squash)
        .await
        .expect("queued");
    let done = w.merger.execute(attempt.id).await.expect("merge");
    assert_eq!(done.outcome, "applied");
    let squash = w.tip("main");
    assert_ne!(squash, w.head, "a new commit");
    let mut want = vec![w.dave.get(), w.owner.get()];
    want.sort_unstable();
    assert_eq!(w.pushers_of(&squash).await, want);
    w.browser.invalidate(w.repo);
    w.open_next("main", "release").await;
    let dave = w.approve(w.dave).await.expect("approve");
    assert_eq!(
        dave.outcome,
        ApproveOutcome::Approved {
            counted: false,
            uncounted_reason: Some(UncountedReason::Committer)
        }
    );
    let rita = w.approve(w.rita).await.expect("approve");
    assert_eq!(
        rita.outcome,
        ApproveOutcome::Approved {
            counted: true,
            uncounted_reason: None
        }
    );
    w.t.drop_now().await;
}

/// R5 review round 5: a merge commit is attributed like a squash commit;
/// when the range's pushers are unknown (a commit with no row) the merge
/// commit gets no row at all (fail closed: a later range holding it has
/// unknown committers, never just the requester).
#[tokio::test]
async fn a_merge_commit_is_attributed_to_the_range_pushers_or_to_no_one() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let attempt = w
        .ask_by(forge_core::MergeMethod::MergeCommit)
        .await
        .expect("queued");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    let mut want = vec![w.dave.get(), w.owner.get()];
    want.sort_unstable();
    assert_eq!(w.pushers_of(&w.tip("main")).await, want);
    w.t.drop_now().await;

    let w = world_with(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
        false,
    )
    .await;
    let attempt = w
        .ask_by(forge_core::MergeMethod::Squash)
        .await
        .expect("queued");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    assert!(w.pushers_of(&w.tip("main")).await.is_empty());
    w.t.drop_now().await;
}

// ── M3 R6: merge conditions (ADR-0024 §7, §8.2, §8.6, §9, §10, §12) ──

impl World {
    /// Protect `main` at `role` with a merge rule (written as the
    /// protected-ref routes write it: the rule and its merge rule).
    async fn protect(
        &self,
        role: &str,
        approvals: i32,
        checks: &[&str],
        talks: bool,
        no_push: bool,
    ) {
        let checks: Vec<String> = checks.iter().map(|c| format!("'{c}'")).collect();
        self.t
            .commit_audited(
                ("project", self.project.get()),
                &[
                    format!(
                        "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
                         VALUES ({}, 'refs/heads/main', '{role}')",
                        self.project.get()
                    ),
                    format!(
                        "INSERT INTO forge.protected_ref_merge_rules (protected_ref_id, \
                         project_id, required_approvals, required_checks, \
                         require_resolved_discussions, direct_push_forbidden) \
                         SELECT id, project_id, {approvals}, ARRAY[{}]::text[], {talks}, {no_push} \
                         FROM forge.protected_refs WHERE project_id = {}",
                        checks.join(","),
                        self.project.get()
                    ),
                ],
            )
            .await;
    }

    /// The project's merge settings (as `PUT …/merge-settings` writes them).
    async fn settings(&self, values: forge_core::MergeSettingsValues) {
        let principal = session(self.owner);
        let project = loaded(&self.t, &principal, self.project).await;
        let proof = prove(
            &principal,
            Action::MergeSettingsManage,
            forge_auth::MergeSettingsCtx::new(project, None, true),
        )
        .expect("proof");
        // U2: the write names the settings it replaces.
        let current = self
            .review
            .stored_merge_settings(project.id)
            .await
            .expect("current settings");
        let (outcome, _) = self
            .review
            .put_merge_settings(
                &request("req-settings"),
                &proof,
                &values,
                current.updated_at.as_deref(),
            )
            .await
            .expect("settings");
        assert_eq!(outcome, forge_db::review::SettingsOutcome::Changed);
    }

    async fn status(&self) -> forge_review::MergeStatus {
        let principal = session(self.owner);
        let proof = self
            .mr_proof(&principal, Action::MergeRequestRead)
            .await
            .expect("proof");
        self.review
            .merge_status(
                &RequestId::parse("req-status").expect("id"),
                &proof,
                ready(self.repo),
                None,
            )
            .await
            .expect("status")
    }

    /// dave pushes `files` on top of the head (recorded like a push).
    async fn push_next(&mut self, files: &[(&str, &str)], request_id: &str) {
        let next = commit(&self.path, files, Some(&self.head));
        plumb(
            &self.path,
            &["update-ref", "refs/heads/feature", &next],
            None,
        );
        self.record_push(
            self.dave,
            "feature",
            Some(&self.head.clone()),
            &next,
            request_id,
        )
        .await;
        self.head = next;
        self.checked().await;
    }

    async fn toggle(
        &self,
        change: forge_auth::CodeownersToggle,
    ) -> forge_db::review::StoredMergeSettings {
        let principal = session(self.owner);
        let project = loaded(&self.t, &principal, self.project).await;
        let proof = prove(
            &principal,
            Action::CodeownersEnforcementToggle,
            forge_auth::CodeownersToggleCtx::new(project, None, change),
        )
        .expect("proof");
        self.review
            .toggle_codeowners(&request("req-toggle"), &proof)
            .await
            .expect("toggle")
            .1
    }
}

/// ADR-0024 §8.6, §10, §7: the protected branch's minimum, a required check
/// nobody reported (never success) and every other reason come back at
/// once, ranked; the status says the same.
#[tokio::test]
async fn a_merge_rule_blocks_with_every_reason_and_the_status_says_so() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.protect("developer", 2, &["ci/gates"], true, false).await;
    w.approve(w.rita).await.expect("approve");
    let blocked = w.ask().await.expect_err("blocked");
    assert_eq!(
        reasons(&blocked),
        "approvals_missing,required_check_missing"
    );
    assert!(
        detail(&blocked).contains("rule=@protected_branch required=2 counted=1"),
        "{}",
        detail(&blocked)
    );
    let status = w.status().await;
    assert!(!status.mergeable);
    let names: Vec<&str> = status.reasons.iter().map(|r| r.as_str()).collect();
    assert_eq!(names, ["approvals_missing", "required_check_missing"]);
    assert_eq!(status.discussions, Some(0));
    assert_eq!(status.checks.len(), 1);
    assert!(status.checks[0].run.is_none(), "no check reported");
    w.approve(w.mara).await.expect("approve");
    let status = w.status().await;
    let names: Vec<&str> = status.reasons.iter().map(|r| r.as_str()).collect();
    assert_eq!(names, ["required_check_missing"], "still no check reported");
    w.t.drop_now().await;
}

/// ADR-0024 §4.2 steps 3 and 5: a merge rule added after the request is
/// decided again by the job (superseded, no ref moves).
#[tokio::test]
async fn the_merge_job_decides_a_merge_rule_added_after_the_request() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let attempt = w.ask().await.expect("queued");
    w.protect("developer", 0, &["ci/gates"], false, false).await;
    let report = w.merger.execute(attempt.id).await.expect("job");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::MergeBlocked));
    assert_eq!(w.tip("main"), w.base);
    w.t.drop_now().await;
}

/// The settings are decided again by the job too: a method the project
/// stopped allowing after the request supersedes the attempt
/// (FG-REVIEW-0010).
#[tokio::test]
async fn the_merge_job_decides_the_allowed_method_again() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let attempt = w.ask().await.expect("queued (merge commit, the default)");
    w.settings(forge_core::MergeSettingsValues {
        allowed_methods: vec![forge_core::MergeMethod::Squash],
        default_method: forge_core::MergeMethod::Squash,
        ..forge_core::MergeSettingsValues::default()
    })
    .await;
    let report = w.merger.execute(attempt.id).await.expect("job");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::MergeMethodNotAllowed));
    assert_eq!(w.tip("main"), w.base);
    w.t.drop_now().await;
}

/// ADR-0024 §4.3: the direct-push ban never stops a merge into the branch.
#[tokio::test]
async fn a_branch_without_direct_pushes_still_merges() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.protect("developer", 0, &[], false, true).await;
    let attempt = w.ask().await.expect("queued");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    assert_ne!(w.tip("main"), w.base);
    w.t.drop_now().await;
}

/// ADR-0024 §8.2: a project that keeps approvals on push carries them to
/// a new head that changes no configuration and no CODEOWNERS path; one
/// `merge_request.approved` with `carried_from`, and the merge lands.
#[tokio::test]
async fn a_push_carries_the_approvals_when_the_project_keeps_them() {
    let mut w = world(
        &[("README.md", "base\n"), ("CODEOWNERS", "docs/ @mara\n")],
        &[
            ("README.md", "base\n"),
            ("CODEOWNERS", "docs/ @mara\n"),
            ("f.txt", "f\n"),
        ],
    )
    .await;
    w.settings(forge_core::MergeSettingsValues {
        reset_approvals_on_push: false,
        ..forge_core::MergeSettingsValues::default()
    })
    .await;
    w.rule(1, &[]).await;
    w.approve(w.rita).await.expect("approve");
    w.push_next(
        &[
            ("README.md", "base\n"),
            ("CODEOWNERS", "docs/ @mara\n"),
            ("f.txt", "f2\n"),
        ],
        "req-push-2",
    )
    .await;
    let carried: i64 = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.events WHERE event_type = 'merge_request.approved' \
         AND payload ->> 'carried_from' IS NOT NULL",
    )
    .await;
    assert_eq!(carried, 1);
    let attempt = w.ask().await.expect("the carried approval counts");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    w.t.drop_now().await;
}

/// ADR-0024 §8.2 (c): a new head that touches a path a CODEOWNERS rule of
/// the target covers never carries — the approvals end (`new_commits`).
#[tokio::test]
async fn a_push_touching_an_owned_path_never_carries() {
    let mut w = world(
        &[("README.md", "base\n"), ("CODEOWNERS", "g.md @mara\n")],
        &[
            ("README.md", "base\n"),
            ("CODEOWNERS", "g.md @mara\n"),
            ("f.txt", "f\n"),
        ],
    )
    .await;
    w.settings(forge_core::MergeSettingsValues {
        reset_approvals_on_push: false,
        ..forge_core::MergeSettingsValues::default()
    })
    .await;
    w.approve(w.rita).await.expect("approve");
    w.push_next(
        &[
            ("README.md", "base\n"),
            ("CODEOWNERS", "g.md @mara\n"),
            ("f.txt", "f\n"),
            ("g.md", "g\n"),
        ],
        "req-push-2",
    )
    .await;
    let revoked: i64 = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.events WHERE event_type = 'merge_request.unapproved' \
         AND payload ->> 'reason' = 'new_commits'",
    )
    .await;
    assert_eq!(revoked, 1);
    w.t.drop_now().await;
}

/// ADR-0024 §9 (runbook codeowners): a broken CODEOWNERS blocks every
/// merge; an Owner suspends the enforcement (time-boxed), the merge
/// passes, and enforcing again restores it.
#[tokio::test]
async fn a_suspended_codeowners_enforcement_lets_the_fix_merge() {
    let w = world(
        &[("README.md", "base\n"), ("CODEOWNERS", "* @ghost\n")],
        &[
            ("README.md", "base\n"),
            ("CODEOWNERS", "* @ghost\n"),
            ("f.txt", "f\n"),
        ],
    )
    .await;
    assert_eq!(
        reasons(&w.ask().await.expect_err("blocked")),
        "codeowners_invalid"
    );
    let suspended = w
        .toggle(forge_auth::CodeownersToggle::Suspend { minutes: Some(30) })
        .await;
    assert!(!suspended.codeowners.enforced());
    assert!(
        !w.status()
            .await
            .approvals
            .expect("report")
            .codeowners_enforced
    );
    let attempt = w.ask().await.expect("passes while suspended");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    let back = w.toggle(forge_auth::CodeownersToggle::Enforce).await;
    assert!(back.codeowners.enforced());
    w.t.drop_now().await;
}

/// R6 review (HIGH): a new head that changes more than 50 files never
/// carries the approvals, even when the project keeps them.
#[tokio::test]
async fn a_large_push_never_carries_the_approvals() {
    let mut w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.settings(forge_core::MergeSettingsValues {
        reset_approvals_on_push: false,
        ..forge_core::MergeSettingsValues::default()
    })
    .await;
    w.approve(w.rita).await.expect("approve");
    let names: Vec<String> = (0..51).map(|i| format!("g{i}.txt")).collect();
    let mut files: Vec<(&str, &str)> = vec![("README.md", "base\n"), ("f.txt", "f\n")];
    files.extend(names.iter().map(|n| (n.as_str(), "g\n")));
    w.push_next(&files, "req-push-big").await;
    let carried: i64 = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.events WHERE event_type = 'merge_request.approved' \
         AND payload ->> 'carried_from' IS NOT NULL",
    )
    .await;
    assert_eq!(carried, 0);
    let revoked: i64 = count(
        &w.t,
        "SELECT count(*) AS n FROM forge.events WHERE event_type = 'merge_request.unapproved' \
         AND payload ->> 'reason' = 'new_commits'",
    )
    .await;
    assert_eq!(revoked, 1);
    w.t.drop_now().await;
}

/// R6 review (MEDIUM): loosening the approvals is decided again on the
/// settings as locked — a Maintainer below the project's Owner-level
/// protection is refused (`above_own_role`) even with a proof decided on a
/// stale "tightening"; tightening stays a Maintainer's.
#[tokio::test]
async fn loosening_the_approvals_needs_a_role_at_or_above_every_protection() {
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
    w.protect("owner", 0, &[], false, false).await;
    let principal = session(w.mara);
    let project = loaded(&w.t, &principal, w.project).await;
    let stale = prove(
        &principal,
        Action::MergeSettingsManage,
        forge_auth::MergeSettingsCtx::new(project, Some(Role::Owner), false),
    )
    .expect("a tightening proof");
    let loose = forge_core::MergeSettingsValues {
        committer_approvals_excluded: false,
        ..forge_core::MergeSettingsValues::default()
    };
    let err = w
        .review
        .put_merge_settings(&request("req-loose"), &stale, &loose, None)
        .await
        .expect_err("loosening");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "above_own_role"));
    let tight = forge_core::MergeSettingsValues {
        allowed_methods: vec![forge_core::MergeMethod::Squash],
        default_method: forge_core::MergeMethod::Squash,
        ..forge_core::MergeSettingsValues::default()
    };
    let (outcome, _) = w
        .review
        .put_merge_settings(&request("req-tight"), &stale, &tight, None)
        .await
        .expect("tightening");
    assert_eq!(outcome, forge_db::review::SettingsOutcome::Changed);
    w.t.drop_now().await;
}

/// R6 review (MEDIUM): the status evaluates a fast-forward's ancestry —
/// mergeable while the target is an ancestor of the head, `ff_not_possible`
/// once the target moved elsewhere.
#[tokio::test]
async fn the_status_evaluates_a_fast_forward() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.settings(forge_core::MergeSettingsValues {
        allowed_methods: vec![forge_core::MergeMethod::FastForward],
        default_method: forge_core::MergeMethod::FastForward,
        ..forge_core::MergeSettingsValues::default()
    })
    .await;
    let status = w.status().await;
    assert_eq!(status.method, forge_core::MergeMethod::FastForward);
    assert!(status.mergeable, "{:?}", status.reasons);
    let other = commit(
        &w.path,
        &[("README.md", "base\n"), ("o.txt", "o\n")],
        Some(&w.base),
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &other], None);
    w.browser.invalidate(w.repo);
    let names: Vec<&str> = w
        .status()
        .await
        .reasons
        .iter()
        .map(|r| r.as_str())
        .collect();
    assert!(names.contains(&"ff_not_possible"), "{names:?}");
    w.t.drop_now().await;
}

/// R6 review (LOW): several protection rules matching the target reach
/// evaluate_merge combined to the strictest — the highest minimum, every
/// required check, resolved discussions from either rule.
#[tokio::test]
async fn several_matching_rules_combine_to_the_strictest() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.protect("developer", 1, &["ci/gates"], false, false).await;
    w.t.commit_audited(
        ("project", w.project.get()),
        &[
            format!(
                "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
                 VALUES ({}, 'refs/heads/ma*', 'developer')",
                w.project.get()
            ),
            format!(
                "INSERT INTO forge.protected_ref_merge_rules (protected_ref_id, project_id, \
                 required_approvals, required_checks, require_resolved_discussions) \
                 SELECT id, project_id, 2, ARRAY['kanflow/linked']::text[], true \
                 FROM forge.protected_refs WHERE project_id = {} AND pattern = 'refs/heads/ma*'",
                w.project.get()
            ),
        ],
    )
    .await;
    let status = w.status().await;
    let reasons: Vec<(String, Option<String>)> = status
        .reasons
        .iter()
        .map(|r| (r.as_str().to_owned(), r.detail()))
        .collect();
    assert_eq!(
        reasons,
        [
            (
                "approvals_missing".to_owned(),
                Some("rule=@protected_branch required=2 counted=0".to_owned())
            ),
            (
                "required_check_missing".to_owned(),
                Some("name=ci/gates".to_owned())
            ),
            (
                "required_check_missing".to_owned(),
                Some("name=kanflow/linked".to_owned())
            ),
        ]
    );
    assert_eq!(status.discussions, Some(0), "required by the second rule");
    w.t.drop_now().await;
}

/// The files of `head` plus `n` new one-line files named `<prefix><i>.txt`.
fn with_new_files(prefix: &str, n: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| (format!("{prefix}{i}.txt"), "g\n".to_owned()))
        .collect()
}

/// R6 review round 2 (HIGH): the carry limits are cumulative — measured
/// from the head each approval was given on, not from the previous head.
/// Two pushes of 30 new files each carry the first time (30 files from the
/// approved head) and end the approval the second time (60 files from the
/// approved head), although each push alone is under the limit. An
/// approval given on the intermediate head is measured from there and is
/// still carried.
#[tokio::test]
async fn the_carry_limits_count_from_the_approved_head() {
    let mut w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.settings(forge_core::MergeSettingsValues {
        reset_approvals_on_push: false,
        ..forge_core::MergeSettingsValues::default()
    })
    .await;
    w.approve(w.rita)
        .await
        .expect("rita approves the first head");
    let first = with_new_files("a", 30);
    let mut files: Vec<(&str, &str)> = vec![("README.md", "base\n"), ("f.txt", "f\n")];
    files.extend(first.iter().map(|(n, c)| (n.as_str(), c.as_str())));
    w.push_next(&files, "req-push-2").await;
    let carried_sql = "SELECT count(*) AS n FROM forge.events \
        WHERE event_type = 'merge_request.approved' AND payload ->> 'carried_from' IS NOT NULL";
    assert_eq!(
        count(&w.t, carried_sql).await,
        1,
        "a single small push carries"
    );
    w.approve(w.mara)
        .await
        .expect("mara approves the second head");
    let second = with_new_files("b", 30);
    files.extend(second.iter().map(|(n, c)| (n.as_str(), c.as_str())));
    w.push_next(&files, "req-push-3").await;
    let ended =
        w.t.json_rows(
            "SELECT (payload ->> 'approver_id')::bigint AS approver FROM forge.events \
             WHERE event_type = 'merge_request.unapproved' \
             AND payload ->> 'reason' = 'new_commits'",
        )
        .await;
    let ended: Vec<i64> = ended
        .iter()
        .filter_map(|r| r.get("approver").and_then(serde_json::Value::as_i64))
        .collect();
    assert_eq!(
        ended,
        vec![w.rita.get()],
        "60 files since rita's approval end it; 30 since mara's do not"
    );
    let live =
        w.t.json_rows(
            "SELECT approver_id, carried_from FROM forge.merge_request_approvals \
             WHERE revoked_at IS NULL",
        )
        .await;
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(live[0]["approver_id"].as_i64(), Some(w.mara.get()));
    assert!(
        live[0]["carried_from"].is_i64(),
        "mara's approval was carried"
    );
    w.t.drop_now().await;
}

/// R6 review round 2 (LOW): a git failure of the status's fast-forward
/// check leaves it unknown (the job decides it under the lock) instead of
/// failing the whole status read.
#[tokio::test]
async fn a_git_failure_of_the_fast_forward_check_leaves_it_unknown() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.settings(forge_core::MergeSettingsValues {
        allowed_methods: vec![forge_core::MergeMethod::FastForward],
        default_method: forge_core::MergeMethod::FastForward,
        ..forge_core::MergeSettingsValues::default()
    })
    .await;
    // The target's tip names a parent that is not in the repository:
    // `merge-base --is-ancestor` cannot walk it (exit 128).
    let tree = plumb(
        &w.path,
        &["rev-parse", &format!("{}^{{tree}}", w.base)],
        None,
    );
    let body = format!(
        "tree {tree}\nparent {}\nauthor A <a@example.test> 1700000000 +0000\n\
         committer A <a@example.test> 1700000000 +0000\n\nbroken\n",
        "1".repeat(40)
    );
    let broken = plumb(
        &w.path,
        &["hash-object", "-t", "commit", "-w", "--stdin"],
        Some(body.as_bytes()),
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &broken], None);
    w.browser.invalidate(w.repo);
    let logs = Captured::default();
    let guard = logs.install();
    let status = w.status().await;
    drop(guard);
    let names: Vec<&str> = status.reasons.iter().map(|r| r.as_str()).collect();
    assert!(!names.contains(&"ff_not_possible"), "{names:?}");
    let warned = logs.named("forge.review.status_ff_unknown");
    assert_eq!(warned.len(), 1, "logged once: {warned:?}");
    assert_eq!(warned[0]["level"], "WARN");
    assert_eq!(
        warned[0]["fields"]["error_code"],
        ErrorCode::GitProcessFailed.id()
    );
    assert_eq!(warned[0]["fields"]["request_id"], "req-status");
    w.t.drop_now().await;
}

/// JSON log lines written on the installing thread while its guard lives.
/// One global subscriber writes to a thread-local sink (a thread-local
/// subscriber would let a callsite first hit by a parallel test stay
/// cached as "never": the pattern of forge-db `runner_stop_tests`).
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

thread_local! {
    static SINK: std::cell::RefCell<Option<Captured>> = const { std::cell::RefCell::new(None) };
}

struct ThreadSink;

impl std::io::Write for ThreadSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        SINK.with(|s| {
            if let Some(c) = s.borrow().as_ref() {
                c.0.lock().expect("log buffer").extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        SINK.with(|s| s.borrow_mut().take());
    }
}

impl Captured {
    fn install(&self) -> CaptureGuard {
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::INFO)
                .with_writer(|| ThreadSink)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("the only global subscriber");
        });
        SINK.with(|s| *s.borrow_mut() = Some(self.clone()));
        CaptureGuard
    }

    /// The captured events named `event`.
    fn named(&self, event: &str) -> Vec<serde_json::Value> {
        let bytes = self.0.lock().expect("log buffer").clone();
        String::from_utf8(bytes)
            .expect("utf-8 logs")
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("a JSON log line"))
            .filter(|e| e["fields"]["event"] == event)
            .collect()
    }
}

/// R6 re-review (LOW): a git failure reading the change from one approved
/// head stops the reads — the heads not decided yet end too — and is
/// logged once (`unread_heads`), never once per approved head.
#[tokio::test]
async fn a_git_failure_of_the_carry_reads_is_logged_once() {
    let mut w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.settings(forge_core::MergeSettingsValues {
        reset_approvals_on_push: false,
        ..forge_core::MergeSettingsValues::default()
    })
    .await;
    let first = w.head.clone();
    w.approve(w.rita)
        .await
        .expect("rita approves the first head");
    w.push_next(
        &[("README.md", "base\n"), ("f.txt", "f\n"), ("g.txt", "g\n")],
        "req-push-2",
    )
    .await;
    w.approve(w.mara)
        .await
        .expect("mara approves the second head");
    // The first head's tree is gone: `diff-tree` from it fails, while the
    // commits (and every history walk) stay readable.
    let tree = plumb(&w.path, &["rev-parse", &format!("{first}^{{tree}}")], None);
    std::fs::remove_file(w.path.join("objects").join(&tree[..2]).join(&tree[2..]))
        .expect("a loose tree object");
    let logs = Captured::default();
    let guard = logs.install();
    w.push_next(
        &[
            ("README.md", "base\n"),
            ("f.txt", "f\n"),
            ("g.txt", "g\n"),
            ("h.txt", "h\n"),
        ],
        "req-push-3",
    )
    .await;
    drop(guard);
    let unread = logs.named("forge.review.approvals_carry_unread");
    assert_eq!(unread.len(), 1, "logged once: {unread:?}");
    assert_eq!(unread[0]["level"], "WARN");
    assert!(unread[0]["fields"]["error_code"].is_string());
    assert_eq!(unread[0]["fields"]["unread_heads"], 2);
    let live =
        w.t.json_rows("SELECT id FROM forge.merge_request_approvals WHERE revoked_at IS NULL")
            .await;
    assert!(live.is_empty(), "both approvals end: {live:?}");
    w.t.drop_now().await;
}

// ── M3 C1: the check runs behind the required checks (ADR-0024 §10) ──

impl World {
    /// Report `name` on `head` as forge-checks writes it (one row per
    /// name; a second report updates it).
    async fn run(&self, head: &str, name: &str, status: &str, conclusion: Option<&str>) {
        let conclusion = conclusion.map_or_else(|| "NULL".to_owned(), |c| format!("'{c}'"));
        let completed = if status == "completed" {
            "now()"
        } else {
            "NULL"
        };
        self.t
            .commit_audited(
                ("project", self.project.get()),
                &[format!(
                    "INSERT INTO forge.check_runs (project_id, head_sha, name, status, \
                     conclusion, completed_at, reported_by, request_id) VALUES ({p}, '{head}', \
                     '{name}', '{status}', {conclusion}, {completed}, {u}, 'req-check') \
                     ON CONFLICT (project_id, head_sha, name) DO UPDATE SET \
                     status = EXCLUDED.status, conclusion = EXCLUDED.conclusion, \
                     completed_at = EXCLUDED.completed_at, \
                     revision = forge.check_runs.revision + 1",
                    p = self.project.get(),
                    u = self.owner.get()
                )],
            )
            .await;
    }

    fn reason_names(status: &forge_review::MergeStatus) -> Vec<&'static str> {
        status.reasons.iter().map(|r| r.as_str()).collect()
    }
}

/// The plan's C1 row: a `completed` / `success` run of the required name on
/// the merge request's head clears `required_check_missing`, and the merge
/// goes through.
#[tokio::test]
async fn a_success_on_the_head_clears_required_check_missing() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.protect("developer", 0, &["ci/gates"], false, false).await;
    assert_eq!(
        World::reason_names(&w.status().await),
        ["required_check_missing"]
    );
    w.run(&w.head, "ci/gates", "completed", Some("success"))
        .await;
    let status = w.status().await;
    assert!(status.mergeable, "{:?}", status.reasons);
    let run = status.checks[0].run.expect("reported");
    assert!(run.succeeded());
    let attempt = w.ask().await.expect("queued");
    assert_eq!(
        w.merger.execute(attempt.id).await.expect("merge").outcome,
        "applied"
    );
    w.t.drop_now().await;
}

/// A run reported on an earlier head never counts for the new one.
#[tokio::test]
async fn a_success_on_an_old_head_does_not_count() {
    let mut w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.protect("developer", 0, &["ci/gates"], false, false).await;
    let old = w.head.clone();
    w.run(&old, "ci/gates", "completed", Some("success")).await;
    assert!(w.status().await.mergeable);
    w.push_next(&[("g.txt", "g\n")], "req-push-2").await;
    assert_ne!(w.head, old);
    let status = w.status().await;
    assert_eq!(World::reason_names(&status), ["required_check_missing"]);
    assert!(status.checks[0].run.is_none());
    let blocked = w.ask().await.expect_err("blocked");
    assert_eq!(reasons(&blocked), "required_check_missing");
    w.t.drop_now().await;
}

/// `neutral` and a run still going are no success (ADR-0024 §10).
#[tokio::test]
async fn neutral_and_in_progress_block() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.protect("developer", 0, &["ci/gates"], false, false).await;
    for (status, conclusion) in [
        ("completed", Some("neutral")),
        ("in_progress", None),
        ("queued", None),
        ("completed", Some("failure")),
    ] {
        w.run(&w.head, "ci/gates", status, conclusion).await;
        let s = w.status().await;
        assert_eq!(
            World::reason_names(&s),
            ["required_check_not_successful"],
            "{status} {conclusion:?}"
        );
        let blocked = w.ask().await.expect_err("blocked");
        assert_eq!(reasons(&blocked), "required_check_not_successful");
    }
    w.t.drop_now().await;
}

/// A spy source: every call is counted and answered "no run reported".
#[derive(Default)]
struct SpySource {
    calls: std::sync::atomic::AtomicUsize,
}

impl forge_core::CheckResultsSource for SpySource {
    fn runs_on<'a>(
        &'a self,
        _project: ProjectId,
        _head: &'a forge_core::ObjectId,
    ) -> forge_core::namespace_resolver::LookupFuture<'a, forge_core::CheckRunsOnHead> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(forge_core::CheckRunsOnHead::default()) })
    }
}

/// Review fix 2 (HIGH): the merge request and the merge job read the runs
/// inside the gate's transaction (`MergeGate::check_runs`, under the merge
/// request's locks), never through the status's `CheckResultsSource`. The
/// spy answers "no run" for every call: a request that read through it
/// would be refused (`required_check_missing`) although the stored run
/// succeeded; the spy must not be called by the request or the job, only
/// by the status. The job then decides on the runs as stored at its own
/// gate: a run that failed after the request supersedes the attempt.
#[tokio::test]
async fn the_request_and_the_job_read_the_runs_inside_the_gate_transaction() {
    let mut w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    let spy = Arc::new(SpySource::default());
    w.review = w.review.clone().with_check_results(spy.clone());
    w.protect("developer", 0, &["ci/gates"], false, false).await;
    w.run(&w.head, "ci/gates", "completed", Some("success"))
        .await;
    let attempt = w
        .ask()
        .await
        .expect("queued: the request read the stored success in its gate");
    assert_eq!(spy.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    w.run(&w.head, "ci/gates", "completed", Some("failure"))
        .await;
    let report = w.merger.execute(attempt.id).await.expect("job");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::MergeBlocked));
    assert_eq!(w.tip("main"), w.base);
    assert_eq!(spy.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    // The status reads through the source (the spy says "no run").
    let status = w.status().await;
    assert_eq!(spy.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(World::reason_names(&status), ["required_check_missing"]);
    w.t.drop_now().await;
}

/// A check results source that always fails (the store is down).
struct FailingSource;

impl forge_core::CheckResultsSource for FailingSource {
    fn runs_on<'a>(
        &'a self,
        _project: ProjectId,
        _head: &'a forge_core::ObjectId,
    ) -> forge_core::namespace_resolver::LookupFuture<'a, forge_core::CheckRunsOnHead> {
        Box::pin(async {
            Err(ForgeError::new(ErrorCode::DbQueryFailed).with_context("query", "checks.runs_read"))
        })
    }
}

/// A failed read of the runs fails the status — it is never shown as
/// "no run reported" (`required_check_missing`).
#[tokio::test]
async fn a_store_failure_in_status_is_an_error_not_missing() {
    let mut w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.review = w.review.clone().with_check_results(Arc::new(FailingSource));
    // Without a required check the runs are not read at all.
    assert!(w.status().await.mergeable);
    w.protect("developer", 0, &["ci/gates"], false, false).await;
    let principal = session(w.owner);
    let proof = w
        .mr_proof(&principal, Action::MergeRequestRead)
        .await
        .expect("proof");
    let err = w
        .review
        .merge_status(
            &RequestId::parse("req-status").expect("id"),
            &proof,
            ready(w.repo),
            None,
        )
        .await
        .expect_err("the store failed");
    assert_eq!(err.code(), ErrorCode::DbQueryFailed);
    w.t.drop_now().await;
}

// ── M3 I1d: the work item link merge condition ─────────────────────────

impl World {
    /// `main` protected (Developer) with a merge rule requiring only a work
    /// item link (as `PUT …/merge-rule` writes it).
    async fn protect_with_link(&self) {
        self.t
            .commit_audited(
                ("project", self.project.get()),
                &[
                    format!(
                        "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
                         VALUES ({}, 'refs/heads/main', 'developer')",
                        self.project.get()
                    ),
                    format!(
                        "INSERT INTO forge.protected_ref_merge_rules (protected_ref_id, \
                         project_id, require_work_item_link) \
                         SELECT id, project_id, true FROM forge.protected_refs \
                         WHERE project_id = {}",
                        self.project.get()
                    ),
                ],
            )
            .await;
    }

    /// A kanflow integration (its own service account) bound to the
    /// project as `external`.
    async fn bind_kanflow(&self, external: &str) -> forge_core::IntegrationId {
        let sa = forge_db::service_accounts::create(
            &self.t.db,
            &system(&format!("req-sa-{external}")),
            &forge_db::service_accounts::NewServiceAccount {
                username: format!("kanflow-{external}"),
                display_name: "kanflow".to_owned(),
                owner: self.owner,
                purpose: "Kanflow links work items".to_owned(),
            },
            self.owner,
        )
        .await
        .expect("service account")
        .user_id;
        let integration = forge_db::integrations::create(
            &self.t.db,
            &system(&format!("req-int-{external}")),
            &forge_db::integrations::NewIntegration {
                kind: forge_core::IntegrationKind::Kanflow,
                service_account: sa,
                event_types: None,
            },
            self.owner,
        )
        .await
        .expect("integration");
        self.rebind(integration, external).await;
        integration
    }

    /// A binding as the bind command writes it (forge-hosting's command is
    /// not this crate's to call: `only_forge_hosting_calls_the_decided_commands`):
    /// the row and the service account's Reporter membership, with the
    /// project's audit row.
    async fn rebind(&self, integration: forge_core::IntegrationId, external: &str) {
        self.t
            .commit_audited(
                ("project", self.project.get()),
                &[
                    format!(
                        "INSERT INTO forge.integration_bindings (integration_id, project_id, \
                         external_project_id, granted_membership, created_by) \
                         VALUES ({}, {}, '{external}', true, {})",
                        integration.get(),
                        self.project.get(),
                        self.owner.get()
                    ),
                    format!(
                        "INSERT INTO forge.project_members (project_id, user_id, role) \
                         SELECT {}, service_account_id, 'reporter' FROM forge.integrations \
                         WHERE id = {} ON CONFLICT DO NOTHING",
                        self.project.get(),
                        integration.get()
                    ),
                ],
            )
            .await;
    }

    /// An unbind as the command writes it: the `integration.unbound` audit
    /// row ends the project's open links (`unlink_source = unbound`, I1b),
    /// then the binding and the membership it granted go.
    async fn unbind_kanflow(&self, integration: forge_core::IntegrationId) {
        let p = self.project.get();
        let i = integration.get();
        let o = self.owner.get();
        self.t
            .commit_audited(
                ("project", p),
                &[
                    format!(
                        "INSERT INTO forge.audit_log (actor_kind, actor_user_id, action, outcome, \
                         resource_type, resource_id, project_id, request_id) VALUES ('user', {o}, \
                         'integration.unbound', 'succeeded', 'project', {p}, {p}, 'req-unbind')"
                    ),
                    format!(
                        "UPDATE forge.merge_request_work_items SET unlinked_at = now(), \
                         unlink_source = 'unbound', unlinked_by = {o}, unlinked_by_kind = 'human' \
                         WHERE project_id = {p} AND unlinked_at IS NULL"
                    ),
                    format!("DELETE FROM forge.integration_bindings WHERE project_id = {p}"),
                    format!(
                        "DELETE FROM forge.project_members WHERE project_id = {p} AND user_id = \
                         (SELECT service_account_id FROM forge.integrations WHERE id = {i})"
                    ),
                ],
            )
            .await;
    }

    /// dave (a Developer) links `raw` by hand.
    async fn link_work_item(&self, raw: &str) {
        let who = session(self.dave);
        let proof = self
            .mr_proof(&who, Action::WorkItemLink)
            .await
            .expect("proof");
        let _outcome = self
            .review
            .link_work_item(
                &request(&format!("req-link-{raw}")),
                &proof,
                &forge_core::WorkItemRef::parse(raw).expect("ref"),
            )
            .await
            .expect("link");
    }

    async fn unlink_work_item(&self, raw: &str) {
        let who = session(self.dave);
        let proof = self
            .mr_proof(&who, Action::WorkItemLink)
            .await
            .expect("proof");
        self.review
            .unlink_work_item(
                &request(&format!("req-unlink-{raw}")),
                &proof,
                &forge_core::LinkTarget::Ref(forge_core::WorkItemRef::parse(raw).expect("ref")),
            )
            .await
            .expect("unlink");
    }
}

fn status_names(status: &forge_review::MergeStatus) -> Vec<&'static str> {
    status.reasons.iter().map(|r| r.as_str()).collect()
}

/// M3 I1d (ADR-0024 §7 / §11): a merge rule requiring a work item link
/// blocks a merge request without an open link (`work_item_link_missing`,
/// in the status, the request and the job alike) and lets it merge once a
/// work item of the project's binding is linked.
#[tokio::test]
async fn a_work_item_link_rule_blocks_until_a_work_item_is_linked() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.protect_with_link().await;
    let status = w.status().await;
    assert_eq!(status_names(&status), ["work_item_link_missing"]);
    assert_eq!(status.work_item_link, Some(false));
    let blocked = w.ask().await.expect_err("blocked");
    assert_eq!(blocked.code(), ErrorCode::MergeBlocked);
    assert_eq!(reasons(&blocked), "work_item_link_missing");
    // Bound, but nothing linked yet.
    w.bind_kanflow("kf-1").await;
    assert_eq!(status_names(&w.status().await), ["work_item_link_missing"]);
    w.link_work_item("PBI-5").await;
    let status = w.status().await;
    assert!(status.mergeable, "{:?}", status.reasons);
    assert_eq!(status.work_item_link, Some(true));
    let attempt = w.ask().await.expect("queued");
    let done = w.merger.execute(attempt.id).await.expect("merge");
    assert_eq!(done.outcome, "applied");
    w.t.drop_now().await;
}

/// M3 I1d: a link to a work item of a binding the project no longer has
/// fails the condition — the unbind ends the links (I1b), and the new
/// binding's kanflow project has its own work items.
#[tokio::test]
async fn a_link_to_a_work_item_of_an_unbound_project_fails_the_condition() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.protect_with_link().await;
    let integration = w.bind_kanflow("kf-1").await;
    w.link_work_item("PBI-5").await;
    assert!(w.status().await.mergeable);
    w.unbind_kanflow(integration).await;
    let status = w.status().await;
    assert_eq!(status_names(&status), ["work_item_link_missing"]);
    let blocked = w.ask().await.expect_err("blocked");
    assert_eq!(reasons(&blocked), "work_item_link_missing");
    w.rebind(integration, "kf-2").await;
    assert_eq!(
        status_names(&w.status().await),
        ["work_item_link_missing"],
        "the old kanflow project's work item does not satisfy the new binding"
    );
    w.t.drop_now().await;
}

/// M3 I1d (ADR-0024 §4.2 steps 3 and 5): the job decides the link again —
/// unlinked after the request, the attempt is superseded and no ref moves.
#[tokio::test]
async fn the_merge_job_decides_the_work_item_link_again() {
    let w = world(
        &[("README.md", "base\n")],
        &[("README.md", "base\n"), ("f.txt", "f\n")],
    )
    .await;
    w.protect_with_link().await;
    w.bind_kanflow("kf-1").await;
    w.link_work_item("PBI-5").await;
    let attempt = w.ask().await.expect("queued");
    w.unlink_work_item("PBI-5").await;
    let report = w.merger.execute(attempt.id).await.expect("job");
    assert_eq!(report.outcome, "superseded");
    assert_eq!(report.error_code, Some(ErrorCode::MergeBlocked));
    assert_eq!(w.tip("main"), w.base);
    w.t.drop_now().await;
}
