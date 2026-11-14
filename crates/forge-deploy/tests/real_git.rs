#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! M4 C11 follow-up: what may be deployed, decided by the real
//! [`GitCommits`] against a bare repository (the service's own tests use a
//! fake `CommitSource`). Only a commit a protected branch of THIS
//! project's repository holds is accepted; anything else is
//! `FG-DEPLOY-0011` before a row is written — an unprotected branch's
//! commit, a missing id, another repository's commit, a commit a
//! force-moved protected branch left behind — and prod also needs a
//! succeeded push pipeline of the SHA.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use forge_auth::{
    prove, AccountKind, Action, Authorized, Credential, LoadedProject, Principal, ProjectCtx, Role,
};
use forge_core::deploy::{DeployTier, DeploymentStatus, TargetPolicy};
use forge_core::{ProjectAccessLoader as _, RefPattern, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::{assert_reason, FreshDb};
use forge_db::users::{self, NewUser};
use forge_deploy::{check_target, ChangeRequest, Deploy, EnvironmentSpec, GitCommits, TargetSpec};
use forge_errors::{ErrorCode, ForgeError};
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
    let message = format!("synthetic {}", content.trim());
    let mut args = vec!["commit-tree", tree.as_str(), "-m", message.as_str()];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

fn rid(s: &str) -> RequestId {
    RequestId::parse(s).expect("request id")
}

fn system(s: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, rid(s))
}

fn request(s: &str) -> ChangeRequest {
    ChangeRequest {
        request_id: rid(s),
        credential_id: None,
    }
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    git: Git,
    storage: StorageRoot,
    browser: Browser,
    deploy: Deploy,
    alice: users::Created,
    web: CreatedProject,
    path: PathBuf,
}

async fn project(t: &FreshDb, owner: &users::Created, name: &str) -> CreatedProject {
    let p = projects::create(
        &t.db,
        &system(&format!("req-{name}")),
        &NewProject::new(name, owner.namespace_id, name, owner.user_id),
    )
    .await
    .expect("project");
    let _ready = forge_db::repositories::mark_ready(&t.db, &system("req-ready"), p.repository_id)
        .await
        .expect("ready");
    forge_db::protected_refs::create(
        &t.db,
        &system("req-protect"),
        p.project_id,
        &RefPattern::parse("refs/heads/main").expect("pattern"),
        Role::Maintainer,
    )
    .await
    .expect("protect");
    p
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = project(&t, &alice, "web").await;
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let commits = GitCommits::new(t.db.clone(), browser.clone());
    let deploy = Deploy::new(t.db.clone(), Arc::new(commits), TargetPolicy::default());
    let w = World {
        t,
        _home: home,
        _root: root,
        git,
        storage,
        browser,
        deploy,
        alice,
        web,
        path,
    };
    w.environments().await;
    w
}

impl World {
    async fn proof(&self, action: Action) -> Authorized<ProjectCtx> {
        let user: UserId = self.alice.user_id;
        let record = self
            .t
            .db
            .load_project(self.web.project_id, Some(user))
            .await
            .expect("load")
            .expect("project");
        let ctx = ProjectCtx::new(LoadedProject {
            id: record.id,
            visibility: record.access.visibility,
            role: record.access.role,
            pending_deletion: record.access.pending_deletion,
        });
        let who =
            Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human);
        prove(&who, action, ctx).expect("allowed")
    }

    async fn environments(&self) {
        let p = self.proof(Action::DeployEnvironmentManage).await;
        for tier in [DeployTier::Stg, DeployTier::Prod] {
            self.deploy
                .put_environment(
                    &request("req-env"),
                    &p,
                    &EnvironmentSpec {
                        tier,
                        requires_approval: tier.is_prod(),
                        approval_ttl_minutes: None,
                        auto_rollback: !tier.is_prod(),
                        auto_deploy_ref: None,
                        freeze_windows: Vec::new(),
                    },
                )
                .await
                .expect("environment");
            let input = check_target(
                &TargetSpec {
                    name: "web-1".to_owned(),
                    host: "app1.internal".to_owned(),
                    port: 22,
                    user: "deploy".to_owned(),
                    host_key_type: "ssh-ed25519".to_owned(),
                    host_key:
                        "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ"
                            .to_owned(),
                    script: "web-app".to_owned(),
                    enabled: true,
                },
                &TargetPolicy::default(),
            )
            .expect("valid");
            forge_db::deploy::put_target_decided(
                &self.t.db,
                &ChangeContext::new(Actor::User(self.alice.user_id), rid("req-target")),
                self.web.project_id,
                tier,
                &input,
                |_| Ok(()),
            )
            .await
            .expect("target");
        }
    }

    /// A ref moved outside a push: forget what the browse API cached (a
    /// push does this in `forge serve`).
    fn set_ref(&self, name: &str, sha: &str) {
        plumb(&self.path, &["update-ref", name, sha], None);
        self.browser.invalidate(self.web.repository_id);
    }

    async fn request(
        &self,
        id: &str,
        tier: DeployTier,
        sha: &str,
    ) -> Result<forge_deploy::StoredDeployment, ForgeError> {
        let p = self.proof(Action::DeployCreate).await;
        self.deploy.request(&request(id), &p, tier, sha).await
    }

    async fn succeeded_pipeline(&self, sha: &str) {
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, status, request_id, \
                   protected, copy_mark) VALUES ({}, 'refs/heads/main', '{sha}', 'push', \
                   'succeeded', 'req-pipe', true, 'open')",
                self.web.project_id.get(),
            ))
            .await;
    }

    async fn deployments(&self) -> usize {
        let p = self.proof(Action::DeployRead).await;
        self.deploy.deployments(&p).await.expect("list").len()
    }
}

fn refused(result: Result<forge_deploy::StoredDeployment, ForgeError>, reason: &str) {
    let e = result.expect_err(reason);
    assert_reason(&e, ErrorCode::DeployCommitNotAllowed, reason);
}

#[tokio::test]
async fn only_a_commit_a_protected_branch_holds_is_deployed() {
    let w = world().await;
    let base = commit(&w.path, "base\n", None);
    let head = commit(&w.path, "head\n", Some(&base));
    w.set_ref("refs/heads/main", &head);
    let feature = commit(&w.path, "feature\n", Some(&head));
    w.set_ref("refs/heads/feature", &feature);

    // The tip and an ancestor of the protected branch.
    for (id, sha) in [("req-tip", &head), ("req-old", &base)] {
        let d = w.request(id, DeployTier::Stg, sha).await.expect(id);
        assert_eq!(d.sha, *sha);
        assert_eq!(d.status, DeploymentStatus::Queued);
    }
    // Only on an unprotected branch (it descends from main: merge-base
    // with main's tip is main's tip, not the commit).
    refused(
        w.request("req-feature", DeployTier::Stg, &feature).await,
        "not_on_protected_branch",
    );
    // No such commit.
    refused(
        w.request("req-missing", DeployTier::Stg, &"0".repeat(40))
            .await,
        "commit_not_found",
    );
    // A commit no ref reaches (only `commit-tree` made it).
    let dangling = commit(&w.path, "dangling\n", Some(&base));
    refused(
        w.request("req-dangling", DeployTier::Stg, &dangling).await,
        "commit_not_found",
    );
    assert_eq!(w.deployments().await, 2);
    w.t.drop_now().await;
}

#[tokio::test]
async fn another_repositorys_commit_is_refused() {
    let w = world().await;
    let mine = commit(&w.path, "mine\n", None);
    w.set_ref("refs/heads/main", &mine);
    // The same owner's other project, its commit on ITS protected main.
    let api = project(&w.t, &w.alice, "api").await;
    let api_path = w
        .storage
        .init_bare(&w.git, api.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init api");
    let theirs = commit(&api_path, "theirs\n", None);
    plumb(&api_path, &["update-ref", "refs/heads/main", &theirs], None);
    refused(
        w.request("req-theirs", DeployTier::Stg, &theirs).await,
        "commit_not_found",
    );
    // Even with a succeeded pipeline recorded for the SHA in this project.
    w.succeeded_pipeline(&theirs).await;
    refused(
        w.request("req-theirs-prod", DeployTier::Prod, &theirs)
            .await,
        "commit_not_found",
    );
    assert_eq!(w.deployments().await, 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_commit_a_force_moved_protected_branch_left_behind_is_refused() {
    let w = world().await;
    let base = commit(&w.path, "base\n", None);
    let shipped = commit(&w.path, "shipped\n", Some(&base));
    w.set_ref("refs/heads/main", &shipped);
    let d = w
        .request("req-before", DeployTier::Stg, &shipped)
        .await
        .expect("on main");
    assert_eq!(d.status, DeploymentStatus::Queued);

    // main force-moved to a sibling; `shipped` kept only by a feature branch.
    let rewritten = commit(&w.path, "rewritten\n", Some(&base));
    w.set_ref("refs/heads/main", &rewritten);
    w.set_ref("refs/heads/keep", &shipped);
    refused(
        w.request("req-kept", DeployTier::Stg, &shipped).await,
        "not_on_protected_branch",
    );
    // And once nothing reaches it, it is like a missing commit.
    plumb(&w.path, &["update-ref", "-d", "refs/heads/keep"], None);
    w.browser.invalidate(w.web.repository_id);
    refused(
        w.request("req-after", DeployTier::Stg, &shipped).await,
        "commit_not_found",
    );
    // The new tip is deployable.
    w.request("req-new", DeployTier::Stg, &rewritten)
        .await
        .expect("new tip");
    assert_eq!(w.deployments().await, 2);
    w.t.drop_now().await;
}

#[tokio::test]
async fn prod_needs_a_succeeded_pipeline_of_the_sha() {
    let w = world().await;
    let base = commit(&w.path, "base\n", None);
    let head = commit(&w.path, "head\n", Some(&base));
    w.set_ref("refs/heads/main", &head);
    refused(
        w.request("req-prod-1", DeployTier::Prod, &head).await,
        "no_succeeded_pipeline",
    );
    // A pipeline of another SHA does not count.
    w.succeeded_pipeline(&base).await;
    refused(
        w.request("req-prod-2", DeployTier::Prod, &head).await,
        "no_succeeded_pipeline",
    );
    w.succeeded_pipeline(&head).await;
    let d = w
        .request("req-prod-3", DeployTier::Prod, &head)
        .await
        .expect("prod");
    assert_eq!(d.status, DeploymentStatus::PendingApproval);
    // A succeeded pipeline never makes an unprotected commit deployable.
    let side = commit(&w.path, "side\n", Some(&head));
    w.set_ref("refs/heads/side", &side);
    w.succeeded_pipeline(&side).await;
    refused(
        w.request("req-prod-4", DeployTier::Prod, &side).await,
        "not_on_protected_branch",
    );
    assert_eq!(w.deployments().await, 1);
    w.t.drop_now().await;
}
