//! M5 L5-5 (plan 19 §3.1): `GET …/merge-requests/{iid}/ai-review` — the
//! AI first pass, read like the merge request; a confidential advice is
//! withheld from agents; reading never decides anything. Synthetic data.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Role, Scope};
use forge_core::checks::{CheckConclusion, CheckStatus, AI_REVIEW_CHECK};
use forge_core::llm::{InputClass, Priority, Purpose, SubjectKind};
use forge_core::{CheckName, ObjectId, ProjectId, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// The route never looks a branch up.
struct NoBranches;

impl forge_review::BranchProbe for NoBranches {
    fn branch_head<'a>(
        &'a self,
        _: forge_core::RepoId,
        _: &'a forge_core::RefName,
        _: &'a str,
    ) -> forge_core::namespace_resolver::LookupFuture<'a, forge_core::ObjectId> {
        Box::pin(async { panic!("no branch is looked up") })
    }
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

struct Answer {
    status: StatusCode,
    json: Value,
}

struct World {
    t: FreshDb,
    owner: UserId,
    developer: UserId,
    outsider: UserId,
    web: ProjectId,
    mr: i64,
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

fn token(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
        AccountKind::Human,
    )
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let user = |name: &'static str| {
            let db = t.db.clone();
            async move {
                users::create(
                    &db,
                    &ctx(&format!("req-user-{name}")),
                    &NewUser::human(name, name, &format!("{name}@example.test")),
                )
                .await
                .expect("user")
            }
        };
        let owner = user("ada").await;
        let developer = user("cy").await;
        let outsider = user("eve").await;
        let web = projects::create(
            &t.db,
            &ctx("req-web"),
            &NewProject::new("web", owner.namespace_id, "Web", owner.user_id),
        )
        .await
        .expect("web");
        projects::add_member(
            &t.db,
            &ctx("req-member"),
            web.project_id,
            developer.user_id,
            Role::Developer,
        )
        .await
        .expect("member");
        let p = web.project_id.get();
        t.commit_audited(
            ("project", p),
            &[
                format!(
                    "INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 1)"
                ),
                format!(
                    "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                     source_branch, target_branch, head_sha) VALUES ({p}, 1, 'synthetic', {}, \
                     'refs/heads/feature', 'refs/heads/main', '{}')",
                    owner.user_id.get(),
                    "a".repeat(40)
                ),
            ],
        )
        .await;
        let mr = t
            .json_rows("SELECT id FROM forge.merge_requests")
            .await
            .first()
            .and_then(|r| r["id"].as_i64())
            .expect("mr");
        Self {
            t,
            owner: owner.user_id,
            developer: developer.user_id,
            outsider: outsider.user_id,
            web: web.project_id,
            mr,
        }
    }

    async fn get(&self, who: Principal, header: Option<(&str, &str)>) -> Answer {
        let mut builder = Request::builder().method("GET").uri(format!(
            "/api/v1/projects/{}/merge-requests/1/ai-review",
            self.web.get()
        ));
        if let Some((k, v)) = header {
            builder = builder.header(k, v);
        }
        let mut req = builder.body(Body::empty()).expect("request");
        req.extensions_mut().insert(who);
        let state = AppState::new(
            "test",
            Vec::new(),
            Arc::new(self.t.db.clone()),
            Lookups {
                namespaces: Arc::new(self.t.db.clone()),
                projects: Arc::new(self.t.db.clone()),
            },
        )
        .with_review(forge_review::Review::new(
            self.t.db.clone(),
            Arc::new(NoBranches),
        ));
        let res = forge_web::app(state, AppConfig::default())
            .expect("router")
            .oneshot(req)
            .await
            .expect("infallible");
        let status = res.status();
        let raw = res
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        Answer {
            status,
            json: serde_json::from_slice(&raw).unwrap_or(Value::Null),
        }
    }

    async fn advice(&self, head: char, class: InputClass) {
        let req = forge_db::llm::insert_request(
            &self.t.db,
            &forge_db::llm::NewRequest {
                purpose: Purpose::MrReview,
                priority: Priority::Review,
                subject_kind: SubjectKind::MergeRequest,
                subject_id: Some(self.mr),
                project: Some(self.web),
                requested_by: None,
                model: "qwen3:27b".to_owned(),
                model_digest: Some("0".repeat(64)),
                prompt_id: "mr_review".to_owned(),
                prompt_version: 1,
                input_sha256: "0".repeat(64),
                input_class: class,
                request_id: RequestId::parse("req-llm").expect("id"),
            },
        )
        .await
        .expect("request");
        forge_db::llm::insert_advice(
            &self.t.db,
            &forge_db::llm::NewAdvice {
                llm_request_id: Some(req),
                purpose: Purpose::MrReview,
                subject_kind: SubjectKind::MergeRequest,
                subject_id: Some(self.mr),
                project: Some(self.web),
                head_sha: Some(head.to_string().repeat(40)),
                summary: "顧客の公差を変えます。".to_owned(),
                findings: json!([{
                    "severity": "high", "path": "src/orders.rs", "line": 41,
                    "quote": "unwrap()", "message": "A missing size panics.", "confidence": 0.9
                }]),
                deep_review: Some(
                    "A deeper review is recommended by code rules: a database migration".to_owned(),
                ),
                dropped_findings: 0,
                degraded: false,
                input_class: class,
                model: "qwen3:27b".to_owned(),
                prompt_version: 1,
            },
        )
        .await
        .expect("advice");
    }
}

#[tokio::test]
async fn no_advice_is_said_plainly_and_an_outsider_sees_nothing() {
    let w = World::new().await;
    let a = w.get(session(w.developer), None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["iid"], 1);
    assert_eq!(a.json["mode"], "off");
    assert_eq!(a.json["check"], Value::Null);
    assert_eq!(a.json["advice"]["state"], "none");
    assert_eq!(a.json["advice"]["note"], "AI の助言は今ありません");
    assert!(a.json["advice"]["label"]
        .as_str()
        .expect("label")
        .starts_with("AI の助言（提案）"));
    let none = w.get(session(w.outsider), None).await;
    assert_eq!(none.status, StatusCode::NOT_FOUND, "{}", none.json);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_confidential_advice_is_read_by_people_and_withheld_from_agents() {
    let w = World::new().await;
    w.t.exec_superuser(&format!(
        "INSERT INTO forge.project_llm (project_id, review, changed_by) VALUES ({}, 'advisory', {})",
        w.web.get(),
        w.owner.get()
    ))
    .await;
    forge_db::test_support::record_forge_check(
        &w.t.db,
        w.web,
        &ObjectId::parse(&"a".repeat(40)).expect("sha"),
        &forge_db::checks::NewCheckRun {
            name: CheckName::parse(AI_REVIEW_CHECK).expect("name"),
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Neutral),
            summary: "synthetic".to_owned(),
            details_url: None,
            annotations: Vec::new(),
        },
    )
    .await;
    w.advice('a', InputClass::Confidential).await;
    let person = w.get(session(w.developer), None).await;
    assert_eq!(person.status, StatusCode::OK, "{}", person.json);
    assert_eq!(person.json["mode"], "advisory");
    assert_eq!(person.json["check"]["conclusion"], "neutral");
    assert_eq!(person.json["advice"]["state"], "shown");
    assert_eq!(person.json["advice"]["stale"], false);
    assert_eq!(person.json["advice"]["summary"], "顧客の公差を変えます。");
    assert_eq!(person.json["advice"]["findings"][0]["severity"], "high");
    assert_eq!(person.json["advice"]["findings"][0]["quote"], "unwrap()");
    assert!(person.json["advice"]["deep_review"]
        .as_str()
        .expect("deep")
        .contains("migration"));
    assert_eq!(person.json["advice"]["model"], "qwen3:27b");
    for (who, header) in [
        (token(w.developer).as_agent(true), None),
        (
            token(w.developer),
            Some(("forge-context-audience", "agent")),
        ),
    ] {
        let agent = w.get(who, header).await;
        assert_eq!(agent.status, StatusCode::OK, "{}", agent.json);
        assert_eq!(agent.json["advice"]["state"], "withheld");
        assert_eq!(agent.json["advice"]["note"], "機密の入力のため表示しません");
        assert_eq!(agent.json["advice"]["summary"], Value::Null);
        assert!(
            !agent.json.to_string().contains("顧客"),
            "the words never reach an agent"
        );
        assert!(!agent.json.to_string().contains("panics"));
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_advice_for_another_head_is_stale() {
    let w = World::new().await;
    w.advice('b', InputClass::Internal).await;
    let agent = w.get(token(w.developer).as_agent(true), None).await;
    assert_eq!(agent.status, StatusCode::OK, "{}", agent.json);
    assert_eq!(
        agent.json["advice"]["state"], "shown",
        "internal: agents read it"
    );
    assert_eq!(agent.json["advice"]["stale"], true);
    assert_eq!(agent.json["advice"]["head_sha"], "b".repeat(40));
    assert_eq!(agent.json["head_sha"], "a".repeat(40));
    w.t.drop_now().await;
}
