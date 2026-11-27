//! M5 L5-5 (plan 19 §3.1): `GET …/merge-requests/{iid}/ai-review` — the
//! AI first pass, read like the merge request; a confidential advice is
//! withheld from agents; reading never decides anything. M5 L5-6 (plan 19
//! §4, §8.2): `POST …/ai-review/feedback` — a person's 「役に立った /
//! 誤り」 on one finding (`AiAdviceFeedback`), one per person and finding;
//! agents never vote, and the read says who may (`can_feedback`) and what
//! the reader said (`my_feedback`). Synthetic data.

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
    guest: UserId,
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
        let guest = user("gil").await;
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
        projects::add_member(
            &t.db,
            &ctx("req-guest"),
            web.project_id,
            guest.user_id,
            Role::Guest,
        )
        .await
        .expect("guest");
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
            guest: guest.user_id,
            web: web.project_id,
            mr,
        }
    }

    async fn get(&self, who: Principal, header: Option<(&str, &str)>) -> Answer {
        self.send("GET", "", who, header, None).await
    }

    async fn vote(&self, who: Principal, header: Option<(&str, &str)>, body: Value) -> Answer {
        self.send("POST", "/feedback", who, header, Some(body))
            .await
    }

    async fn send(
        &self,
        method: &str,
        suffix: &str,
        who: Principal,
        header: Option<(&str, &str)>,
        body: Option<Value>,
    ) -> Answer {
        let mut builder = Request::builder().method(method).uri(format!(
            "/api/v1/projects/{}/merge-requests/1/ai-review{suffix}",
            self.web.get()
        ));
        if let Some((k, v)) = header {
            builder = builder.header(k, v);
        }
        let body = match body {
            Some(b) => {
                builder = builder.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let mut req = builder.body(body).expect("request");
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

    async fn advice(&self, head: char, class: InputClass) -> i64 {
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
        .expect("advice")
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
    let _ = w.advice('a', InputClass::Confidential).await;
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
    let _ = w.advice('b', InputClass::Internal).await;
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

fn full_token(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(vec![Scope::Api]),
        AccountKind::Human,
    )
}

fn vote_body(advice: i64, index: i64, verdict: &str) -> Value {
    json!({"advice_id": advice, "finding_index": index, "verdict": verdict})
}

#[tokio::test]
async fn a_person_votes_once_per_finding_and_may_change_the_verdict() {
    let w = World::new().await;
    let id = w.advice('a', InputClass::Confidential).await;
    let before = w.get(session(w.developer), None).await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.json);
    assert_eq!(before.json["can_feedback"], true);
    assert_eq!(before.json["advice"]["my_feedback"], json!([]));
    let first = w
        .vote(session(w.developer), None, vote_body(id, 0, "useful"))
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.json);
    assert_eq!(first.json["outcome"], "recorded");
    assert_eq!(first.json["verdict"], "useful");
    assert_eq!(first.json["finding_index"], 0);
    assert_eq!(first.json["advice_id"], id);
    let again = w
        .vote(session(w.developer), None, vote_body(id, 0, "useful"))
        .await;
    assert_eq!(again.json["outcome"], "unchanged", "{}", again.json);
    let changed = w
        .vote(session(w.developer), None, vote_body(id, 0, "wrong"))
        .await;
    assert_eq!(changed.json["outcome"], "changed", "{}", changed.json);
    // A person's own token votes too (plan 19 §8.2).
    let owner = w
        .vote(full_token(w.owner), None, vote_body(id, 0, "useful"))
        .await;
    assert_eq!(owner.status, StatusCode::OK, "{}", owner.json);
    let rows =
        w.t.json_rows("SELECT user_id, finding_index, verdict FROM forge.llm_advice_feedback")
            .await;
    assert_eq!(rows.len(), 2, "one row per person and finding: {rows:?}");
    let after = w.get(session(w.developer), None).await;
    assert_eq!(
        after.json["advice"]["my_feedback"],
        json!([{"finding_index": 0, "verdict": "wrong"}])
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn agents_guests_and_read_tokens_never_vote_and_are_not_offered_it() {
    let w = World::new().await;
    let id = w.advice('a', InputClass::Internal).await;
    let cases: [(Principal, Option<(&str, &str)>); 4] = [
        (token(w.developer).as_agent(true), None),
        (full_token(w.developer).as_agent(true), None),
        (
            full_token(w.developer),
            Some(("forge-context-audience", "agent")),
        ),
        (token(w.developer), None),
    ];
    for (who, header) in cases {
        let read = w.get(who.clone(), header).await;
        assert_eq!(read.status, StatusCode::OK, "{}", read.json);
        assert_eq!(read.json["can_feedback"], false, "{who:?} {header:?}");
        let a = w.vote(who, header, vote_body(id, 0, "wrong")).await;
        assert_eq!(a.status, StatusCode::FORBIDDEN, "{header:?} {}", a.json);
        assert!(a.json["request_id"].is_string(), "{}", a.json);
    }
    // A Guest reads no merge request of a private project, and votes on
    // none of its advice.
    let guest_read = w.get(session(w.guest), None).await;
    assert_eq!(
        guest_read.status,
        StatusCode::FORBIDDEN,
        "{}",
        guest_read.json
    );
    let guest = w
        .vote(session(w.guest), None, vote_body(id, 0, "wrong"))
        .await;
    assert_eq!(guest.status, StatusCode::FORBIDDEN, "{}", guest.json);
    let outsider = w
        .vote(session(w.outsider), None, vote_body(id, 0, "wrong"))
        .await;
    assert_eq!(outsider.status, StatusCode::NOT_FOUND, "{}", outsider.json);
    let rows =
        w.t.json_rows("SELECT id FROM forge.llm_advice_feedback")
            .await;
    assert!(rows.is_empty(), "nothing was counted: {rows:?}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_vote_on_a_missing_advice_or_finding_is_not_found_and_a_bad_body_is_refused() {
    let w = World::new().await;
    let id = w.advice('a', InputClass::Internal).await;
    let missing = w
        .vote(
            session(w.developer),
            None,
            vote_body(id + 1000, 0, "useful"),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.json);
    let beyond = w
        .vote(session(w.developer), None, vote_body(id, 1, "useful"))
        .await;
    assert_eq!(beyond.status, StatusCode::NOT_FOUND, "{}", beyond.json);
    for bad in [
        vote_body(id, 0, "great"),
        vote_body(id, -1, "useful"),
        vote_body(id, 256, "useful"),
        json!({"advice_id": id, "verdict": "useful"}),
    ] {
        let a = w.vote(session(w.developer), None, bad.clone()).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{bad} {}", a.json);
    }
    let rows =
        w.t.json_rows("SELECT id FROM forge.llm_advice_feedback")
            .await;
    assert!(rows.is_empty(), "{rows:?}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_advice_of_another_merge_request_is_not_voted_on_through_this_one() {
    let w = World::new().await;
    let p = w.web.get();
    w.t.commit_audited(
        ("project", p),
        &[
            format!("UPDATE forge.merge_request_counters SET last_iid = 2 WHERE project_id = {p}"),
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch, head_sha) VALUES ({p}, 2, 'other', {}, \
                 'refs/heads/other', 'refs/heads/main', '{}')",
                w.owner.get(),
                "c".repeat(40)
            ),
        ],
    )
    .await;
    let other =
        w.t.json_rows("SELECT id FROM forge.merge_requests WHERE iid = 2")
            .await
            .first()
            .and_then(|r| r["id"].as_i64())
            .expect("mr 2");
    let foreign = World { mr: other, ..w };
    let id = foreign.advice('c', InputClass::Internal).await;
    // The route is merge request 1's; the advice is merge request 2's.
    let a = foreign
        .vote(session(foreign.developer), None, vote_body(id, 0, "useful"))
        .await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "{}", a.json);
    let rows = foreign
        .t
        .json_rows("SELECT id FROM forge.llm_advice_feedback")
        .await;
    assert!(rows.is_empty(), "{rows:?}");
    foreign.t.drop_now().await;
}
