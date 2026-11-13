//! Deployments against a migrated database (M4 C11): environments and
//! targets with their audit rows, requests and approvals, the freeze
//! windows and the approval checked again when the worker takes a
//! deployment, one running deployment per environment, the previous
//! version, and the automatic request from a succeeded pipeline.

use forge_core::deploy::{DeployTier, DeploymentStatus, HostKey};
use forge_core::{ProjectId, RequestId, UserId};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
}

fn host_key() -> HostKey {
    // ssh-ed25519 with a key of 32 bytes 0x09.
    HostKey::parse(
        "ssh-ed25519",
        "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ",
    )
    .expect("key")
}

struct World {
    t: FreshDb,
    alice: UserId,
    bob: UserId,
    web: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &ctx(Actor::System, "req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let bob = users::create(
        &t.db,
        &ctx(Actor::System, "req-user-bob"),
        &NewUser::human("bob", "bob", "bob@example.test"),
    )
    .await
    .expect("bob");
    let web = projects::create(
        &t.db,
        &ctx(Actor::System, "req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    World {
        t,
        alice: alice.user_id,
        bob: bob.user_id,
        web,
    }
}

fn env_input(tier: DeployTier, freeze: Vec<(String, String)>) -> EnvironmentInput {
    EnvironmentInput {
        tier,
        requires_approval: tier.is_prod(),
        approval_ttl_minutes: 60,
        auto_rollback: !tier.is_prod(),
        auto_deploy_ref: (!tier.is_prod()).then(|| "refs/heads/main".to_owned()),
        freeze_windows: freeze,
    }
}

impl World {
    fn project(&self) -> ProjectId {
        self.web.project_id
    }

    async fn env(&self, tier: DeployTier, freeze: Vec<(String, String)>) -> StoredEnvironment {
        put_environment_decided(
            &self.t.db,
            &ctx(Actor::User(self.alice), "req-env"),
            self.project(),
            &env_input(tier, freeze),
            |_| Ok(()),
        )
        .await
        .expect("environment")
        .0
    }

    async fn target(&self, tier: DeployTier, name: &str) -> Result<StoredTarget, ForgeError> {
        put_target_decided(
            &self.t.db,
            &ctx(Actor::User(self.alice), "req-target"),
            self.project(),
            tier,
            &TargetInput {
                name: name.to_owned(),
                host: "app1.internal".to_owned(),
                port: 22,
                login_user: "deploy".to_owned(),
                host_key: host_key(),
                script: "web-app".to_owned(),
                enabled: true,
            },
            |_| Ok(()),
        )
        .await
        .map(|(t, _)| t)
    }

    async fn request(
        &self,
        who: UserId,
        tier: DeployTier,
        sha: &str,
    ) -> Result<StoredDeployment, ForgeError> {
        request_decided(
            &self.t.db,
            &ctx(Actor::User(who), "req-deploy"),
            self.project(),
            tier,
            sha,
            |_| Ok(()),
        )
        .await
    }

    async fn approve(&self, who: UserId, id: i64) -> Result<StoredDeployment, ForgeError> {
        approve_decided(
            &self.t.db,
            &ctx(Actor::User(who), "req-approve"),
            self.project(),
            id,
            |_, _| Ok(()),
        )
        .await
    }

    async fn audit_actions(&self) -> Vec<String> {
        let mut su = self.t.superuser().await;
        sqlx::query_scalar(
            "SELECT action FROM forge.audit_log WHERE action LIKE 'deploy%' ORDER BY id",
        )
        .fetch_all(&mut su)
        .await
        .expect("audit")
    }
}

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

/// A freeze window around now (the database clock is close to ours).
fn around_now() -> Vec<(String, String)> {
    vec![(
        "2000-01-01T00:00:00Z".to_owned(),
        "2100-01-01T00:00:00Z".to_owned(),
    )]
}

#[tokio::test]
async fn environments_and_targets_are_set_with_an_audit_row_each() {
    let w = world().await;
    let stg = w.env(DeployTier::Stg, Vec::new()).await;
    assert!(stg.auto_rollback && !stg.requires_approval && !stg.frozen_now);
    let prod = w
        .env(
            DeployTier::Prod,
            vec![(
                "2030-01-01T00:00:00+09:00".to_owned(),
                "2030-01-02T00:00:00+09:00".to_owned(),
            )],
        )
        .await;
    assert_eq!(
        prod.freeze_windows,
        vec!["2029-12-31T15:00:00Z/2030-01-01T15:00:00Z".to_owned()]
    );
    assert!(!prod.frozen_now);
    let t = w.target(DeployTier::Stg, "web-1").await.expect("target");
    assert_eq!(t.host_key_fingerprint, host_key().fingerprint());
    let missing = w
        .target(DeployTier::Dev, "web-1")
        .await
        .expect_err("no dev");
    assert_eq!(missing.code(), ErrorCode::NotFound);
    let listed = list_environments(&w.t.db, w.project()).await.expect("list");
    assert_eq!(
        listed.iter().map(|e| e.tier).collect::<Vec<_>>(),
        vec![DeployTier::Stg, DeployTier::Prod]
    );
    assert_eq!(
        list_targets(&w.t.db, w.project())
            .await
            .expect("targets")
            .len(),
        1
    );
    assert_eq!(
        w.audit_actions().await,
        vec![
            "deploy_environment.created",
            "deploy_environment.created",
            "deploy_target.created"
        ]
    );
    // A bad window is the caller's 400, nothing written.
    for (bad, reason) in [
        (
            ("not-a-time".to_owned(), "2030-01-01T00:00:00Z".to_owned()),
            "invalid_timestamp",
        ),
        (
            (
                "2030-01-02T00:00:00Z".to_owned(),
                "2030-01-01T00:00:00Z".to_owned(),
            ),
            "empty_window",
        ),
    ] {
        let e = put_environment_decided(
            &w.t.db,
            &ctx(Actor::User(w.alice), "req-env-bad"),
            w.project(),
            &env_input(DeployTier::Stg, vec![bad]),
            |_| Ok(()),
        )
        .await
        .expect_err(reason);
        assert_eq!(e.code(), ErrorCode::RequestInvalid);
        assert!(e.to_string().contains(reason), "{e}");
    }
    // The database refuses an automatic prod.
    let mut su = w.t.superuser().await;
    let refused = sqlx::query(
        "UPDATE forge.deploy_environments SET auto_rollback = true WHERE tier = 'prod'",
    )
    .execute(&mut su)
    .await;
    assert!(refused.is_err(), "prod never rolls back by itself");
    w.t.drop_now().await;
}

#[tokio::test]
async fn approving_a_deployment_twice_is_a_state_conflict() {
    let w = world().await;
    w.env(DeployTier::Prod, Vec::new()).await;
    w.target(DeployTier::Prod, "web-1").await.expect("target");
    let d = w
        .request(w.alice, DeployTier::Prod, &sha('a'))
        .await
        .expect("request");
    assert_eq!(d.status, DeploymentStatus::PendingApproval);
    assert_eq!(d.requested_by, Some(w.alice.get()));
    let approved = w.approve(w.bob, d.id).await.expect("approve");
    assert_eq!(approved.status, DeploymentStatus::Queued);
    assert_eq!(approved.approved_by, Some(w.bob.get()));
    assert!(approved.approval_expires_at.is_some());
    let again = w.approve(w.bob, d.id).await.expect_err("twice");
    assert_eq!(again.code(), ErrorCode::DeploymentStateConflict);
    // The table refuses a self-approval even past authorize().
    let d2 = w
        .request(w.alice, DeployTier::Prod, &sha('b'))
        .await
        .expect("request 2");
    let own = w.approve(w.alice, d2.id).await.expect_err("own");
    assert_ne!(own.code(), ErrorCode::DeploymentStateConflict);
    assert_eq!(
        w.audit_actions()
            .await
            .iter()
            .filter(|a| a.starts_with("deployment."))
            .count(),
        3,
        "two requests and one approval"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_freeze_window_blocks_at_request_and_at_execution() {
    let w = world().await;
    w.env(DeployTier::Stg, around_now()).await;
    w.target(DeployTier::Stg, "web-1").await.expect("target");
    let refused = w
        .request(w.alice, DeployTier::Stg, &sha('a'))
        .await
        .expect_err("frozen");
    assert_eq!(refused.code(), ErrorCode::DeployFrozen);
    // Requested while open, then frozen before the worker takes it.
    w.env(DeployTier::Stg, Vec::new()).await;
    let d = w
        .request(w.alice, DeployTier::Stg, &sha('a'))
        .await
        .expect("request");
    assert_eq!(d.status, DeploymentStatus::Queued);
    w.env(DeployTier::Stg, around_now()).await;
    match claim_next(&w.t.db).await.expect("claim") {
        Claim::Blocked { id, code, .. } => {
            assert_eq!(id, d.id);
            assert_eq!(code, ErrorCode::DeployFrozen);
        }
        other @ (Claim::Idle | Claim::Run(_)) => panic!("expected blocked, got {other:?}"),
    }
    let stored = get_deployment(&w.t.db, w.project(), d.id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(stored.status, DeploymentStatus::Blocked);
    assert_eq!(stored.error_code.as_deref(), Some("FG-DEPLOY-0002"));
    assert_eq!(claim_next(&w.t.db).await.expect("idle"), Claim::Idle);
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_expired_approval_blocks_the_deployment() {
    let w = world().await;
    let prod = w.env(DeployTier::Prod, Vec::new()).await;
    w.target(DeployTier::Prod, "web-1").await.expect("target");
    // An approval that expired before the worker ran (written directly:
    // the window is at least a minute).
    let mut su = w.t.superuser().await;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO forge.deployments (project_id, environment_id, tier, sha, status, \
           requested_by, request_id, approved_by, approved_at, approval_expires_at) \
         VALUES ($1, $2, 'prod', $3, 'queued', $4, 'req-old', $5, now() - interval '2 hours', \
           now() - interval '1 hour') RETURNING id",
    )
    .bind(w.project().get())
    .bind(prod.id)
    .bind(sha('c'))
    .bind(w.alice.get())
    .bind(w.bob.get())
    .fetch_one(&mut su)
    .await
    .expect("insert");
    match claim_next(&w.t.db).await.expect("claim") {
        Claim::Blocked {
            id: got,
            code,
            reason,
            ..
        } => {
            assert_eq!(got, id);
            assert_eq!(code, ErrorCode::DeployApprovalRequired);
            assert_eq!(reason, "approval_expired");
        }
        other @ (Claim::Idle | Claim::Run(_)) => panic!("expected blocked, got {other:?}"),
    }
    // A request nobody approved within the window ends blocked too.
    let pending: i64 = sqlx::query_scalar(
        "INSERT INTO forge.deployments (project_id, environment_id, tier, sha, status, \
           requested_by, request_id, created_at) \
         VALUES ($1, $2, 'prod', $3, 'pending_approval', $4, 'req-old', \
           now() - interval '2 hours') RETURNING id",
    )
    .bind(w.project().get())
    .bind(prod.id)
    .bind(sha('d'))
    .bind(w.alice.get())
    .fetch_one(&mut su)
    .await
    .expect("insert pending");
    let late = w.approve(w.bob, pending).await.expect_err("too late");
    assert_eq!(late.code(), ErrorCode::DeployApprovalRequired);
    let blocked = block_unapproved(&w.t.db).await.expect("block");
    assert_eq!(
        blocked.iter().map(|b| b.0).collect::<Vec<_>>(),
        vec![pending]
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_claim_records_the_previous_version_and_runs_one_per_environment() {
    let w = world().await;
    w.env(DeployTier::Stg, Vec::new()).await;
    w.target(DeployTier::Stg, "web-1").await.expect("target");
    w.target(DeployTier::Stg, "web-2").await.expect("target 2");
    let first = w
        .request(w.alice, DeployTier::Stg, &sha('a'))
        .await
        .expect("first");
    let second = w
        .request(w.alice, DeployTier::Stg, &sha('b'))
        .await
        .expect("second");
    let Claim::Run(run) = claim_next(&w.t.db).await.expect("claim") else {
        panic!("expected a run");
    };
    assert_eq!(run.id, first.id);
    assert_eq!(run.previous_sha, None, "the first deployment has none");
    assert!(run.auto_rollback);
    assert_eq!(
        run.targets
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["web-1", "web-2"]
    );
    // The environment runs one at a time.
    assert_eq!(claim_next(&w.t.db).await.expect("busy"), Claim::Idle);
    let end = Finish {
        status: DeploymentStatus::Succeeded,
        error_code: None,
        failed_target: None,
    };
    assert!(finish(&w.t.db, first.id, &end).await.expect("finish"));
    assert!(!finish(&w.t.db, first.id, &end).await.expect("only once"));
    let Claim::Run(next) = claim_next(&w.t.db).await.expect("claim 2") else {
        panic!("expected the second");
    };
    assert_eq!(next.id, second.id);
    assert_eq!(next.previous_sha.as_deref(), Some(sha('a').as_str()));
    // The guard: a finished deployment never changes.
    let mut su = w.t.superuser().await;
    let changed = sqlx::query("UPDATE forge.deployments SET status = 'queued' WHERE id = $1")
        .bind(first.id)
        .execute(&mut su)
        .await;
    assert!(changed.is_err());
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_succeeded_push_pipeline_requests_an_automatic_deployment_once() {
    let w = world().await;
    w.env(DeployTier::Stg, Vec::new()).await;
    w.target(DeployTier::Stg, "web-1").await.expect("target");
    let mut su = w.t.superuser().await;
    sqlx::query(
        "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, status, request_id, \
           protected, copy_mark) \
         VALUES ($1, 'refs/heads/main', $2, 'push', 'succeeded', 'req-pipe', true, 'open')",
    )
    .bind(w.project().get())
    .bind(sha('e'))
    .execute(&mut su)
    .await
    .expect("pipeline");
    let rid = RequestId::parse("req-auto").expect("rid");
    let made = auto_request(&w.t.db, &rid, 10).await.expect("auto");
    assert_eq!(made.len(), 1);
    assert_eq!(made[0].requested_by, None, "requested by the system");
    assert_eq!(made[0].status, DeploymentStatus::Queued);
    assert!(auto_request(&w.t.db, &rid, 10)
        .await
        .expect("again")
        .is_empty());
    w.t.drop_now().await;
}
