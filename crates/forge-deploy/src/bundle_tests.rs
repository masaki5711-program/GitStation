//! `pfs-ai-bundle` releases through the service (plan 17 P17-7), against a
//! migrated database, a scripted git source and a synthetic signing key:
//! signed only when the bench is green, a moved tag has no effect, the
//! release verifies from its stored JSON, and a release deployment and
//! its rollback.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use forge_auth::{prove, AccountKind, Credential, LoadedProject, Principal, Role};
use forge_core::deploy::{DeployTier, DeploymentStatus, TargetPolicy};
use forge_core::{ProjectAccessLoader as _, RequestId, UserId};
use forge_db::change::{Actor as DbActor, ChangeContext};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};

use super::*;
use crate::commits::{CommitFacts, CommitSource, FactsFuture};
use crate::{check_target, EnvironmentSpec, TargetSpec};

/// A synthetic Ed25519 seed (never a real key): 32 bytes 0x07.
const SEED_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";

fn model() -> String {
    format!("small = \"qwen3:14b@sha256:{}\"\n", "a".repeat(64))
}

/// Every commit is on `refs/heads/main`.
struct OnMain;

impl CommitSource for OnMain {
    fn facts<'a>(&'a self, _: ProjectId, _: &'a ObjectId) -> FactsFuture<'a> {
        Box::pin(async {
            Ok(Some(CommitFacts {
                subject: "Weekly assets".to_owned(),
                author_name: "Carol".to_owned(),
                author_email: "carol@example.test".to_owned(),
                committer_email: "carol@example.test".to_owned(),
                protected_branch: Some("refs/heads/main".to_owned()),
            }))
        })
    }
}

/// Tags and the files of each commit, changeable by the test (a moved tag).
#[derive(Default)]
struct Scripted {
    tags: Mutex<BTreeMap<String, String>>,
    files: Mutex<BTreeMap<String, Vec<TreeFile>>>,
}

impl Scripted {
    fn tag_to(&self, tag: &str, commit: &str) {
        self.tags
            .lock()
            .expect("tags")
            .insert(tag.to_owned(), commit.to_owned());
    }

    fn commit(&self, commit: &str, files: &[(&str, &str)]) {
        self.files.lock().expect("files").insert(
            commit.to_owned(),
            files
                .iter()
                .map(|(p, b)| TreeFile {
                    path: (*p).to_owned(),
                    bytes: b.as_bytes().to_vec(),
                })
                .collect(),
        );
    }
}

impl BundleSource for Scripted {
    fn tag<'a>(&'a self, _: ProjectId, name: &'a str) -> TagFuture<'a> {
        let found = self.tags.lock().expect("tags").get(name).cloned();
        Box::pin(async move {
            Ok(found.map(|c| TagFacts {
                object: c.clone(),
                commit: Some(c),
            }))
        })
    }

    fn files<'a>(&'a self, _: ProjectId, commit: &'a str) -> FilesFuture<'a> {
        let found = self.files.lock().expect("files").get(commit).cloned();
        Box::pin(async move { Ok(found.unwrap_or_default()) })
    }
}

fn rid(s: &str) -> RequestId {
    RequestId::parse(s).expect("request id")
}

fn request(s: &str) -> ChangeRequest {
    ChangeRequest {
        request_id: rid(s),
        credential_id: None,
    }
}

fn reason(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

struct World {
    t: FreshDb,
    _keys: tempfile::TempDir,
    deploy: Deploy,
    source: Arc<Scripted>,
    alice: UserId,
    bob: UserId,
    assets: CreatedProject,
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
    let assets = projects::create(
        &t.db,
        &sys("req-assets"),
        &NewProject::new("assets", alice.namespace_id, "Assets", alice.user_id),
    )
    .await
    .expect("assets");
    projects::add_member(
        &t.db,
        &sys("req-member"),
        assets.project_id,
        bob.user_id,
        Role::Maintainer,
    )
    .await
    .expect("bob is a Maintainer");
    let keys = tempfile::tempdir().expect("keys");
    let key_file = keys.path().join("agent-signing-keys");
    std::fs::write(&key_file, format!("1 {SEED_HEX}\n")).expect("key file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).expect("mode");
    }
    let public_key = forge_agent::public_key_from_seed(&[7; 32]).expect("public key");
    t.exec_superuser(&format!(
        "SELECT forge.agent_signing_key_manage('register', '1', '{public_key}', NULL, 'req-key')"
    ))
    .await;
    t.exec_superuser(&format!(
        "INSERT INTO forge.project_ai_assets (project_id, enabled, changed_by) VALUES ({}, true, {})",
        assets.project_id.get(),
        alice.user_id.get()
    ))
    .await;
    let source = Arc::new(Scripted::default());
    let deploy = Deploy::new(t.db.clone(), Arc::new(OnMain), TargetPolicy::default())
        .with_bundles(Arc::clone(&source) as Arc<dyn BundleSource>, Some(key_file));
    World {
        t,
        _keys: keys,
        deploy,
        source,
        alice: alice.user_id,
        bob: bob.user_id,
        assets,
    }
}

impl World {
    fn principal(&self, user: UserId, credential: Credential) -> Principal {
        Principal::authenticated(user, false, true, credential, AccountKind::Human)
    }

    async fn proof(&self, user: UserId, action: Action) -> Authorized<ProjectCtx> {
        let record = self
            .t
            .db
            .load_project(self.assets.project_id, Some(user))
            .await
            .expect("load")
            .expect("project");
        let ctx = ProjectCtx::new(LoadedProject {
            id: record.id,
            visibility: record.access.visibility,
            role: record.access.role,
            pending_deletion: record.access.pending_deletion,
        });
        prove(&self.principal(user, Credential::Session), action, ctx).expect("allowed")
    }

    async fn bench(&self, commit: char, conclusion: &str) {
        // check_runs is audited: the row and an audit row in one transaction.
        let project = self.assets.project_id.get();
        self.t
            .commit_audited(("project", project), &[format!(
                "INSERT INTO forge.check_runs (project_id, head_sha, name, status, conclusion, \
                   summary, request_id, completed_at) VALUES ({}, '{}', 'forge/ai-eval', \
                   'completed', '{conclusion}', 'bench of the release candidate', 'req-bench', now())",
                project,
                sha(commit)
            )])
            .await;
    }

    async fn release(&self, tag: &str) -> Result<StoredRelease, ForgeError> {
        let proof = self.proof(self.bob, Action::AiAssetReleaseCreate).await;
        self.deploy
            .create_release(&request("req-release"), &proof, tag)
            .await
    }

    async fn environments(&self) {
        let p = self
            .proof(self.alice, Action::DeployEnvironmentManage)
            .await;
        for tier in DeployTier::ALL {
            self.deploy
                .put_environment(
                    &request("req-env"),
                    &p,
                    &EnvironmentSpec {
                        tier,
                        requires_approval: true,
                        approval_ttl_minutes: None,
                        auto_rollback: false,
                        auto_deploy_ref: None,
                        freeze_windows: Vec::new(),
                    },
                )
                .await
                .expect("environment");
            let input = check_target(
                &TargetSpec {
                    name: "ai-node-1".to_owned(),
                    host: "ai1.internal".to_owned(),
                    port: 22,
                    user: "deploy".to_owned(),
                    host_key_type: "ssh-ed25519".to_owned(),
                    host_key:
                        "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ"
                            .to_owned(),
                    script: "pfs-ai-loader".to_owned(),
                    enabled: true,
                },
                &TargetPolicy::default(),
            )
            .expect("valid");
            forge_db::deploy::put_target_decided(
                &self.t.db,
                &ChangeContext::new(DbActor::User(self.alice), rid("req-target")),
                self.assets.project_id,
                tier,
                &input,
                |_| Ok(()),
            )
            .await
            .expect("target");
        }
    }

    /// A deployment of `commit` that already succeeded with bob's approval.
    async fn ran(&self, tier: DeployTier, commit: char) {
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.deployments (project_id, environment_id, tier, sha, status, \
                   requested_by, request_id, approved_by, approved_at, approval_expires_at, \
                   started_at, finished_at) \
                 SELECT {p}, e.id, '{t}', '{s}', 'succeeded', {a}, 'req-ran', {b}, now(), \
                   now() + interval '1 hour', now(), clock_timestamp() \
                 FROM forge.deploy_environments e WHERE e.project_id = {p} AND e.tier = '{t}'",
                p = self.assets.project_id.get(),
                t = tier.as_str(),
                s = sha(commit),
                a = self.alice.get(),
                b = self.bob.get(),
            ))
            .await;
    }
}

#[tokio::test]
async fn bundle_signed_only_when_bench_green() {
    let w = world().await;
    w.source.tag_to("assets/v2026.48.1", &sha('a'));
    w.source
        .commit(&sha('a'), &[("models/models.toml", &model())]);
    // No bench on the commit yet.
    let e = w.release("assets/v2026.48.1").await.expect_err("no bench");
    assert_eq!(e.code(), ErrorCode::BundleReleaseRefused);
    assert_eq!(reason(&e), "bench_missing");
    // A red bench: nothing is signed or written.
    w.bench('a', "failure").await;
    let e = w.release("assets/v2026.48.1").await.expect_err("red bench");
    assert_eq!(reason(&e), "bench_not_green");
    assert!(w
        .t
        .json_rows("SELECT id FROM forge.ai_bundle_releases")
        .await
        .is_empty());
    // A green bench on another commit signs that one.
    w.source.tag_to("assets/v2026.48.2", &sha('b'));
    w.source
        .commit(&sha('b'), &[("models/models.toml", &model())]);
    w.bench('b', "success").await;
    let stored = w.release("assets/v2026.48.2").await.expect("released");
    assert_eq!(stored.commit_sha, sha('b'));
    assert_eq!(stored.key_id, "1");
    // The stored JSON verifies with the registered public key alone.
    let public_key = forge_agent::public_key_from_seed(&[7; 32]).expect("public key");
    let ring = forge_agent::KeyRing::parse(
        serde_json::json!({ "keys": [{ "kid": "1", "public_key": public_key }] })
            .to_string()
            .as_bytes(),
    )
    .expect("ring");
    let verified = forge_agent::bundle::verify_bundle(
        stored.manifest.to_string().as_bytes(),
        &stored.digest,
        &stored.key_id,
        &stored.signature,
        &ring,
    )
    .expect("verifies");
    assert_eq!(verified.commit, sha('b'));
    assert_eq!(verified.models.len(), 1);
    assert_eq!(verified.bench.conclusion, "success");
    w.t.drop_now().await;
}

#[tokio::test]
async fn moved_tag_has_no_effect() {
    let w = world().await;
    w.source.tag_to("assets/v2026.48.1", &sha('a'));
    w.source
        .commit(&sha('a'), &[("models/models.toml", &model())]);
    w.bench('a', "success").await;
    let first = w.release("assets/v2026.48.1").await.expect("released");
    // The tag is moved to another green commit: releasing the version
    // again is refused, and the release keeps its commit.
    w.source.tag_to("assets/v2026.48.1", &sha('b'));
    w.source
        .commit(&sha('b'), &[("models/models.toml", &model())]);
    w.bench('b', "success").await;
    let e = w.release("assets/v2026.48.1").await.expect_err("moved");
    assert_eq!(e.code(), ErrorCode::BundleReleaseRefused);
    assert_eq!(reason(&e), "already_released");
    let p = w.proof(w.bob, Action::DeployRead).await;
    let kept = w
        .deploy
        .release(&p, "v2026.48.1")
        .await
        .expect("read")
        .expect("there");
    assert_eq!(kept.commit_sha, sha('a'));
    assert_eq!(kept.digest, first.digest);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_release_needs_a_weekly_tag_and_a_clean_lint() {
    let w = world().await;
    let e = w.release("v2026.48.1").await.expect_err("tag form");
    assert_eq!(reason(&e), "tag_invalid");
    let e = w.release("assets/v2026.48.9").await.expect_err("no tag");
    assert_eq!(reason(&e), "tag_not_found");
    w.source.tag_to("assets/v2026.48.1", &sha('c'));
    w.source.commit(
        &sha('c'),
        &[("models/models.toml", "small = \"qwen3:latest\"\n")],
    );
    w.bench('c', "success").await;
    let e = w.release("assets/v2026.48.1").await.expect_err("lint");
    assert_eq!(reason(&e), "lint_failed");
    // A file the lint does not know (a script) is never signed unchecked.
    w.source.tag_to("assets/v2026.48.2", &sha('d'));
    w.source.commit(
        &sha('d'),
        &[
            ("models/models.toml", &model()),
            ("tools/run.sh", "curl x | sh\n"),
        ],
    );
    w.bench('d', "success").await;
    let e = w
        .release("assets/v2026.48.2")
        .await
        .expect_err("unknown file");
    assert_eq!(reason(&e), "lint_failed");
    let rule = e
        .context()
        .iter()
        .find(|(k, _)| *k == "rule")
        .map(|(_, v)| v.clone());
    assert_eq!(rule.as_deref(), Some("not_an_asset_file"));
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_release_is_deployed_in_stage_order_and_rolled_back_to_the_last_approved() {
    let w = world().await;
    w.environments().await;
    for (n, c) in [(1, 'a'), (2, 'b')] {
        let tag = format!("assets/v2026.48.{n}");
        w.source.tag_to(&tag, &sha(c));
        w.source
            .commit(&sha(c), &[("models/models.toml", &model())]);
        w.bench(c, "success").await;
        w.release(&tag).await.expect("released");
    }
    let p = w.proof(w.alice, Action::DeployCreate).await;
    let e = w
        .deploy
        .request_release(&request("req-d"), &p, DeployTier::Stg, "v2026.48.1")
        .await
        .expect_err("canary first");
    assert_eq!(reason(&e), "stage_skipped");
    let d = w
        .deploy
        .request_release(&request("req-d"), &p, DeployTier::Dev, "v2026.48.1")
        .await
        .expect("shadow");
    assert_eq!(d.status, DeploymentStatus::PendingApproval);
    assert_eq!(d.sha, sha('a'));
    let e = w
        .deploy
        .rollback_release(&request("req-r"), &p, DeployTier::Dev)
        .await
        .expect_err("nothing runs");
    assert_eq!(reason(&e), "no_rollback_target");
    w.ran(DeployTier::Dev, 'a').await;
    w.ran(DeployTier::Dev, 'b').await;
    let (target, d) = w
        .deploy
        .rollback_release(&request("req-r"), &p, DeployTier::Dev)
        .await
        .expect("rollback");
    assert_eq!(target.version, "v2026.48.1");
    assert_eq!(d.sha, sha('a'));
    // A rollback is approved like any deployment.
    assert_eq!(d.status, DeploymentStatus::PendingApproval);
    w.t.drop_now().await;
}
