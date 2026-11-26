#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Deployments through the product router (M4 C11, plan 14 §4.2,
//! ADR-0031) against the development database: a Maintainer sets the
//! environments, an administrator the targets; a Developer (an agent
//! included) requests; prod is approved only by a person in a web
//! session who is neither the requester nor an agent, a service account
//! or a token; a freeze window refuses the request. Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Role, Scope};
use forge_core::{ProjectId, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// The delegation routes never look a branch up.
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

/// Every commit is on `refs/heads/main` (synthetic).
struct OnMain;

impl forge_deploy::CommitSource for OnMain {
    fn facts<'a>(
        &'a self,
        _: ProjectId,
        _: &'a forge_core::ObjectId,
    ) -> forge_deploy::FactsFuture<'a> {
        Box::pin(async {
            Ok(Some(forge_deploy::CommitFacts {
                subject: "Fix the cart".to_owned(),
                author_name: "Carol".to_owned(),
                author_email: "carol@example.test".to_owned(),
                committer_email: "carol@example.test".to_owned(),
                protected_branch: Some("refs/heads/main".to_owned()),
            }))
        })
    }
}

const HOST_KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ";

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

struct Answer {
    status: StatusCode,
    json: Value,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }
}

struct World {
    t: FreshDb,
    owner: UserId,
    maintainer: UserId,
    developer: UserId,
    admin: UserId,
    web: ProjectId,
}

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Session, AccountKind::Human)
}

fn token(user: UserId, scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::Human,
    )
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let user = |name: &'static str, admin: bool| {
            let db = t.db.clone();
            async move {
                users::create(
                    &db,
                    &ctx(&format!("req-user-{name}")),
                    &NewUser {
                        is_admin: admin,
                        ..NewUser::human(name, name, &format!("{name}@example.test"))
                    },
                )
                .await
                .expect("user")
            }
        };
        let owner = user("ada", false).await;
        let maintainer = user("bob", false).await;
        let developer = user("cy", false).await;
        let admin = user("root", true).await;
        let web = projects::create(
            &t.db,
            &ctx("req-web"),
            &NewProject::new("web", owner.namespace_id, "Web", owner.user_id),
        )
        .await
        .expect("web");
        for (u, role) in [
            (maintainer.user_id, Role::Maintainer),
            (developer.user_id, Role::Developer),
        ] {
            projects::add_member(&t.db, &ctx("req-member"), web.project_id, u, role)
                .await
                .expect("member");
        }
        Self {
            t,
            owner: owner.user_id,
            maintainer: maintainer.user_id,
            developer: developer.user_id,
            admin: admin.user_id,
            web: web.project_id,
        }
    }

    async fn call(&self, method: &str, uri: &str, who: Principal, body: Option<Value>) -> Answer {
        let builder = Request::builder().method(method).uri(uri);
        let mut req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("request");
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
        .with_deploy(forge_deploy::Deploy::new(
            self.t.db.clone(),
            Arc::new(OnMain),
            forge_core::deploy::TargetPolicy::default(),
        ))
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

    /// A GET with one extra header (M5 L5-8: the audience).
    async fn call_with_header(&self, uri: &str, who: Principal, header: (&str, &str)) -> Answer {
        let mut req = Request::builder()
            .method("GET")
            .uri(uri)
            .header(header.0, header.1)
            .body(Body::empty())
            .expect("request");
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
        .with_deploy(forge_deploy::Deploy::new(
            self.t.db.clone(),
            Arc::new(OnMain),
            forge_core::deploy::TargetPolicy::default(),
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

    fn uri(&self, tail: &str) -> String {
        format!("/api/v1/projects/{}/{tail}", self.web.get())
    }

    async fn environment(&self, tier: &str, body: Value) -> Answer {
        self.call(
            "PUT",
            &self.uri(&format!("deploy/environments/{tier}")),
            session(self.owner, false),
            Some(body),
        )
        .await
    }

    async fn setup(&self) {
        // prod deploys only a SHA whose push pipeline succeeded.
        for c in ['a', 'b'] {
            self.t
                .exec_superuser(&format!(
                    "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, status, \
                       request_id, protected, copy_mark) VALUES ({}, 'refs/heads/main', '{}', \
                       'push', 'succeeded', 'req-pipe', true, 'open')",
                    self.web.get(),
                    c.to_string().repeat(40)
                ))
                .await;
        }
        for tier in ["stg", "prod"] {
            let prod = tier == "prod";
            let a = self
                .environment(
                    tier,
                    json!({"requires_approval": prod, "auto_rollback": !prod}),
                )
                .await;
            assert_eq!(a.status, StatusCode::CREATED, "{}", a.json);
            let t = self
                .call(
                    "PUT",
                    &self.uri(&format!("deploy/environments/{tier}/targets/web-1")),
                    session(self.admin, true),
                    Some(json!({
                        "host": "app1.internal", "user": "deploy",
                        "host_key_type": "ssh-ed25519", "host_key": HOST_KEY,
                        "script": "web-app"
                    })),
                )
                .await;
            assert_eq!(t.status, StatusCode::CREATED, "{}", t.json);
            assert!(t.json["host_key_fingerprint"]
                .as_str()
                .expect("fingerprint")
                .starts_with("SHA256:"));
            assert!(
                t.json.get("host_key").is_none(),
                "only the fingerprint is answered"
            );
        }
    }

    async fn request(&self, who: Principal, tier: &str, sha: char) -> Answer {
        self.call(
            "POST",
            &self.uri("deployments"),
            who,
            Some(json!({"environment": tier, "sha": sha.to_string().repeat(40)})),
        )
        .await
    }

    async fn approve(&self, who: Principal, id: i64) -> Answer {
        self.call(
            "POST",
            &self.uri(&format!("deployments/{id}/approve")),
            who,
            None,
        )
        .await
    }
}

#[tokio::test]
async fn only_a_person_in_a_session_approves_prod_and_never_their_own_request() {
    let w = World::new().await;
    w.setup().await;
    // A Developer (an agent's token too) may request; prod waits.
    let agent = token(w.developer, &[Scope::Api]).as_agent(true);
    let d = w.request(agent, "prod", 'a').await;
    assert_eq!(d.status, StatusCode::CREATED, "{}", d.json);
    assert_eq!(d.json["status"], "pending_approval");
    let id = d.json["id"].as_i64().expect("id");
    // Any token (an agent's too) is refused: approval is a web session.
    let a = w
        .approve(token(w.maintainer, &[Scope::Api]).as_agent(true), id)
        .await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002")
    );
    let a = w.approve(token(w.maintainer, &[Scope::Api]), id).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002")
    );
    // The Developer requested it: the requester never approves (before
    // any role).
    let a = w.approve(session(w.developer, false), id).await;
    assert_eq!(a.code(), "FG-DEPLOY-0009");
    // A Maintainer in a session approves; a second approval conflicts.
    let a = w.approve(session(w.maintainer, false), id).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["status"], "queued");
    assert_eq!(a.json["approved_by"], w.maintainer.get());
    let again = w.approve(session(w.owner, false), id).await;
    assert_eq!(
        (again.status, again.code()),
        (StatusCode::CONFLICT, "FG-DEPLOY-0010")
    );
    // The requester never approves their own.
    let mine = w.request(session(w.maintainer, false), "prod", 'b').await;
    let mine_id = mine.json["id"].as_i64().expect("id");
    let own = w.approve(session(w.maintainer, false), mine_id).await;
    assert_eq!(
        (own.status, own.code()),
        (StatusCode::FORBIDDEN, "FG-DEPLOY-0009")
    );
    // An unknown deployment is the invisible 404.
    let none = w.approve(session(w.owner, false), 999_999).await;
    assert_eq!(none.code(), "FG-REPO-0005");
    // The list shows both.
    let list = w
        .call(
            "GET",
            &w.uri("deployments"),
            session(w.developer, false),
            None,
        )
        .await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.json["deployments"].as_array().expect("list").len(), 2);
    w.t.drop_now().await;
}

#[tokio::test]
async fn targets_are_administrators_and_environments_are_maintainers() {
    let w = World::new().await;
    w.setup().await;
    let target_uri = w.uri("deploy/environments/stg/targets/web-2");
    let owner = w
        .call(
            "PUT",
            &target_uri,
            session(w.owner, false),
            Some(json!({
                "host": "app2.internal", "user": "deploy",
                "host_key_type": "ssh-ed25519", "host_key": HOST_KEY, "script": "web-app"
            })),
        )
        .await;
    assert_eq!(owner.code(), "FG-AUTH-0003", "an Owner is no administrator");
    let bad = w
        .call(
            "PUT",
            &w.uri("deploy/environments/stg/targets/web-2"),
            session(w.admin, true),
            Some(json!({
                "host": "-oProxyCommand=sh", "user": "deploy",
                "host_key_type": "ssh-ed25519", "host_key": HOST_KEY, "script": "web-app"
            })),
        )
        .await;
    assert_eq!(
        (bad.status, bad.code()),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    let env = w
        .call(
            "PUT",
            &w.uri("deploy/environments/dev"),
            token(w.maintainer, &[Scope::Api]).as_agent(true),
            Some(json!({"requires_approval": false, "auto_rollback": true})),
        )
        .await;
    assert_eq!(
        env.code(),
        "FG-AUTH-0003",
        "an agent never sets an environment"
    );
    let prod = w
        .environment(
            "prod",
            json!({"requires_approval": true, "auto_rollback": true}),
        )
        .await;
    assert_eq!(
        (prod.status, prod.code()),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    // A freeze window refuses the request.
    let frozen = w
        .environment(
            "stg",
            json!({"requires_approval": false, "auto_rollback": true, "freeze_windows": [
                {"starts_at": "2000-01-01T00:00:00Z", "ends_at": "2100-01-01T00:00:00Z"}
            ]}),
        )
        .await;
    assert_eq!(frozen.status, StatusCode::OK, "{}", frozen.json);
    assert_eq!(frozen.json["frozen_now"], true);
    let refused = w.request(session(w.developer, false), "stg", 'c').await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::CONFLICT, "FG-DEPLOY-0002")
    );
    let read = w
        .call(
            "GET",
            &w.uri("deploy/environments"),
            session(w.developer, false),
            None,
        )
        .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.json["environments"].as_array().expect("envs").len(), 2);
    w.t.drop_now().await;
}

/// ADR-0038 §3.4–§3.6 (step C): an Owner names the approvers; then only
/// they approve (a named Reporter-level person included), two approvals
/// are needed, a delegate approves for a named person only where the
/// environment allows it.
#[tokio::test]
async fn a_person_outside_the_environment_approvers_cannot_approve() {
    let w = World::new().await;
    w.setup().await;
    // A Maintainer may not change the approvers (an Owner's grant).
    let body = json!({"users": ["bob", "cy"], "approvals_required": 2, "allow_delegation": true});
    let refused = w
        .call(
            "PUT",
            &w.uri("deploy/environments/prod/approvers"),
            session(w.maintainer, false),
            Some(body.clone()),
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.json);
    let set = w
        .call(
            "PUT",
            &w.uri("deploy/environments/prod/approvers"),
            session(w.owner, false),
            Some(body),
        )
        .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.json);
    assert_eq!(set.json["approvals_required"], 2);
    assert_eq!(set.json["users"].as_array().expect("users").len(), 2);
    // A bad selector is the caller's 400.
    let bad = w
        .call(
            "PUT",
            &w.uri("deploy/environments/prod/approvers"),
            session(w.owner, false),
            Some(json!({"selectors": [{"relation": "boss"}], "approvals_required": 1})),
        )
        .await;
    assert_eq!(
        (bad.status, bad.code()),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    let list = w
        .call(
            "GET",
            &w.uri("deploy/approvers"),
            session(w.developer, false),
            None,
        )
        .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.json);
    let prod = list.json["environments"]
        .as_array()
        .expect("environments")
        .iter()
        .find(|e| e["tier"] == "prod")
        .expect("prod")
        .clone();
    assert_eq!(prod["unrestricted"], false);
    // The owner (a Maintainer and more) is not named: refused.
    // Dee, a Developer, requests it (never one of the approvers).
    let dee = users::create(
        &w.t.db,
        &ctx("req-user-dee"),
        &NewUser::human("dee", "dee", "dee@example.test"),
    )
    .await
    .expect("dee")
    .user_id;
    projects::add_member(&w.t.db, &ctx("req-member-dee"), w.web, dee, Role::Developer)
        .await
        .expect("member");
    let d = w.request(session(dee, false), "prod", 'a').await;
    assert_eq!(d.status, StatusCode::CREATED, "{}", d.json);
    let id = d.json["id"].as_i64().expect("id");
    let owner = w.approve(session(w.owner, false), id).await;
    assert_eq!(
        (owner.status, owner.code()),
        (StatusCode::FORBIDDEN, "FG-DEPLOY-0014")
    );
    // Bob (named) approves: one of two.
    let first = w.approve(session(w.maintainer, false), id).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.json);
    assert_eq!(first.json["status"], "pending_approval");
    assert_eq!(first.json["approvals"], 1);
    // Cy is a Developer, named: allowed; the deployment is queued.
    let second = w.approve(session(w.developer, false), id).await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.json);
    assert_eq!(second.json["status"], "queued");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_delegation_is_the_persons_own_and_set_in_a_session() {
    let w = World::new().await;
    let path = "/api/v1/user/approval-delegation";
    let empty = w
        .call("GET", path, session(w.maintainer, false), None)
        .await;
    assert_eq!(empty.status, StatusCode::OK, "{}", empty.json);
    assert!(empty.json["mine"].is_null());
    let body = json!({
        "delegate": "cy",
        "starts_at": "2026-11-20T00:00:00Z",
        "ends_at": "2026-12-31T00:00:00Z",
    });
    // A token never sets one.
    let token_set = w
        .call(
            "PUT",
            path,
            token(w.maintainer, &[Scope::Api]),
            Some(body.clone()),
        )
        .await;
    assert_eq!(
        (token_set.status, token_set.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002")
    );
    // Longer than 30 days: the caller's 400.
    let long = w
        .call("PUT", path, session(w.maintainer, false), Some(body))
        .await;
    assert_eq!(
        (long.status, long.code()),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    w.t.drop_now().await;
}

/// M5 L5-8: a confidential approval summary of deployment `id` (`sha`).
async fn confidential_summary(w: &World, id: i64, sha: &str) {
    use forge_core::llm::{InputClass, Priority, Purpose, SubjectKind};
    let req = forge_db::llm::insert_request(
        &w.t.db,
        &forge_db::llm::NewRequest {
            purpose: Purpose::ApprovalSummary,
            priority: Priority::Interactive,
            subject_kind: SubjectKind::Deployment,
            subject_id: Some(id),
            project: Some(w.web),
            requested_by: None,
            model: "qwen3:27b".to_owned(),
            model_digest: Some("0".repeat(64)),
            prompt_id: "approval_summary".to_owned(),
            prompt_version: 1,
            input_sha256: "0".repeat(64),
            input_class: InputClass::Confidential,
            request_id: RequestId::parse("req-llm").expect("id"),
        },
    )
    .await
    .expect("request");
    forge_db::llm::insert_advice(
        &w.t.db,
        &forge_db::llm::NewAdvice {
            llm_request_id: Some(req),
            purpose: Purpose::ApprovalSummary,
            subject_kind: SubjectKind::Deployment,
            subject_id: Some(id),
            project: Some(w.web),
            head_sha: Some(sha.to_owned()),
            summary: "顧客の表を変えます。".to_owned(),
            findings: json!([]),
            deep_review: None,
            dropped_findings: 0,
            degraded: false,
            input_class: InputClass::Confidential,
            model: "qwen3:27b".to_owned(),
            prompt_version: 1,
        },
    )
    .await
    .expect("advice");
}

#[tokio::test]
async fn the_approval_summary_lists_the_facts_and_withholds_confidential_words_from_agents() {
    let w = World::new().await;
    w.setup().await;
    let d = w.request(session(w.developer, false), "prod", 'a').await;
    let id = d.json["id"].as_i64().expect("id");
    let uri = w.uri(&format!("deployments/{id}/approval-summary"));
    // No summary yet: the facts alone, and the screen's sentence.
    let a = w.call("GET", &uri, session(w.developer, false), None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["deployment"]["status"], "pending_approval");
    assert_eq!(a.json["facts"]["range"], "first_deployment");
    assert_eq!(a.json["summary"]["state"], "none");
    assert_eq!(a.json["summary"]["note"], "AI の助言は今ありません");
    assert_eq!(a.json["summary"]["label"], "AI の助言（提案）");
    // A confidential summary: a person reads it, an agent never does.
    confidential_summary(&w, id, &"a".repeat(40)).await;
    let person = w.call("GET", &uri, session(w.developer, false), None).await;
    assert_eq!(person.json["summary"]["state"], "shown", "{}", person.json);
    assert_eq!(person.json["summary"]["text"], "顧客の表を変えます。");
    assert_eq!(person.json["summary"]["model"], "qwen3:27b");
    let agent = w
        .call(
            "GET",
            &uri,
            token(w.developer, &[Scope::ReadApi]).as_agent(true),
            None,
        )
        .await;
    assert_eq!(agent.status, StatusCode::OK, "{}", agent.json);
    assert_eq!(agent.json["summary"]["state"], "withheld");
    assert_eq!(agent.json["summary"]["text"], Value::Null);
    assert!(
        !agent.json.to_string().contains("顧客"),
        "the words never reach an agent"
    );
    assert_eq!(
        agent.json["facts"]["range"], "first_deployment",
        "the facts do"
    );
    // A person's token that declares an agent audience is an agent too.
    let declared = w
        .call_with_header(
            &uri,
            token(w.developer, &[Scope::ReadApi]),
            ("forge-context-audience", "agent"),
        )
        .await;
    assert_eq!(
        declared.json["summary"]["state"], "withheld",
        "{}",
        declared.json
    );
    // An unknown deployment is the invisible 404.
    let none = w
        .call(
            "GET",
            &w.uri("deployments/999999/approval-summary"),
            session(w.developer, false),
            None,
        )
        .await;
    assert_eq!(
        (none.status, none.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    // The summary decides nothing: the approval works as before.
    let ok = w.approve(session(w.maintainer, false), id).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.json);
    assert_eq!(ok.json["status"], "queued");
    w.t.drop_now().await;
}
