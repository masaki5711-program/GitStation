//! Plan 17 P17-10 (ADR-0039 Amendment P17-10b): `GET
//! …/merge-requests/{iid}/ai-draft` — the AI asset draft beside a draft
//! improvement merge request, read like the merge request
//! (`MergeRequestRead`: allowed for a member reading it, refused for a
//! Guest of a private project and as absent for an outsider); the branch
//! now says whether it still holds the draft. Synthetic data.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Role, Scope};
use forge_core::{
    MergeRequestChange, MergeRequestId, ObjectId, ProjectId, RefName, RequestId, UserId,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

/// The draft branch's head as the test sets it (`None`: no branch).
#[derive(Clone, Default)]
struct Heads(Arc<Mutex<Option<String>>>);

impl forge_review::BranchProbe for Heads {
    fn branch_head<'a>(
        &'a self,
        _: forge_core::RepoId,
        branch: &'a RefName,
        _: &'a str,
    ) -> forge_core::namespace_resolver::LookupFuture<'a, ObjectId> {
        let head = self.0.lock().expect("lock").clone();
        Box::pin(async move {
            assert_eq!(branch.as_str(), "refs/heads/ai-draft/ir-1");
            match head {
                Some(h) => Ok(ObjectId::parse(&h).expect("oid")),
                None => Err(ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                    .with_context("reason", "ref_not_found")),
            }
        })
    }
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

fn read_token(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
        AccountKind::Human,
    )
}

struct World {
    t: FreshDb,
    developer: UserId,
    outsider: UserId,
    guest: UserId,
    web: ProjectId,
    heads: Heads,
}

const COMMIT: char = 'b';
const HEAD: char = 'a';

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
            &NewProject::new("pfs-ai", owner.namespace_id, "PFS AI", owner.user_id),
        )
        .await
        .expect("web");
        let _ready =
            forge_db::repositories::mark_ready(&t.db, &ctx("req-ready"), web.repository_id)
                .await
                .expect("ready");
        for (who, role) in [
            (developer.user_id, Role::Developer),
            (guest.user_id, Role::Guest),
        ] {
            projects::add_member(&t.db, &ctx("req-member"), web.project_id, who, role)
                .await
                .expect("member");
        }
        let p = web.project_id.get();
        t.commit_audited(
            ("project", p),
            &[
                format!(
                    "INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 2)"
                ),
                format!(
                    "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                     source_branch, target_branch, head_sha, draft) VALUES \
                     ({p}, 1, 'Improve SPC', {d}, 'refs/heads/improve/ir-1', 'refs/heads/main', '{h}', true), \
                     ({p}, 2, 'Feature', {d}, 'refs/heads/feature', 'refs/heads/main', '{h}', false)",
                    d = developer.user_id.get(),
                    h = HEAD.to_string().repeat(40)
                ),
            ],
        )
        .await;
        Self {
            t,
            developer: developer.user_id,
            outsider: outsider.user_id,
            guest: guest.user_id,
            web: web.project_id,
            heads: Heads::default(),
        }
    }

    async fn mr_id(&self, iid: i64) -> MergeRequestId {
        let rows = self
            .t
            .json_rows(&format!(
                "SELECT id FROM forge.merge_requests WHERE iid = {iid}"
            ))
            .await;
        MergeRequestId::new(rows[0]["id"].as_i64().expect("id"))
    }

    async fn propose(&self, dropped: u32) {
        let change = MergeRequestChange::AiDraftProposed {
            merge_request: self.mr_id(1).await,
            iid: 1,
            branch: RefName::parse("refs/heads/ai-draft/ir-1").expect("ref"),
            commit: ObjectId::parse(&COMMIT.to_string().repeat(40)).expect("oid"),
            parent: ObjectId::parse(&HEAD.to_string().repeat(40)).expect("oid"),
            files: vec!["skills/pe-quality-spc.md".to_owned()],
            dropped_edits: dropped,
            advice_id: 7,
            llm_request_id: 8,
        };
        let recorded = forge_db::llm::asset_draft::record_proposed(
            &self.t.db,
            &ctx("req-draft"),
            self.web,
            change,
        )
        .await
        .expect("record");
        assert!(recorded);
    }

    async fn get(&self, iid: i64, who: Principal) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method("GET")
            .uri(format!(
                "/api/v1/projects/{}/merge-requests/{iid}/ai-draft",
                self.web.get()
            ))
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
        .with_review(forge_review::Review::new(
            self.t.db.clone(),
            Arc::new(self.heads.clone()),
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
        (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
    }

    fn set_head(&self, head: Option<char>) {
        *self.heads.0.lock().expect("lock") = head.map(|c| c.to_string().repeat(40));
    }
}

#[tokio::test]
async fn a_proposed_draft_is_shown_with_its_branch_commit_files_and_dropped_count() {
    let w = World::new().await;
    // Before the worker proposed anything: an improvement without a draft.
    let (status, none) = w.get(1, session(w.developer)).await;
    assert_eq!(status, StatusCode::OK, "{none}");
    assert_eq!(none["improvement"], true);
    assert_eq!(none["state"], "none");
    assert_eq!(none["draft"], Value::Null);
    assert!(none["label"]
        .as_str()
        .expect("label")
        .starts_with("AI の下書き（提案）"));
    w.propose(2).await;
    w.set_head(Some(COMMIT));
    let (status, shown) = w.get(1, session(w.developer)).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["state"], "proposed");
    let d = &shown["draft"];
    assert_eq!(d["branch"], "ai-draft/ir-1");
    assert_eq!(d["commit_sha"], COMMIT.to_string().repeat(40));
    assert_eq!(d["parent_sha"], HEAD.to_string().repeat(40));
    assert_eq!(d["files"], serde_json::json!(["skills/pe-quality-spc.md"]));
    assert_eq!(d["dropped_edits"], 2);
    assert_eq!(d["advice_id"], 7);
    assert_eq!(d["branch_state"], "current");
    assert!(d["proposed_at"].as_str().expect("time").ends_with('Z'));
    // A read-only token reads it too (as the merge request).
    let (status, _) = w.get(1, read_token(w.developer)).await;
    assert_eq!(status, StatusCode::OK);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_moved_or_deleted_draft_branch_is_said_so() {
    let w = World::new().await;
    w.propose(0).await;
    w.set_head(Some('c'));
    let (_, moved) = w.get(1, session(w.developer)).await;
    assert_eq!(moved["draft"]["branch_state"], "moved", "{moved}");
    assert_eq!(moved["draft"]["branch_head"], "c".repeat(40));
    w.set_head(None);
    let (status, deleted) = w.get(1, session(w.developer)).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["draft"]["branch_state"], "deleted");
    assert_eq!(deleted["draft"]["branch_head"], Value::Null);
    w.t.drop_now().await;
}

/// A draft branch without the worker's record (its record failed) is
/// said so, never "no draft".
#[tokio::test]
async fn a_draft_branch_without_its_record_is_unrecorded() {
    let w = World::new().await;
    w.set_head(Some(COMMIT));
    let (status, body) = w.get(1, session(w.developer)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "unrecorded");
    assert_eq!(body["branch"], "ai-draft/ir-1");
    assert_eq!(body["draft"], Value::Null);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_merge_request_from_another_branch_has_no_draft() {
    let w = World::new().await;
    w.propose(0).await;
    let (status, other) = w.get(2, session(w.developer)).await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert_eq!(other["improvement"], false);
    assert_eq!(other["state"], "none");
    w.t.drop_now().await;
}

/// The permission matrix rows of the route (`MergeRequestRead`): a Guest
/// of the private project is refused (403 with the request id), an
/// outsider and an anonymous reader see nothing (404 / 401).
#[tokio::test]
async fn a_guest_an_outsider_and_anonymous_are_refused() {
    let w = World::new().await;
    w.propose(0).await;
    let (status, guest) = w.get(1, session(w.guest)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{guest}");
    assert!(guest["request_id"].is_string(), "{guest}");
    assert!(guest.get("draft").is_none(), "{guest}");
    let (status, outsider) = w.get(1, session(w.outsider)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{outsider}");
    let (status, _) = w.get(1, Principal::anonymous()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    w.t.drop_now().await;
}
