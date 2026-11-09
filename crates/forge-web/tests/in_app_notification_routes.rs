#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The in-app notification routes of M3 U4 through the product router
//! against the development database (ADR-0011), with the real loaders and
//! commands: `GET /api/v1/user/notifications` (+ `/unread-count`) and
//! `POST …/{id}/read`, `…/read-all`. Only the caller's own items are
//! reachable, an item whose merge request the caller cannot read is not
//! listed or counted, another person's item and an absent one answer alike,
//! scopes decide read versus mark, and the query is strict. The permission
//! matrix's rows over HTTP. Principals are inserted as request extensions
//! (the authentication layer is tested elsewhere). Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal, Scope};
use forge_core::{ProjectId, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_notify::Settings;
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const LIST: &str = "/api/v1/user/notifications";
const COUNT: &str = "/api/v1/user/notifications/unread-count";
const READ_ALL: &str = "/api/v1/user/notifications/read-all";

struct NoTokens;

impl forge_core::Authenticator for NoTokens {
    fn authenticate<'a>(
        &'a self,
        _: &'a RequestId,
        _: &'a str,
    ) -> forge_core::authenticator::AuthFuture<'a> {
        Box::pin(async {
            Err(forge_errors::ForgeError::new(
                forge_errors::ErrorCode::Unauthenticated,
            ))
        })
    }
}

fn state(db: &Db) -> AppState {
    AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
}

fn router(db: &Db) -> axum::Router {
    let state = state(db).with_notifications(Settings::new(db.clone()));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

struct World {
    t: FreshDb,
    alice: UserId,
    carol: UserId,
    root: UserId,
    project: ProjectId,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let mut ids = Vec::new();
    for (name, admin) in [("alice", false), ("carol", false), ("root", true)] {
        let mut new = NewUser::human(
            name,
            &format!("Synthetic {name}"),
            &format!("{name}@example.test"),
        );
        new.is_admin = admin;
        ids.push(
            users::create(&t.db, &system(&format!("req-user-{name}")), &new)
                .await
                .expect("user"),
        );
    }
    let project = projects::create(
        &t.db,
        &system("req-web"),
        &NewProject::new("web", ids[0].namespace_id, "Web", ids[0].user_id),
    )
    .await
    .expect("project")
    .project_id;
    World {
        t,
        alice: ids[0].user_id,
        carol: ids[1].user_id,
        root: ids[2].user_id,
        project,
    }
}

impl World {
    /// Merge request `iid` of `author` and one unread item per entry of
    /// `recipients` about it (as the notify subscriber records them).
    async fn mr_with_items(&self, iid: i64, author: UserId, recipients: &[UserId]) -> Vec<i64> {
        let p = self.project.get();
        let counter = if iid == 1 {
            format!(
                "INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 1)"
            )
        } else {
            format!(
                "UPDATE forge.merge_request_counters SET last_iid = {iid} WHERE project_id = {p}"
            )
        };
        self.t
            .commit_audited(
                ("project", p),
                &[
                    counter,
                    format!(
                        "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                         source_branch, target_branch) VALUES ({p}, {iid}, 'Change {iid}', {}, \
                         'refs/heads/b{iid}', 'refs/heads/main')",
                        author.get()
                    ),
                ],
            )
            .await;
        let mut ids = Vec::new();
        for r in recipients {
            self.t
                .exec_superuser(&format!(
                    "INSERT INTO forge.in_app_notifications (user_id, kind, event_id, event_seq, \
                     project_id, merge_request_iid, actor_user_id) VALUES ({}, 'mr_approved', \
                     gen_random_uuid(), 1, {p}, {iid}, NULL)",
                    r.get()
                ))
                .await;
            let rows = self
                .t
                .json_rows("SELECT max(id) AS id FROM forge.in_app_notifications")
                .await;
            ids.push(rows[0]["id"].as_i64().expect("id"));
        }
        ids
    }
}

struct Answer {
    status: StatusCode,
    json: Value,
    cache_control: String,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }
}

async fn send(
    db: &Db,
    method: &str,
    uri: &str,
    who: Option<Principal>,
    body: Option<Value>,
) -> Answer {
    let builder = Request::builder().method(method).uri(uri);
    let mut req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    if let Some(who) = who {
        req.extensions_mut().insert(who);
    }
    let res = router(db).oneshot(req).await.expect("infallible");
    let status = res.status();
    let cache_control = res
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let raw = res.into_body().collect().await.expect("body").to_bytes();
    Answer {
        status,
        json: serde_json::from_slice(&raw).unwrap_or(Value::Null),
        cache_control,
    }
}

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(
        user,
        admin,
        true,
        Credential::Session,
        forge_auth::AccountKind::Human,
    )
}

fn token(user: UserId, scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        forge_auth::AccountKind::Human,
    )
}

fn read_one(id: i64) -> String {
    format!("{LIST}/{id}/read")
}

#[tokio::test]
async fn a_person_lists_counts_and_marks_their_own_items() {
    let w = world().await;
    let ids = w.mr_with_items(1, w.alice, &[w.alice, w.alice]).await;
    let me = || Some(session(w.alice, false));

    let list = send(&w.t.db, "GET", LIST, me(), None).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.json);
    assert_eq!(list.cache_control, "private, no-store");
    let items = list.json["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], ids[1], "newest first");
    assert_eq!(items[0]["kind"], "mr_approved");
    assert_eq!(items[0]["project_path"], "alice/web");
    assert_eq!(items[0]["merge_request_iid"], 1);
    assert_eq!(items[0]["merge_request_title"], "Change 1");
    assert_eq!(items[0]["actor_name"], Value::Null);
    assert_eq!(items[0]["read_at"], Value::Null);
    assert_eq!(list.json["next_before"], Value::Null);

    let count = send(&w.t.db, "GET", COUNT, me(), None).await;
    assert_eq!(count.json, json!({"count": 2, "capped": false}));

    // A page of one has a cursor; the next page is the older item.
    let first = send(&w.t.db, "GET", &format!("{LIST}?limit=1"), me(), None).await;
    assert_eq!(first.json["next_before"], ids[1]);
    let second = send(
        &w.t.db,
        "GET",
        &format!("{LIST}?limit=1&before={}", ids[1]),
        me(),
        None,
    )
    .await;
    assert_eq!(second.json["items"][0]["id"], ids[0]);

    let marked = send(&w.t.db, "POST", &read_one(ids[0]), me(), None).await;
    assert_eq!(marked.status, StatusCode::OK, "{}", marked.json);
    assert_eq!(marked.json["id"], ids[0]);
    assert!(marked.json["read_at"].is_string());
    // The answer carries no words of the item.
    assert!(marked.json.get("merge_request_title").is_none());
    let unread = send(&w.t.db, "GET", &format!("{LIST}?unread=true"), me(), None).await;
    let unread = unread.json["items"].as_array().expect("items").clone();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0]["id"], ids[1]);

    let all = send(&w.t.db, "POST", READ_ALL, me(), Some(json!({}))).await;
    // No count: it would include items the caller may no longer see.
    assert_eq!(all.status, StatusCode::NO_CONTENT, "{}", all.json);
    assert_eq!(all.json, Value::Null);
    let count = send(&w.t.db, "GET", COUNT, me(), None).await;
    assert_eq!(count.json["count"], 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_item_the_person_can_no_longer_see_is_neither_listed_nor_counted() {
    let w = world().await;
    // Carol is not a member of Alice's private project.
    w.mr_with_items(1, w.alice, &[w.carol]).await;
    let carol = || Some(session(w.carol, false));
    let list = send(&w.t.db, "GET", LIST, carol(), None).await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.json["items"], json!([]));
    let count = send(&w.t.db, "GET", COUNT, carol(), None).await;
    assert_eq!(count.json["count"], 0);
    // A member again: shown.
    w.t.commit_audited(
        ("project", w.project.get()),
        &[format!(
            "INSERT INTO forge.project_members (project_id, user_id, role) VALUES ({}, {}, 'reporter')",
            w.project.get(),
            w.carol.get()
        )],
    )
    .await;
    let count = send(&w.t.db, "GET", COUNT, carol(), None).await;
    assert_eq!(count.json["count"], 1);
    w.t.drop_now().await;
}

#[tokio::test]
async fn another_persons_item_and_an_absent_one_answer_alike() {
    let w = world().await;
    let ids = w.mr_with_items(1, w.alice, &[w.alice]).await;
    let foreign = send(
        &w.t.db,
        "POST",
        &read_one(ids[0]),
        Some(session(w.carol, false)),
        None,
    )
    .await;
    let absent = send(
        &w.t.db,
        "POST",
        &read_one(ids[0] + 1000),
        Some(session(w.carol, false)),
        None,
    )
    .await;
    let admin = send(
        &w.t.db,
        "POST",
        &read_one(ids[0]),
        Some(session(w.root, true)),
        None,
    )
    .await;
    for a in [&foreign, &absent, &admin] {
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
            "{}",
            a.json
        );
    }
    assert_eq!(foreign.json["user_message"], absent.json["user_message"]);
    // Still unread for Alice; the administrator's own list is empty.
    let count = send(&w.t.db, "GET", COUNT, Some(session(w.alice, false)), None).await;
    assert_eq!(count.json["count"], 1);
    let roots = send(&w.t.db, "GET", LIST, Some(session(w.root, true)), None).await;
    assert_eq!(roots.json["items"], json!([]));
    w.t.drop_now().await;
}

#[tokio::test]
async fn scopes_decide_reading_versus_marking_and_anonymous_is_refused() {
    let w = world().await;
    let ids = w.mr_with_items(1, w.alice, &[w.alice]).await;
    let read_api = || Some(token(w.alice, &[Scope::ReadApi]));
    assert_eq!(
        send(&w.t.db, "GET", LIST, read_api(), None).await.status,
        StatusCode::OK
    );
    assert_eq!(
        send(&w.t.db, "GET", COUNT, read_api(), None).await.status,
        StatusCode::OK
    );
    let refused = send(&w.t.db, "POST", &read_one(ids[0]), read_api(), None).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.json);
    let refused = send(&w.t.db, "POST", READ_ALL, read_api(), Some(json!({}))).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let repo = send(
        &w.t.db,
        "GET",
        LIST,
        Some(token(w.alice, &[Scope::ReadRepository])),
        None,
    )
    .await;
    assert_eq!(repo.status, StatusCode::FORBIDDEN);
    let api = send(
        &w.t.db,
        "POST",
        &read_one(ids[0]),
        Some(token(w.alice, &[Scope::Api])),
        None,
    )
    .await;
    assert_eq!(api.status, StatusCode::OK, "{}", api.json);
    for (method, uri) in [
        ("GET", LIST.to_owned()),
        ("GET", COUNT.to_owned()),
        ("POST", read_one(ids[0])),
    ] {
        let anon = send(&w.t.db, method, &uri, None, None).await;
        assert_eq!(anon.status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_bad_query_or_body_is_a_precise_error() {
    let w = world().await;
    let me = || Some(session(w.alice, false));
    for query in [
        "limit=0",
        "limit=101",
        "before=x",
        "unread=yes",
        "user_id=2",
    ] {
        let bad = send(&w.t.db, "GET", &format!("{LIST}?{query}"), me(), None).await;
        assert_eq!(
            (bad.status, bad.code()),
            (StatusCode::BAD_REQUEST, "FG-WEB-0006"),
            "{query}: {}",
            bad.json
        );
    }
    for body in [
        json!({"up_to": 0}),
        json!({"up_to": "x"}),
        json!({"user_id": 2}),
    ] {
        let bad = send(&w.t.db, "POST", READ_ALL, me(), Some(body.clone())).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{body}: {}", bad.json);
    }
    let bad = send(&w.t.db, "POST", &format!("{LIST}/x/read"), me(), None).await;
    assert!(bad.status.is_client_error(), "{}", bad.json);
    w.t.drop_now().await;
}
