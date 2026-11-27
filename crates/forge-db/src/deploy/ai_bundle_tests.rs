//! `pfs-ai-bundle` releases against a migrated database (plan 17 P17-7):
//! the release row and its audit row, a release never changes, a version
//! and a commit are released once, the stage order of a release
//! deployment, plain SHAs refused in a releases-only project, and the
//! rollback target.

use forge_core::deploy::{DeployTier, DeploymentStatus, HostKey};
use forge_core::{ProjectId, RequestId, UserId};
use forge_errors::ErrorCode;

use super::super::{put_environment_decided, put_target_decided, EnvironmentInput, TargetInput};
use super::*;
use crate::change::Actor;
use crate::deploy::{request_decided, request_release_decided};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
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
    alice: UserId,
    bob: UserId,
    assets: CreatedProject,
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
    let assets = projects::create(
        &t.db,
        &ctx(Actor::System, "req-project-assets"),
        &NewProject::new("assets", alice.namespace_id, "Assets", alice.user_id),
    )
    .await
    .expect("assets");
    let w = World {
        t,
        alice: alice.user_id,
        bob: bob.user_id,
        assets,
    };
    for tier in DeployTier::ALL {
        put_environment_decided(
            &w.t.db,
            &ctx(Actor::User(w.alice), "req-env"),
            w.project(),
            &EnvironmentInput {
                tier,
                requires_approval: true,
                approval_ttl_minutes: 60,
                auto_rollback: false,
                auto_deploy_ref: None,
                freeze_windows: Vec::new(),
            },
            |_| Ok(()),
        )
        .await
        .expect("environment");
        put_target_decided(
            &w.t.db,
            &ctx(Actor::User(w.alice), "req-target"),
            w.project(),
            tier,
            &TargetInput {
                name: "ai-node-1".to_owned(),
                host: "ai1.internal".to_owned(),
                port: 22,
                login_user: "deploy".to_owned(),
                host_key: HostKey::parse(
                    "ssh-ed25519",
                    "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ",
                )
                .expect("key"),
                script: "pfs-ai-loader".to_owned(),
                enabled: true,
            },
            |_| Ok(()),
        )
        .await
        .expect("target");
    }
    w
}

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

fn new_release(version: &str, commit: char) -> NewRelease {
    NewRelease {
        version: version.to_owned(),
        tag_object: sha(commit),
        commit_sha: sha(commit),
        manifest: serde_json::json!({ "kind": "pfs-ai-bundle" }),
        digest: format!("sha256:{}", "0".repeat(64)),
        key_id: "1".to_owned(),
        signature: format!("{}==", "A".repeat(86)),
        bench_check_run_id: 1,
        files: 3,
    }
}

impl World {
    fn project(&self) -> ProjectId {
        self.assets.project_id
    }

    async fn enable(&self) {
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.project_ai_assets (project_id, enabled, changed_by) \
                 VALUES ({}, true, {})",
                self.project().get(),
                self.alice.get()
            ))
            .await;
    }

    async fn release(&self, version: &str, commit: char) -> Result<StoredRelease, ForgeError> {
        create_release_decided(
            &self.t.db,
            &ctx(Actor::User(self.alice), "req-release"),
            self.project(),
            &new_release(version, commit),
            |_| Ok(()),
            |_| Ok(()),
        )
        .await
    }

    /// A deployment that already ran (written as the worker would leave it).
    async fn ran(&self, tier: DeployTier, commit: char, approved: bool) {
        let approval = if approved {
            format!("{}, now(), now() + interval '1 hour'", self.bob.get())
        } else {
            "NULL, NULL, NULL".to_owned()
        };
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.deployments (project_id, environment_id, tier, sha, status, \
                   requested_by, request_id, approved_by, approved_at, approval_expires_at, \
                   started_at, finished_at) \
                 SELECT {p}, e.id, '{t}', '{s}', 'succeeded', {a}, 'req-ran', {approval}, \
                   now(), clock_timestamp() FROM forge.deploy_environments e \
                 WHERE e.project_id = {p} AND e.tier = '{t}'",
                p = self.project().get(),
                t = tier.as_str(),
                s = sha(commit),
                a = self.alice.get(),
            ))
            .await;
    }

    async fn request_release(
        &self,
        tier: DeployTier,
        commit: char,
    ) -> Result<super::super::StoredDeployment, ForgeError> {
        let s = sha(commit);
        request_release_decided(
            &self.t.db,
            &ctx(Actor::User(self.alice), "req-deploy"),
            (self.project(), tier, s.as_str()),
            None,
            |_| Ok(()),
        )
        .await
    }
}

#[tokio::test]
async fn a_release_is_written_with_its_audit_row_and_never_changes() {
    let w = world().await;
    w.enable().await;
    let stored = w.release("v2026.48.1", 'a').await.expect("release");
    assert_eq!(stored.tag, "assets/v2026.48.1");
    assert_eq!(stored.kind, "pfs-ai-bundle");
    let audits = w
        .t
        .json_rows(&format!(
            "SELECT id FROM forge.audit_log WHERE action = 'ai_asset_release.created'              AND resource_id = {}",
            w.project().get()
        ))
        .await;
    assert_eq!(audits.len(), 1);
    let refused = sqlx::query(&format!(
        "UPDATE forge.ai_bundle_releases SET commit_sha = '{}' WHERE id = {}",
        sha('b'),
        stored.id
    ))
    .execute(w.t.db.write_pool())
    .await;
    assert!(refused.is_err(), "forge_app cannot change a release");
    assert_eq!(
        release(&w.t.db, w.project(), "v2026.48.1")
            .await
            .expect("read")
            .expect("there")
            .commit_sha,
        sha('a')
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_release_needs_ai_assets_enabled_and_is_made_once() {
    let w = world().await;
    let e = w.release("v2026.48.1", 'a').await.expect_err("disabled");
    assert_eq!(e.code(), ErrorCode::BundleReleaseRefused);
    assert_eq!(reason(&e), "ai_assets_disabled");
    w.enable().await;
    w.release("v2026.48.1", 'a').await.expect("release");
    // The same version again (a moved tag) and another version of the
    // same commit are both refused; the first release stands.
    for (version, commit) in [("v2026.48.1", 'b'), ("v2026.48.2", 'a')] {
        let e = w.release(version, commit).await.expect_err(version);
        assert_eq!(reason(&e), "already_released", "{version}");
    }
    assert_eq!(
        list_releases(&w.t.db, w.project())
            .await
            .expect("list")
            .len(),
        1
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_releases_only_project_refuses_a_plain_sha() {
    let w = world().await;
    w.enable().await;
    let s = sha('c');
    let e = request_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-deploy"),
        (w.project(), DeployTier::Dev, s.as_str()),
        None,
        |_| Ok(()),
    )
    .await
    .expect_err("plain sha");
    assert_eq!(e.code(), ErrorCode::BundleDeployRefused);
    assert_eq!(reason(&e), "release_required");
    let e = w
        .request_release(DeployTier::Dev, 'c')
        .await
        .expect_err("no release");
    assert_eq!(reason(&e), "release_not_found");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_release_goes_shadow_then_canary_then_prod() {
    let w = world().await;
    w.enable().await;
    w.release("v2026.48.1", 'a').await.expect("release");
    let e = w
        .request_release(DeployTier::Stg, 'a')
        .await
        .expect_err("canary before shadow");
    assert_eq!(reason(&e), "stage_skipped");
    let shadow = w
        .request_release(DeployTier::Dev, 'a')
        .await
        .expect("shadow");
    assert_eq!(shadow.status, DeploymentStatus::PendingApproval);
    // An unapproved success on the shadow does not open the canary.
    w.ran(DeployTier::Dev, 'a', false).await;
    let e = w
        .request_release(DeployTier::Stg, 'a')
        .await
        .expect_err("unapproved");
    assert_eq!(reason(&e), "stage_skipped");
    w.ran(DeployTier::Dev, 'a', true).await;
    w.request_release(DeployTier::Stg, 'a')
        .await
        .expect("canary");
    let e = w
        .request_release(DeployTier::Prod, 'a')
        .await
        .expect_err("prod");
    assert_eq!(reason(&e), "stage_skipped");
    w.ran(DeployTier::Stg, 'a', true).await;
    let prod = w.request_release(DeployTier::Prod, 'a').await;
    // prod also needs the commit's succeeded push pipeline (ADR-0031).
    let e = prod.expect_err("no pipeline");
    assert_eq!(e.code(), ErrorCode::DeployCommitNotAllowed);
    w.t.drop_now().await;
}

#[tokio::test]
async fn rollback_targets_last_approved() {
    let w = world().await;
    w.enable().await;
    assert_eq!(
        rollback_target(&w.t.db, w.project(), DeployTier::Stg)
            .await
            .expect("target"),
        RollbackTarget::NothingRunning
    );
    for (version, commit) in [
        ("v2026.46.1", 'a'),
        ("v2026.47.1", 'b'),
        ("v2026.48.1", 'c'),
    ] {
        w.release(version, commit).await.expect(version);
    }
    w.ran(DeployTier::Stg, 'a', true).await;
    match rollback_target(&w.t.db, w.project(), DeployTier::Stg)
        .await
        .expect("target")
    {
        RollbackTarget::NoEarlier { current } => assert_eq!(current.version, "v2026.46.1"),
        other @ (RollbackTarget::NothingRunning | RollbackTarget::To { .. }) => panic!("{other:?}"),
    }
    // v47 ran without an approval (the environment did not need one then):
    // never a rollback target. v48 runs now.
    w.ran(DeployTier::Stg, 'b', false).await;
    w.ran(DeployTier::Stg, 'c', true).await;
    // An approved run on another environment does not count either.
    w.ran(DeployTier::Dev, 'b', true).await;
    match rollback_target(&w.t.db, w.project(), DeployTier::Stg)
        .await
        .expect("target")
    {
        RollbackTarget::To { current, target } => {
            assert_eq!(current.version, "v2026.48.1");
            assert_eq!(target.version, "v2026.46.1");
        }
        other @ (RollbackTarget::NothingRunning | RollbackTarget::NoEarlier { .. }) => {
            panic!("{other:?}")
        }
    }
    // Back on v46 (a newer deployment of the older release): going back
    // again finds nothing older — never forward to v48.
    w.ran(DeployTier::Stg, 'a', true).await;
    match rollback_target(&w.t.db, w.project(), DeployTier::Stg)
        .await
        .expect("target")
    {
        RollbackTarget::NoEarlier { current } => assert_eq!(current.version, "v2026.46.1"),
        other @ (RollbackTarget::NothingRunning | RollbackTarget::To { .. }) => panic!("{other:?}"),
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_failed_rollback_never_returns_to_the_newer_release() {
    let w = world().await;
    w.enable().await;
    for (version, commit) in [
        ("v2026.46.1", 'a'),
        ("v2026.47.1", 'b'),
        ("v2026.48.1", 'c'),
    ] {
        w.release(version, commit).await.expect(version);
    }
    // v46 then v48 ran on stg; a person approved the rollback to v47.
    w.ran(DeployTier::Stg, 'a', true).await;
    w.ran(DeployTier::Stg, 'c', true).await;
    w.t.exec_superuser(&format!(
        "INSERT INTO forge.deployments (project_id, environment_id, tier, sha, status, \
           requested_by, request_id, approved_by, approved_at, approval_expires_at) \
         SELECT {p}, e.id, 'stg', '{s}', 'queued', {a}, 'req-rollback', {b}, \
           clock_timestamp(), now() + interval '1 hour' \
         FROM forge.deploy_environments e WHERE e.project_id = {p} AND e.tier = 'stg'",
        p = w.project().get(),
        s = sha('b'),
        a = w.alice.get(),
        b = w.bob.get(),
    ))
    .await;
    let claim = super::super::claim_next(&w.t.db, &RequestId::parse("req-claim").expect("id"))
        .await
        .expect("claim");
    let super::super::Claim::Run(claimed) = claim else {
        panic!("{claim:?}");
    };
    assert_eq!(claimed.sha, sha('b'));
    // If the rollback to v47 fails, the worker goes back further (v46),
    // never forward to v48, the release it left.
    assert_eq!(claimed.previous_sha, Some(sha('a')));
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_release_signed_by_a_revoked_key_is_neither_deployed_nor_gone_back_to() {
    let w = world().await;
    w.enable().await;
    w.t.exec_superuser(&format!(
        "SELECT forge.agent_signing_key_manage('register', '1', '{}=', NULL, 'req-key')",
        "A".repeat(43)
    ))
    .await;
    for (version, commit) in [("v2026.47.1", 'a'), ("v2026.48.1", 'b')] {
        w.release(version, commit).await.expect(version);
    }
    w.ran(DeployTier::Dev, 'a', true).await;
    w.ran(DeployTier::Dev, 'b', true).await;
    w.t.exec_superuser(
        "SELECT forge.agent_signing_key_manage('revoke', '1', NULL, 'compromised', 'req-revoke')",
    )
    .await;
    let e = w
        .request_release(DeployTier::Dev, 'a')
        .await
        .expect_err("revoked key");
    assert_eq!(reason(&e), "signing_key_revoked");
    assert!(matches!(
        rollback_target(&w.t.db, w.project(), DeployTier::Dev)
            .await
            .expect("target"),
        RollbackTarget::NoEarlier { .. }
    ));
    w.t.drop_now().await;
}
