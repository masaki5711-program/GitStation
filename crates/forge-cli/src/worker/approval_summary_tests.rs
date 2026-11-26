//! The approval summary pass (M5 L5-8) against a migrated database and a
//! scripted model: one request per set of facts, the event for Kanflow,
//! and nothing about the approval changes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_auth::{prove, AccountKind, Action, Credential, LoadedProject, Principal, ProjectCtx};
use forge_core::deploy::{DeployTier, DeploymentStatus, TargetPolicy};
use forge_core::llm::{ChatFuture, ChatReply, ChatRequest, LlmProvider, ModelRef};
use forge_core::{ObjectId, ProjectAccessLoader as _, ProjectId, Role};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_deploy::{
    ChangeRequest, CommitFacts, CommitSource, EnvironmentSpec, FactsFuture, TargetSpec,
};
use forge_llm::{DbRecords, GatewayConfig};

use super::*;

#[test]
fn the_worker_asks_with_the_policy_the_bench_measures() {
    // One policy (forge_llm::prompts), the same cap as the facts' display.
    assert_eq!(policy(), forge_llm::prompts::approval_summary_policy());
    assert_eq!(
        policy().max_summary_chars,
        forge_deploy::summary::MAX_SUMMARY_CHARS
    );
}

fn rid(s: &str) -> RequestId {
    RequestId::parse(s).expect("request id")
}

/// Every commit is on `main`; no range is readable (the facts say so).
struct OnMain;

impl CommitSource for OnMain {
    fn facts<'a>(&'a self, _: ProjectId, _: &'a ObjectId) -> FactsFuture<'a> {
        Box::pin(async {
            Ok(Some(CommitFacts {
                subject: "Fix the cart".to_owned(),
                author_name: "Carol".to_owned(),
                author_email: "carol@example.test".to_owned(),
                committer_email: "carol@example.test".to_owned(),
                protected_branch: Some("refs/heads/main".to_owned()),
            }))
        })
    }
}

/// A model that answers from a script, counting the calls.
struct Scripted {
    answer: Result<String, ErrorCode>,
    calls: Mutex<usize>,
}

impl LlmProvider for Scripted {
    fn chat<'a>(&'a self, _: &'a ChatRequest) -> ChatFuture<'a> {
        if let Ok(mut n) = self.calls.lock() {
            *n += 1;
        }
        let answer = self.answer.clone();
        Box::pin(async move {
            match answer {
                Ok(content) => Ok(ChatReply {
                    content,
                    input_tokens: 10,
                    output_tokens: 5,
                }),
                Err(code) => Err(ForgeError::new(code).with_context("reason", "scripted")),
            }
        })
    }
}

fn gateway(db: &Db, provider: Arc<Scripted>) -> Gateway {
    Gateway::new(
        GatewayConfig {
            model: ModelRef::parse(&format!("qwen3:27b@sha256:{}", "0".repeat(64))).expect("model"),
            num_ctx: 8192,
            concurrency: 1,
            paused: false,
            call_timeout: Duration::from_secs(5),
        },
        provider,
        Arc::new(DbRecords(db.clone())),
    )
}

struct World {
    t: FreshDb,
    deploy: Deploy,
    project: ProjectId,
    deployment: i64,
}

/// Prod with a target; Bob requested `a…a`, waiting for an approval.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let sys = |s: &str| ChangeContext::new(Actor::System, rid(s));
    let alice = users::create(
        &t.db,
        &sys("r-a"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let bob = users::create(
        &t.db,
        &sys("r-b"),
        &NewUser::human("bob", "bob", "bob@example.test"),
    )
    .await
    .expect("bob");
    let web = projects::create(
        &t.db,
        &sys("r-p"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("project");
    projects::add_member(
        &t.db,
        &sys("r-m"),
        web.project_id,
        bob.user_id,
        Role::Maintainer,
    )
    .await
    .expect("member");
    t.exec_superuser(&format!(
        "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, status, request_id, \
           protected, copy_mark) VALUES ({}, 'refs/heads/main', '{}', 'push', 'succeeded', \
           'req-pipe', true, 'open')",
        web.project_id.get(),
        "a".repeat(40)
    ))
    .await;
    let deploy = Deploy::new(t.db.clone(), Arc::new(OnMain), TargetPolicy::default());
    let proof = |user, action| {
        let db = t.db.clone();
        async move {
            let record = db
                .load_project(web.project_id, Some(user))
                .await
                .expect("load")
                .expect("project");
            let ctx = ProjectCtx::new(LoadedProject {
                id: record.id,
                visibility: record.access.visibility,
                role: record.access.role,
                pending_deletion: record.access.pending_deletion,
            });
            let principal = Principal::authenticated(
                user,
                false,
                true,
                Credential::Session,
                AccountKind::Human,
            );
            prove(&principal, action, ctx).expect("allowed")
        }
    };
    let change = ChangeRequest {
        request_id: rid("r-env"),
        credential_id: None,
    };
    deploy
        .put_environment(
            &change,
            &proof(alice.user_id, Action::DeployEnvironmentManage).await,
            &EnvironmentSpec {
                tier: DeployTier::Prod,
                requires_approval: true,
                approval_ttl_minutes: None,
                auto_rollback: false,
                auto_deploy_ref: None,
                freeze_windows: Vec::new(),
            },
        )
        .await
        .expect("environment");
    let admin = users::create(
        &t.db,
        &sys("r-root"),
        &NewUser {
            is_admin: true,
            ..NewUser::human("root", "root", "root@example.test")
        },
    )
    .await
    .expect("admin");
    let admin_ctx = {
        let record =
            t.db.load_project(web.project_id, Some(admin.user_id))
                .await
                .expect("load")
                .expect("project");
        ProjectCtx::new(LoadedProject {
            id: record.id,
            visibility: record.access.visibility,
            role: record.access.role,
            pending_deletion: record.access.pending_deletion,
        })
    };
    let admin_proof = prove(
        &Principal::authenticated(
            admin.user_id,
            true,
            true,
            Credential::Session,
            AccountKind::Human,
        ),
        Action::DeployTargetManage,
        admin_ctx,
    )
    .expect("an administrator manages targets");
    deploy
        .put_target(
            &ChangeRequest {
                request_id: rid("r-t"),
                credential_id: None,
            },
            &admin_proof,
            DeployTier::Prod,
            &TargetSpec {
                name: "web-1".to_owned(),
                host: "app1.internal".to_owned(),
                port: 22,
                user: "deploy".to_owned(),
                host_key_type: "ssh-ed25519".to_owned(),
                host_key: "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ"
                    .to_owned(),
                script: "web-app".to_owned(),
                enabled: true,
            },
        )
        .await
        .expect("target");
    let d = deploy
        .request(
            &ChangeRequest {
                request_id: rid("r-d"),
                credential_id: None,
            },
            &proof(bob.user_id, Action::DeployCreate).await,
            DeployTier::Prod,
            &"a".repeat(40),
        )
        .await
        .expect("request");
    assert_eq!(d.status, DeploymentStatus::PendingApproval);
    World {
        t,
        deploy,
        project: web.project_id,
        deployment: d.id,
    }
}

async fn events(w: &World, summary: &str) -> i64 {
    let rows = w
        .t
        .json_rows(&format!(
            "SELECT count(*) AS n FROM forge.events WHERE event_type = 'deployment.approval_summarized' AND aggregate_id = {} AND payload->>'summary' = '{summary}'",
            w.deployment
        ))
        .await;
    rows[0]["n"].as_i64().expect("count")
}

#[tokio::test]
async fn one_summary_per_set_of_facts_and_an_event_for_kanflow() {
    let w = world().await;
    let model = Arc::new(Scripted {
        answer: Ok(r#"{"summary": "カートの表示を直します。", "findings": []}"#.to_owned()),
        calls: Mutex::new(0),
    });
    let g = gateway(&w.t.db, Arc::clone(&model));
    let mut memory = Memory::default();
    let first = pass(&g, &w.t.db, &w.deploy, &rid("r-p1"), &mut memory)
        .await
        .expect("pass");
    assert_eq!(first.advised, 1);
    assert_eq!(events(&w, "ready").await, 1);
    // The same facts are not asked again.
    let second = pass(&g, &w.t.db, &w.deploy, &rid("r-p2"), &mut memory)
        .await
        .expect("pass");
    assert_eq!(second.asked, 0);
    assert_eq!(*model.calls.lock().expect("calls"), 1);
    // Nothing about the approval moved.
    let d = w
        .deploy
        .deployment(w.project, w.deployment)
        .await
        .expect("read")
        .expect("deployment");
    assert_eq!(d.status, DeploymentStatus::PendingApproval);
    assert_eq!(d.approvals, 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn llm_down_is_an_unavailable_event_never_a_summary() {
    let w = world().await;
    let model = Arc::new(Scripted {
        answer: Err(ErrorCode::LlmUnavailable),
        calls: Mutex::new(0),
    });
    let g = gateway(&w.t.db, model);
    let mut memory = Memory::default();
    let count = pass(&g, &w.t.db, &w.deploy, &rid("r-p1"), &mut memory)
        .await
        .expect("pass");
    assert_eq!((count.asked, count.advised, count.failed), (1, 0, 1));
    assert_eq!(events(&w, "unavailable").await, 1);
    assert_eq!(events(&w, "ready").await, 0);
    // Not asked again within the pause after a failure.
    let again = pass(&g, &w.t.db, &w.deploy, &rid("r-p2"), &mut memory)
        .await
        .expect("pass");
    assert_eq!(again.asked, 0);
    let label = forge_db::llm::latest_advice(
        &w.t.db,
        SubjectKind::Deployment,
        w.deployment,
        Purpose::ApprovalSummary,
    )
    .await
    .expect("read");
    assert_eq!(label, None, "a failure is never an advice");
    w.t.drop_now().await;
}

/// A model catalog that reports another digest (the gateway stops).
struct OtherDigest;

impl forge_llm::ModelCatalog for OtherDigest {
    fn digest<'a>(&'a self, _: &'a str) -> forge_llm::DigestFuture<'a> {
        Box::pin(async { Ok(Some("1".repeat(64))) })
    }
}

#[tokio::test]
async fn a_failure_before_any_request_row_is_still_an_event_and_bounded() {
    let w = world().await;
    let model = Arc::new(Scripted {
        answer: Ok(r#"{"summary": "x", "findings": []}"#.to_owned()),
        calls: Mutex::new(0),
    });
    let g = gateway(&w.t.db, Arc::clone(&model));
    let stopped = g.verify_model(&OtherDigest).await.expect_err("mismatch");
    assert_eq!(stopped.code(), ErrorCode::LlmModelDigestMismatch);
    let mut memory = Memory::default();
    let count = pass(&g, &w.t.db, &w.deploy, &rid("r-p1"), &mut memory)
        .await
        .expect("pass");
    assert_eq!((count.asked, count.failed), (1, 1));
    let rows = w
        .t
        .json_rows(&format!(
            "SELECT payload FROM forge.events WHERE aggregate_id = {} AND event_type = 'deployment.approval_summarized'",
            w.deployment
        ))
        .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["payload"]["summary"], "unavailable");
    assert_eq!(
        rows[0]["payload"]["llm_request_id"],
        serde_json::Value::Null
    );
    assert_eq!(rows[0]["payload"]["error_code"], "FG-LLM-0003");
    // Not asked again within the pause, though no row bounds it.
    let again = pass(&g, &w.t.db, &w.deploy, &rid("r-p2"), &mut memory)
        .await
        .expect("pass");
    assert_eq!(again.asked, 0);
    assert_eq!(
        *model.calls.lock().expect("calls"),
        0,
        "the model never ran"
    );
    w.t.drop_now().await;
}
