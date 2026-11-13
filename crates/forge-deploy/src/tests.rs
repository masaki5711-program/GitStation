//! The service against a migrated database (M4 C11): validation before
//! anything is written, the approval decided by `authorize()` and again in
//! its transaction, a new approval for every request (ADR-0029 obligation
//! 6), and one worker pass with a scripted transport.

use std::sync::Mutex;
use std::time::Duration;

use forge_auth::{prove, AccountKind, Credential, LoadedProject, Role};
use forge_core::deploy::{DeploymentStatus, RemoteVerb};
use forge_core::ProjectAccessLoader as _;
use forge_db::change::Actor as DbActor;
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};

use super::*;
use crate::run::{run_once, Pass, Timeouts};
use crate::transport::{CallFuture, DeployTransport, Outcome, RemoteCall};

fn rid(s: &str) -> RequestId {
    RequestId::parse(s).expect("request id")
}

fn request(s: &str) -> ChangeRequest {
    ChangeRequest {
        request_id: rid(s),
        credential_id: None,
    }
}

struct World {
    t: FreshDb,
    deploy: Deploy,
    alice: UserId,
    bob: UserId,
    web: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let sys = |s: &str| ChangeContext::new(DbActor::System, rid(s));
    let alice = users::create(
        &t.db,
        &sys("req-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let bob = users::create(
        &t.db,
        &sys("req-bob"),
        &NewUser::human("bob", "bob", "bob@example.test"),
    )
    .await
    .expect("bob");
    let web = projects::create(
        &t.db,
        &sys("req-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    projects::add_member(
        &t.db,
        &sys("req-member"),
        web.project_id,
        bob.user_id,
        Role::Maintainer,
    )
    .await
    .expect("bob is a Maintainer");
    let deploy = Deploy::new(t.db.clone());
    World {
        t,
        deploy,
        alice: alice.user_id,
        bob: bob.user_id,
        web,
    }
}

impl World {
    fn principal(&self, user: UserId, credential: Credential) -> Principal {
        Principal::authenticated(user, false, true, credential, AccountKind::Human)
    }

    async fn project_ctx(&self, user: UserId) -> ProjectCtx {
        let record = self
            .t
            .db
            .load_project(self.web.project_id, Some(user))
            .await
            .expect("load")
            .expect("project");
        ProjectCtx::new(LoadedProject {
            id: record.id,
            visibility: record.access.visibility,
            role: record.access.role,
            pending_deletion: record.access.pending_deletion,
        })
    }

    async fn proof(&self, user: UserId, action: Action) -> Authorized<ProjectCtx> {
        let ctx = self.project_ctx(user).await;
        prove(&self.principal(user, Credential::Session), action, ctx).expect("allowed")
    }

    async fn setup(&self) {
        let p = self
            .proof(self.alice, Action::DeployEnvironmentManage)
            .await;
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
        }
        // Targets are an administrator's: written here through forge-db
        // with the same checks.
        for tier in [DeployTier::Stg, DeployTier::Prod] {
            let input = check_target(&target_spec("web-1")).expect("valid");
            forge_db::deploy::put_target_decided(
                &self.t.db,
                &ChangeContext::new(DbActor::User(self.alice), rid("req-target")),
                self.web.project_id,
                tier,
                &input,
                |_| Ok(()),
            )
            .await
            .expect("target");
        }
    }

    async fn approval_proof(
        &self,
        user: UserId,
        id: i64,
        credential: Credential,
    ) -> Result<Authorized<DeploymentCtx>, ForgeError> {
        let stored = self
            .deploy
            .deployment(self.web.project_id, id)
            .await
            .expect("read")
            .expect("deployment");
        let p = self.project_ctx(user).await;
        let loaded_project = LoadedProject {
            id: self.web.project_id,
            visibility: p.view().visibility,
            role: p.view().role,
            pending_deletion: p.view().pending_deletion,
        };
        prove(
            &self.principal(user, credential),
            Action::DeployApprove,
            DeploymentCtx::new(loaded_project, loaded_deployment(&stored)),
        )
    }
}

fn target_spec(name: &str) -> TargetSpec {
    TargetSpec {
        name: name.to_owned(),
        host: "app1.internal".to_owned(),
        port: 22,
        user: "deploy".to_owned(),
        host_key_type: "ssh-ed25519".to_owned(),
        host_key: "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ".to_owned(),
        script: "web-app".to_owned(),
        enabled: true,
    }
}

#[test]
fn values_that_could_reach_ssh_are_refused_before_anything_is_written() {
    for (spec, param_name) in [
        (
            TargetSpec {
                host: "-oProxyCommand=sh".to_owned(),
                ..target_spec("web-1")
            },
            "host",
        ),
        (
            TargetSpec {
                script: "deploy.sh; rm -rf /".to_owned(),
                ..target_spec("web-1")
            },
            "script",
        ),
        (
            TargetSpec {
                user: "root@evil".to_owned(),
                ..target_spec("web-1")
            },
            "user",
        ),
        (
            TargetSpec {
                host_key: "AAAA\nevil".to_owned(),
                ..target_spec("web-1")
            },
            "host_key",
        ),
        (
            TargetSpec {
                port: 0,
                ..target_spec("web-1")
            },
            "port",
        ),
    ] {
        let e = check_target(&spec).expect_err(param_name);
        assert_eq!(e.code(), ErrorCode::RequestInvalid);
        assert!(
            e.to_string().contains(&format!("param={param_name}")),
            "{e}"
        );
    }
    let prod = |requires_approval, auto_rollback, auto: Option<&str>| EnvironmentSpec {
        tier: DeployTier::Prod,
        requires_approval,
        approval_ttl_minutes: None,
        auto_rollback,
        auto_deploy_ref: auto.map(str::to_owned),
        freeze_windows: Vec::new(),
    };
    assert!(check_environment(&prod(true, false, None)).is_ok());
    assert!(check_environment(&prod(false, false, None)).is_err());
    assert!(check_environment(&prod(true, true, None)).is_err());
    assert!(check_environment(&prod(true, false, Some("refs/heads/main"))).is_err());
}

#[tokio::test]
async fn the_requester_cannot_approve_their_own_deployment() {
    let w = world().await;
    w.setup().await;
    // Bob (a Maintainer) requests prod; he may not approve it.
    let p = w.proof(w.bob, Action::DeployCreate).await;
    let d = w
        .deploy
        .request(&request("req-d1"), &p, DeployTier::Prod, &"a".repeat(40))
        .await
        .expect("request");
    assert_eq!(d.status, DeploymentStatus::PendingApproval);
    let own = w
        .approval_proof(w.bob, d.id, Credential::Session)
        .await
        .expect_err("own request");
    assert_eq!(own.code(), ErrorCode::DeploySelfApprovalRefused);
    // Alice (the Owner) may, but only in a web session for prod.
    let token = w
        .approval_proof(
            w.alice,
            d.id,
            Credential::Token(vec![forge_core::Scope::Api]),
        )
        .await
        .expect_err("token");
    assert_eq!(token.code(), ErrorCode::InsufficientScope);
    let proof = w
        .approval_proof(w.alice, d.id, Credential::Session)
        .await
        .expect("allowed");
    let approved = w
        .deploy
        .approve(&request("req-a1"), &proof)
        .await
        .expect("approve");
    assert_eq!(approved.status, DeploymentStatus::Queued);
    assert_eq!(approved.approved_by, Some(w.alice.get()));
    w.t.drop_now().await;
}

#[tokio::test]
async fn requesting_the_same_sha_again_waits_for_a_new_approval() {
    let w = world().await;
    w.setup().await;
    let p = w.proof(w.bob, Action::DeployCreate).await;
    let sha = "c".repeat(40);
    let first = w
        .deploy
        .request(&request("req-d1"), &p, DeployTier::Prod, &sha)
        .await
        .expect("first");
    let proof = w
        .approval_proof(w.alice, first.id, Credential::Session)
        .await
        .expect("allowed");
    w.deploy
        .approve(&request("req-a1"), &proof)
        .await
        .expect("approve");
    // A retried deployment of the same SHA is a new request: it waits.
    let again = w
        .deploy
        .request(&request("req-d2"), &p, DeployTier::Prod, &sha)
        .await
        .expect("again");
    assert_eq!(again.status, DeploymentStatus::PendingApproval);
    assert_eq!(again.approved_by, None);
    // A bad SHA is the caller's 400.
    let bad = w
        .deploy
        .request(&request("req-d3"), &p, DeployTier::Prod, "HEAD")
        .await
        .expect_err("bad sha");
    assert_eq!(bad.code(), ErrorCode::RequestInvalid);
    w.t.drop_now().await;
}

/// Answers every call with `Ok` and counts them.
struct Count(Mutex<Vec<&'static str>>);

impl DeployTransport for Count {
    fn call<'a>(&'a self, call: &'a RemoteCall) -> CallFuture<'a> {
        self.0.lock().expect("calls").push(call.verb.as_str());
        Box::pin(async { Outcome::Ok })
    }
}

#[tokio::test]
async fn a_worker_pass_runs_a_queued_stg_deployment_to_success() {
    let w = world().await;
    w.setup().await;
    let p = w.proof(w.bob, Action::DeployCreate).await;
    let d = w
        .deploy
        .request(&request("req-d1"), &p, DeployTier::Stg, &"d".repeat(40))
        .await
        .expect("request");
    assert_eq!(
        d.status,
        DeploymentStatus::Queued,
        "stg needs no approval here"
    );
    let key_dir = tempfile::tempdir().expect("dir");
    let key = key_dir.path().join("deploy_key");
    std::fs::write(&key, b"x").expect("key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    let timeouts = Timeouts {
        script: Duration::from_secs(5),
        health: Duration::from_secs(5),
    };
    let transport = Count(Mutex::new(Vec::new()));
    // Without a key nothing is taken.
    let e = run_once(&w.t.db, &transport, None, timeouts)
        .await
        .expect_err("no key");
    assert_eq!(e.code(), ErrorCode::DeployKeyUnavailable);
    assert!(transport.0.lock().expect("calls").is_empty());
    let pass = run_once(&w.t.db, &transport, Some(&key), timeouts)
        .await
        .expect("pass");
    assert_eq!(pass, Pass::Ran(d.id, DeploymentStatus::Succeeded));
    assert_eq!(
        *transport.0.lock().expect("calls"),
        vec![RemoteVerb::Deploy.as_str(), RemoteVerb::Health.as_str()]
    );
    let pass = run_once(&w.t.db, &transport, Some(&key), timeouts)
        .await
        .expect("idle");
    assert_eq!(pass, Pass::Idle);
    w.t.drop_now().await;
}
