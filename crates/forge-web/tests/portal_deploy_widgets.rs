#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The PFS portal's P2b widgets through the product router (widget design
//! §3.3, §3.5; ADR-0036, ADR-0038 step C) against the development
//! database: `deploy-approvals` and `attention-count?scope=deploy` show a
//! deployment only to a person the approval route would let approve it now
//! — unrestricted environments: Maintainers; named approvers: them (and a
//! delegate approving for them); never the requester, someone who already
//! approved, or anyone in a project they cannot read. Synthetic data only.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Role};
use forge_core::portal::{PortalAuthFuture, PortalAuthenticator, PortalIdentity};
use forge_core::{ProjectId, RequestId, UserId, UserKind};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const APPROVALS: &str = "/api/portal-widgets/v1/deploy-approvals";
const ATTENTION: &str = "/api/portal-widgets/v1/attention-count";
const HOST_KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ";

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

#[derive(Clone)]
enum Stand {
    Is(PortalIdentity),
    Refused(ErrorCode, &'static str),
}

struct FakePortal(HashMap<String, Stand>);

impl PortalAuthenticator for FakePortal {
    fn authenticate_portal<'a>(
        &'a self,
        _request_id: &'a RequestId,
        presented: &'a str,
    ) -> PortalAuthFuture<'a> {
        let stand = self.0.get(presented).cloned();
        Box::pin(async move {
            match stand {
                Some(Stand::Is(identity)) => Ok(identity),
                Some(Stand::Refused(code, reason)) => {
                    Err(ForgeError::new(code).with_context("reason", reason))
                }
                None => Err(ForgeError::new(ErrorCode::PortalTokenInvalid)
                    .with_context("reason", "signature")),
            }
        })
    }
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

fn identity(user: UserId) -> PortalIdentity {
    PortalIdentity {
        user_id: user,
        active: true,
        kind: UserKind::Human,
        portal_party: true,
        access_role: true,
        is_admin: false,
    }
}

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Session, AccountKind::Human)
}

struct World {
    t: FreshDb,
    ada: UserId,
    bob: UserId,
    cy: UserId,
    dee: UserId,
    root: UserId,
    web: ProjectId,
    hidden: ProjectId,
    portal: Arc<FakePortal>,
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let mut ids = HashMap::new();
        for (name, admin) in [
            ("ada", false),
            ("bob", false),
            ("cy", false),
            ("dee", false),
            ("eve", false),
            ("root", true),
        ] {
            let u = users::create(
                &t.db,
                &ctx(&format!("req-user-{name}")),
                &NewUser {
                    is_admin: admin,
                    ..NewUser::human(name, name, &format!("{name}@example.test"))
                },
            )
            .await
            .expect("user");
            ids.insert(name, u);
        }
        let id = |n: &str| ids[n].user_id;
        // ada owns both; bob maintains, cy and dee develop `web`; nobody
        // but ada holds a role on `hidden` (private).
        let web = projects::create(
            &t.db,
            &ctx("req-web"),
            &NewProject::new("web", ids["ada"].namespace_id, "Web", id("ada")),
        )
        .await
        .expect("web")
        .project_id;
        let hidden = projects::create(
            &t.db,
            &ctx("req-hidden"),
            &NewProject::new("hidden", ids["ada"].namespace_id, "Hidden", id("ada")),
        )
        .await
        .expect("hidden")
        .project_id;
        for (u, role) in [
            (id("bob"), Role::Maintainer),
            (id("cy"), Role::Developer),
            (id("dee"), Role::Developer),
        ] {
            projects::add_member(&t.db, &ctx("req-member"), web, u, role)
                .await
                .expect("member");
        }
        let mut tokens = HashMap::new();
        for name in ["ada", "bob", "cy", "dee", "eve"] {
            tokens.insert(format!("t-{name}"), Stand::Is(identity(id(name))));
        }
        tokens.insert(
            "t-norole".to_owned(),
            Stand::Is(PortalIdentity {
                access_role: false,
                ..identity(id("bob"))
            }),
        );
        tokens.insert(
            "t-otherparty".to_owned(),
            Stand::Is(PortalIdentity {
                portal_party: false,
                ..identity(id("bob"))
            }),
        );
        tokens.insert(
            "t-disabled".to_owned(),
            Stand::Is(PortalIdentity {
                active: false,
                ..identity(id("bob"))
            }),
        );
        tokens.insert(
            "t-unlinked".to_owned(),
            Stand::Refused(ErrorCode::PortalUserNotLinked, "unlinked"),
        );
        Self {
            t,
            ada: id("ada"),
            bob: id("bob"),
            cy: id("cy"),
            dee: id("dee"),
            root: id("root"),
            web,
            hidden,
            portal: Arc::new(FakePortal(tokens)),
        }
    }

    fn router(&self) -> axum::Router {
        let db = &self.t.db;
        let state = AppState::new(
            "test",
            Vec::new(),
            Arc::new(db.clone()),
            Lookups {
                namespaces: Arc::new(db.clone()),
                projects: Arc::new(db.clone()),
            },
        )
        .with_deploy(forge_deploy::Deploy::new(
            db.clone(),
            Arc::new(OnMain),
            forge_core::deploy::TargetPolicy::default(),
        ))
        .with_review(forge_review::Review::new(db.clone(), Arc::new(NoBranches)))
        .with_portal(Arc::clone(&self.portal) as Arc<dyn PortalAuthenticator>);
        forge_web::app(state, AppConfig::default()).expect("router")
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, Value) {
        let res = self.router().oneshot(req).await.expect("infallible");
        let status = res.status();
        let raw = res.into_body().collect().await.expect("body").to_bytes();
        (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
    }

    /// A session call (the setup: environments, targets, requests,
    /// approvals, delegations).
    async fn call(&self, method: &str, uri: &str, who: UserId, body: Option<Value>) -> Value {
        let admin = who == self.root;
        let builder = Request::builder().method(method).uri(uri);
        let mut req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("request");
        req.extensions_mut().insert(session(who, admin));
        let (status, json) = self.send(req).await;
        assert!(status.is_success(), "{method} {uri}: {status} {json}");
        json
    }

    async fn portal(&self, uri: &str, token: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("request");
        self.send(req).await
    }

    fn uri(project: ProjectId, tail: &str) -> String {
        format!("/api/v1/projects/{}/{tail}", project.get())
    }

    /// `[now - 1 h, now + 2 h)` in UTC, as the database's clock says.
    async fn around_now(&self) -> (String, String) {
        let rows = self
            .t
            .json_rows(
                "SELECT to_char((now() - interval '1 hour') AT TIME ZONE 'UTC', \
                   'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS a, \
                 to_char((now() + interval '2 hours') AT TIME ZONE 'UTC', \
                   'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS b",
            )
            .await;
        let s = |k: &str| rows[0][k].as_str().expect("ts").to_owned();
        (s("a"), s("b"))
    }

    /// prod (approval) and stg (none) with a target each, and a succeeded
    /// pipeline for the SHA `a…a` of `project`.
    async fn setup(&self, project: ProjectId) {
        self.t
            .exec_superuser(&format!(
                "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, status, \
                   request_id, protected, copy_mark) VALUES ({}, 'refs/heads/main', '{}', \
                   'push', 'succeeded', 'req-pipe', true, 'open')",
                project.get(),
                "a".repeat(40)
            ))
            .await;
        for tier in ["stg", "prod"] {
            let prod = tier == "prod";
            self.call(
                "PUT",
                &Self::uri(project, &format!("deploy/environments/{tier}")),
                self.ada,
                Some(json!({"requires_approval": prod, "auto_rollback": !prod})),
            )
            .await;
            self.call(
                "PUT",
                &Self::uri(
                    project,
                    &format!("deploy/environments/{tier}/targets/web-1"),
                ),
                self.root,
                Some(json!({
                    "host": "app1.internal", "user": "deploy",
                    "host_key_type": "ssh-ed25519", "host_key": HOST_KEY,
                    "script": "web-app"
                })),
            )
            .await;
        }
    }

    /// `who` requests prod at `a…a` of `project`: the deployment's id.
    async fn request_prod(&self, project: ProjectId, who: UserId) -> i64 {
        let d = self
            .call(
                "POST",
                &Self::uri(project, "deployments"),
                who,
                Some(json!({"environment": "prod", "sha": "a".repeat(40)})),
            )
            .await;
        assert_eq!(d["status"], "pending_approval", "{d}");
        d["id"].as_i64().expect("id")
    }
}

/// The list's items (asserting the envelope).
fn items(answer: &(StatusCode, Value)) -> Vec<Value> {
    assert_eq!(answer.0, StatusCode::OK, "{}", answer.1);
    assert_eq!(answer.1["success"], true, "{}", answer.1);
    let data = &answer.1["data"];
    assert_eq!(data["type"], "list");
    assert!(data["title"].as_str().is_some_and(|t| !t.is_empty()));
    data["body"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The kpi's value.
fn count(answer: &(StatusCode, Value)) -> i64 {
    assert_eq!(answer.0, StatusCode::OK, "{}", answer.1);
    assert_eq!(answer.1["data"]["type"], "kpi", "{}", answer.1);
    answer.1["data"]["body"]["value"].as_i64().expect("value")
}

fn refused(answer: &(StatusCode, Value)) -> String {
    assert_eq!(answer.1["success"], false, "{}", answer.1);
    assert!(answer.1["error"]["request_id"].as_str().is_some());
    answer.1["error"]["code"].as_str().unwrap_or("").to_owned()
}

#[tokio::test]
async fn only_a_person_the_approval_route_would_let_approve_sees_a_deployment() {
    let w = World::new().await;
    w.setup(w.web).await;
    w.setup(w.hidden).await;
    let id = w.request_prod(w.web, w.dee).await;
    // ada requests in `hidden`, where bob holds no role.
    w.request_prod(w.hidden, w.ada).await;
    let deploy_count = format!("{ATTENTION}?scope=deploy&locale=en");

    // Unrestricted: Maintainers (bob, ada the Owner) — never the requester
    // (dee), a Developer (cy) or someone without a role (eve).
    let bob = items(&w.portal(&format!("{APPROVALS}?locale=en"), "t-bob").await);
    assert_eq!(bob.len(), 1, "{bob:?}");
    assert_eq!(bob[0]["label"], "ada/web → prod");
    assert_eq!(bob[0]["id"], format!("ada/web#{id}"));
    assert_eq!(bob[0]["href"], "/ada/web");
    assert_eq!(bob[0]["badge"]["tone"], "warn");
    assert_eq!(bob[0]["badge"]["label"], "Awaiting approval");
    let sub = bob[0]["sub"].as_str().expect("sub");
    assert!(sub.starts_with("aaaaaaa · waiting 0 min"), "{sub}");
    let kpi = w.portal(&deploy_count, "t-bob").await;
    assert_eq!(count(&kpi), 1);
    assert_eq!(kpi.1["data"]["body"]["delta"]["tone"], "warn");
    assert_eq!(kpi.1["data"]["href"], "/-/projects?tab=member");
    assert_eq!(kpi.1["data"]["body"]["display"], "1 deployments");
    // ada sees both of her projects' requests but not her own.
    assert_eq!(count(&w.portal(&deploy_count, "t-ada").await), 1);
    for nobody in ["t-cy", "t-dee", "t-eve"] {
        assert!(
            items(&w.portal(APPROVALS, nobody).await).is_empty(),
            "{nobody}"
        );
        assert_eq!(count(&w.portal(&deploy_count, nobody).await), 0, "{nobody}");
    }
    // The environment and project settings narrow; the hidden project
    // counts nothing for bob, asked or not.
    assert!(items(&w.portal(&format!("{APPROVALS}?env=stg"), "t-bob").await).is_empty());
    assert_eq!(
        items(
            &w.portal(&format!("{APPROVALS}?env=prod&env=stg"), "t-bob")
                .await
        )
        .len(),
        1
    );
    let asked_hidden = format!("{ATTENTION}?scope=deploy&project={}", w.hidden.get());
    assert_eq!(count(&w.portal(&asked_hidden, "t-bob").await), 0);
    let asked_web = format!("{ATTENTION}?scope=deploy&project={}", w.web.get());
    assert_eq!(count(&w.portal(&asked_web, "t-bob").await), 1);

    // A freeze holding now is said (badge `stop`, the end in UTC).
    let (from, to) = w.around_now().await;
    w.call(
        "PUT",
        &World::uri(w.web, "deploy/environments/prod"),
        w.ada,
        Some(json!({"requires_approval": true, "auto_rollback": false,
                    "freeze_windows": [{"starts_at": from, "ends_at": to}]})),
    )
    .await;
    let frozen = items(&w.portal(&format!("{APPROVALS}?locale=ja"), "t-bob").await);
    assert_eq!(frozen[0]["badge"]["tone"], "stop");
    assert_eq!(frozen[0]["badge"]["label"], "凍結中");
    let until = format!("凍結中（{} {} UTC まで）", &to[..10], &to[11..16]);
    assert!(
        frozen[0]["sub"].as_str().expect("sub").ends_with(&until),
        "{}",
        frozen[0]["sub"]
    );

    // Named approvers (ADR-0038 step C): cy and ada, two approvals,
    // delegation allowed. bob is no longer one of them; cy (a Developer,
    // named) is.
    w.call(
        "PUT",
        &World::uri(w.web, "deploy/environments/prod/approvers"),
        w.ada,
        Some(json!({"users": ["cy", "ada"], "approvals_required": 2, "allow_delegation": true})),
    )
    .await;
    assert!(items(&w.portal(APPROVALS, "t-bob").await).is_empty());
    assert_eq!(items(&w.portal(APPROVALS, "t-cy").await).len(), 1);
    // cy delegates to bob: bob may approve for cy (as a delegate).
    w.call(
        "PUT",
        "/api/v1/user/approval-delegation",
        w.cy,
        Some(json!({"delegate": "bob", "starts_at": from, "ends_at": to})),
    )
    .await;
    let delegated = items(&w.portal(&format!("{APPROVALS}?locale=en"), "t-bob").await);
    assert_eq!(delegated.len(), 1, "{delegated:?}");
    assert!(
        delegated[0]["sub"]
            .as_str()
            .expect("sub")
            .contains("as a delegate"),
        "{}",
        delegated[0]["sub"]
    );
    // bob approves for cy through the approval route (the delegated
    // approval's loader reads the delegation on the write pool: it failed
    // with FG-DB-0001 on the read-only pool). Then neither bob (one
    // approval per approver) nor cy (one per person) can approve it again;
    // ada, named and not yet counted, still can (one of two given).
    let approved = w
        .call(
            "POST",
            &World::uri(w.web, &format!("deployments/{id}/approve?on_behalf_of=cy")),
            w.bob,
            None,
        )
        .await;
    assert_eq!(approved["status"], "pending_approval", "{approved}");
    assert_eq!(approved["approvals"], 1, "{approved}");
    for who in ["t-cy", "t-bob"] {
        assert!(items(&w.portal(APPROVALS, who).await).is_empty(), "{who}");
        assert_eq!(count(&w.portal(&deploy_count, who).await), 0, "{who}");
    }
    assert_eq!(count(&w.portal(&deploy_count, "t-ada").await), 1);
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_p2b_route_is_decided_by_authorize_and_checked() {
    let w = World::new().await;
    for (token, status, code) in [
        ("t-norole", StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ("t-otherparty", StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ("t-disabled", StatusCode::FORBIDDEN, "FG-AUTH-0004"),
        ("t-unlinked", StatusCode::FORBIDDEN, "FG-AUTH-0027"),
        ("t-forged", StatusCode::UNAUTHORIZED, "FG-AUTH-0026"),
    ] {
        for uri in [APPROVALS.to_owned(), format!("{ATTENTION}?scope=deploy")] {
            let a = w.portal(&uri, token).await;
            assert_eq!(a.0, status, "{token} {uri}: {}", a.1);
            assert_eq!(refused(&a), code, "{token} {uri}");
        }
    }
    // A session never reads a widget.
    let mut req = Request::builder()
        .uri(APPROVALS)
        .body(Body::empty())
        .expect("request");
    req.extensions_mut().insert(session(w.bob, false));
    let s = w.send(req).await;
    assert!(s.0.is_client_error(), "{}", s.1);
    for uri in [
        format!("{APPROVALS}?env=production"),
        format!("{APPROVALS}?limit=2"),
        format!("{APPROVALS}?limit=5&limit=6"),
        format!("{APPROVALS}?locale=xx"),
        format!("{ATTENTION}?scope=deploy&project=0"),
    ] {
        let a = w.portal(&uri, "t-bob").await;
        assert_eq!(a.0, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {}", a.1);
        assert_eq!(refused(&a), "FG-WEB-0011", "{uri}");
    }
    w.t.drop_now().await;
}
