#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Review fix 7: the real `Browser` adapter of `CommitProbe` against a bare
//! repository. A commit no branch or tag reaches, a reachability check that
//! runs out of time (and, by the same branch, one that finds no free slot)
//! answer exactly like a missing commit (`FG-REPO-0023 reason=ref_not_found`), and the report
//! records nothing.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use forge_auth::{
    prove, AccountKind, Action, CheckNamespaceGrantCtx, CheckRunCtx, Credential, LoadedProject,
    Principal, Scope, ServiceAccountView,
};
use forge_checks::{parse_name, Checks, ChecksRequest, CommitProbe, ReportInput};
use forge_core::{
    CheckNamespace, ObjectId, ProjectAccessLoader, ProjectRepository, RepositoryState, RequestId,
    Role,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::service_accounts::{self, NewServiceAccount};
use forge_db::test_support::{assert_reason, FreshDb};
use forge_db::users::{self, NewUser};
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};

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

fn commit(repo: &Path, content: &str, parent: Option<&str>) -> String {
    let blob = plumb(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(content.as_bytes()),
    );
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

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

#[tokio::test]
async fn the_browser_adapter_answers_unreachable_and_undecided_like_missing() {
    let t = FreshDb::migrated().await;
    let mut ada = NewUser::human("ada", "Ada", "ada@example.test");
    ada.is_admin = true;
    let admin = users::create(&t.db, &ctx("req-ada"), &ada)
        .await
        .expect("ada");
    let sa = service_accounts::create(
        &t.db,
        &ctx("req-sa"),
        &NewServiceAccount {
            username: "ci-runner".to_owned(),
            display_name: "CI".to_owned(),
            owner: admin.user_id,
            purpose: "synthetic CI reporter".to_owned(),
        },
        admin.user_id,
    )
    .await
    .expect("sa")
    .user_id;
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", admin.namespace_id, "Web", admin.user_id),
    )
    .await
    .expect("web");
    projects::add_member(
        &t.db,
        &ctx("req-member"),
        web.project_id,
        sa,
        Role::Developer,
    )
    .await
    .expect("member");
    let _ready = forge_db::repositories::mark_ready(&t.db, &ctx("req-ready"), web.repository_id)
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
    let base = commit(&path, "base\n", None);
    let head = commit(&path, "head\n", Some(&base));
    plumb(&path, &["update-ref", "refs/heads/main", &head], None);
    // A commit no ref reaches (a force-pushed-away tip, say).
    let lost = commit(&path, "lost\n", Some(&base));
    let repo = ProjectRepository {
        id: web.repository_id,
        state: RepositoryState::Ready,
    };
    let checks_with = |limits: BrowseLimits| {
        let browser = Browser::new(git.clone(), storage.clone(), limits);
        Checks::new(t.db.clone(), Arc::new(browser))
    };
    let normal = checks_with(BrowseLimits::default());
    // The grant (an administrator's) on this project.
    let admin_principal = Principal::authenticated(
        admin.user_id,
        true,
        true,
        Credential::Session,
        AccountKind::Human,
    );
    let grant = prove(
        &admin_principal,
        Action::CheckNamespaceGrantManage,
        CheckNamespaceGrantCtx::new(
            sa,
            ServiceAccountView {
                kind: AccountKind::ServiceAccount,
                active: true,
            },
            web.project_id,
            CheckNamespace::Ci,
        ),
    )
    .expect("grant proof");
    let request = |id: &str| ChecksRequest {
        request_id: RequestId::parse(id).expect("id"),
        credential_id: None,
    };
    let _ = normal
        .change_grant(&request("req-grant"), &grant, true)
        .await
        .expect("grant");
    let who = Principal::authenticated(
        sa,
        false,
        true,
        Credential::Token(vec![Scope::Api]),
        AccountKind::ServiceAccount,
    );
    let record =
        t.db.load_project(web.project_id, Some(sa))
            .await
            .expect("load")
            .expect("project");
    let loaded = LoadedProject {
        id: record.id,
        visibility: record.access.visibility,
        role: record.access.role,
        pending_deletion: record.access.pending_deletion,
    };
    let proof_for = |sha: &str| {
        prove(
            &who,
            Action::CheckWrite,
            CheckRunCtx::new(
                loaded,
                ObjectId::parse(sha).expect("sha"),
                parse_name("ci/gates").expect("name"),
                true,
            ),
        )
        .expect("allowed")
    };
    let input = ReportInput {
        status: "queued".to_owned(),
        conclusion: None,
        summary: None,
        details_url: None,
        annotations: Vec::new(),
    };
    // Unreachable: like a missing commit.
    let err = normal
        .report(&request("req-lost"), &proof_for(&lost), repo, &input)
        .await
        .expect_err("unreachable");
    assert_reason(&err, ErrorCode::RepositoryPathNotResolved, "ref_not_found");
    // The reachability check runs out of time: the same answer. Its
    // `rev-list` never answers (`forge_git::test_git`): a 1 ns limit is no
    // timeout — under load a fast git that already answered wins.
    let stall_dir = tempfile::tempdir().expect("stall");
    let slow = Checks::new(
        t.db.clone(),
        Arc::new(Browser::new(
            forge_git::test_git::stalled_git(home.path(), stall_dir.path(), Some("rev-list")),
            storage.clone(),
            BrowseLimits {
                reach_timeout: forge_git::test_git::STALL,
                ..BrowseLimits::default()
            },
        )),
    );
    let err = slow
        .report(&request("req-slow"), &proof_for(&lost), repo, &input)
        .await
        .expect_err("timed out");
    assert_reason(&err, ErrorCode::RepositoryPathNotResolved, "ref_not_found");
    assert!(
        err.context().iter().any(|(k, _)| *k == "reach_error"),
        "the check could not decide (logged as reach_error): {err:?}"
    );
    // No free browse slot (FG-GIT-0004 repo_browse) takes the same
    // `undecided` branch of forge-git as the timeout
    // (forge-git `a_reachability_check_without_a_slot_is_git_busy_not_success`);
    // a slot cannot be held from outside forge-git.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM forge.check_runs")
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count");
    assert_eq!(rows, 0, "nothing recorded");
    // A reachable commit is recorded (the adapter is not refusing all).
    let probe: &dyn CommitProbe =
        &Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let found = probe
        .reachable_commit(
            web.repository_id,
            &ObjectId::parse(&base).expect("sha"),
            "user:1",
        )
        .await
        .expect("base is reachable from main");
    assert_eq!(found.as_str(), base);
    normal
        .report(&request("req-head"), &proof_for(&head), repo, &input)
        .await
        .expect("recorded");
    t.drop_now().await;
}
