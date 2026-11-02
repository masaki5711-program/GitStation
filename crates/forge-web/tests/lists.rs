#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `GET /api/v1/projects` and `GET /api/v1/groups` (M1 phase 8b MR7b)
//! against the real page loaders (`forge_db::lists`, one statement per
//! page) on the development database (ADR-0011), through the product
//! router:
//!
//! - a page never holds a row `authorize()` denies (private of others,
//!   Guest on private, pending deletion), and holds internal ones;
//! - budget-cut pages are short with `has_more` and continue after the
//!   last scanned row, with no row repeated or skipped;
//! - a cursor of another caller or another list, or a tampered one, is
//!   `FG-WEB-0006`; bad `limit` values are `FG-WEB-0006`;
//! - rows inserted while paging appear on later pages, none repeated;
//! - anonymous `401`, a repository-only token `403`, before any load.
//!
//! Synthetic data only.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal, Scope};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    CursorKeys, GroupId, GroupListing, ProjectId, ProjectRecord, RequestId, Role, UserId,
    Visibility,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, CreatedGroup, NewGroup};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_errors::ErrorCode;
use forge_web::{AppConfig, AppState, Lists, Lookups};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

/// The database as the page loaders, counting the calls.
struct Counting {
    db: Db,
    pages: AtomicUsize,
}

impl forge_core::ProjectPageLoader for Counting {
    fn project_page<'a>(
        &'a self,
        after: Option<ProjectId>,
        viewer: UserId,
        max_rows: u32,
    ) -> LookupFuture<'a, Vec<ProjectRecord>> {
        self.pages.fetch_add(1, Ordering::SeqCst);
        forge_core::ProjectPageLoader::project_page(&self.db, after, viewer, max_rows)
    }
}

impl forge_core::GroupPageLoader for Counting {
    fn group_page<'a>(
        &'a self,
        after: Option<GroupId>,
        viewer: UserId,
        max_rows: u32,
    ) -> LookupFuture<'a, Vec<GroupListing>> {
        self.pages.fetch_add(1, Ordering::SeqCst);
        forge_core::GroupPageLoader::group_page(&self.db, after, viewer, max_rows)
    }
}

/// No token store (principals are inserted directly).
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

/// A synthetic key (never a real one).
fn keys() -> Arc<CursorKeys> {
    Arc::new(CursorKeys::parse(&format!("7 {}\n", "5a".repeat(32))).expect("keys"))
}

fn app(counting: &Arc<Counting>, budget: u32) -> axum::Router {
    let db = counting.db.clone(); // clone: the pool handle is shared
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db),
        },
    )
    .with_lists(
        Lists::new(
            Arc::clone(counting) as Arc<dyn forge_core::ProjectPageLoader>,
            Arc::clone(counting) as Arc<dyn forge_core::GroupPageLoader>,
            keys(),
        )
        .with_scan_budget(budget),
    );
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn user(t: &FreshDb, name: &str) -> users::Created {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
}

async fn project(
    t: &FreshDb,
    owner: &users::Created,
    segment: &str,
    v: Visibility,
) -> CreatedProject {
    projects::create(
        &t.db,
        &ctx(&format!("req-project-{segment}")),
        &NewProject {
            visibility: v,
            ..NewProject::new(segment, owner.namespace_id, segment, owner.user_id)
        },
    )
    .await
    .expect("project")
}

async fn group(t: &FreshDb, segment: &str, owner: UserId) -> CreatedGroup {
    groups::create(
        &t.db,
        &ctx(&format!("req-group-{segment}")),
        &NewGroup {
            segment: segment.to_owned(),
            parent: None,
            display_name: segment.to_owned(),
            description: String::new(),
            owner,
        },
    )
    .await
    .expect("group")
}

fn session(who: UserId) -> Principal {
    Principal::authenticated(who, false, true, Credential::Session)
}

async fn get(router: axum::Router, uri: &str, who: Option<Principal>) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    if let Some(principal) = who {
        req.extensions_mut().insert(principal);
    }
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The ids of a page's items.
fn ids(page: &Value) -> Vec<i64> {
    page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["id"].as_i64().expect("id"))
        .collect()
}

/// Every page of `base` for `who`, following the cursors.
async fn walk(counting: &Arc<Counting>, budget: u32, base: &str, who: UserId) -> Vec<Value> {
    let mut pages = Vec::new();
    let mut uri = base.to_owned();
    loop {
        let (status, page) = get(app(counting, budget), &uri, Some(session(who))).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let more = page["has_more"].as_bool().expect("has_more");
        let next = page["next_cursor"].as_str().map(str::to_owned);
        assert_eq!(
            more,
            next.is_some(),
            "a cursor exactly when more may follow"
        );
        pages.push(page);
        match next {
            Some(cursor) => uri = format!("{base}&cursor={cursor}"),
            None => return pages,
        }
        assert!(pages.len() < 50, "paging terminates");
    }
}

#[tokio::test]
async fn a_page_holds_only_rows_authorize_allows_and_budget_cuts_are_short() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let mine = project(&t, &bob, "mine", Visibility::Private).await;
    let a1 = project(&t, &alice, "a1", Visibility::Private).await;
    let a2 = project(&t, &alice, "a2", Visibility::Private).await;
    let guest = project(&t, &alice, "guest", Visibility::Private).await;
    let open = project(&t, &alice, "open", Visibility::Internal).await;
    let gone = project(&t, &bob, "gone", Visibility::Private).await;
    let reporter = project(&t, &alice, "reporter", Visibility::Private).await;
    for (p, role) in [(&guest, Role::Guest), (&reporter, Role::Reporter)] {
        projects::add_member(&t.db, &ctx("req-member"), p.project_id, bob.user_id, role)
            .await
            .expect("member");
    }
    t.commit_audited(
        ("project", gone.project_id.get()),
        &[format!(
            "UPDATE forge.projects SET deleted_at = now(), \
             purge_after = now() + interval '7 days' WHERE id = {}",
            gone.project_id.get()
        )],
    )
    .await;
    let counting = Arc::new(Counting {
        db: t.db.clone(), // clone: the pool handle is shared
        pages: AtomicUsize::new(0),
    });
    let visible = vec![
        mine.project_id.get(),
        open.project_id.get(),
        reporter.project_id.get(),
    ];

    // One page, the full budget: only what ProjectRead allows.
    let (status, page) = get(
        app(&counting, 1000),
        "/api/v1/projects",
        Some(session(bob.user_id)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(ids(&page), visible);
    assert_eq!(page["has_more"], false);
    assert_eq!(page["next_cursor"], Value::Null);
    assert_eq!(page["items"][0]["role"], "owner");
    assert_eq!(page["items"][1]["role"], Value::Null, "internal, no role");
    assert_eq!(page["items"][2]["path"], "alice/reporter");
    for hidden in [
        a1.project_id,
        a2.project_id,
        guest.project_id,
        gone.project_id,
    ] {
        assert!(!ids(&page).contains(&hidden.get()), "{hidden:?} is denied");
    }

    // Budget 2: short (even empty) pages, the same rows in total, in order.
    let pages = walk(&counting, 2, "/api/v1/projects?limit=5", bob.user_id).await;
    let all: Vec<i64> = pages.iter().flat_map(ids).collect();
    assert_eq!(all, visible, "no row repeated, skipped or denied");
    assert!(
        pages.iter().any(|p| ids(p).is_empty()),
        "a budget cut gives a short page"
    );
    assert!(pages.iter().all(|p| ids(p).len() <= 2));

    // limit=1 with the full budget: one row a page.
    let pages = walk(&counting, 1000, "/api/v1/projects?limit=1", bob.user_id).await;
    assert_eq!(pages.iter().flat_map(ids).collect::<Vec<_>>(), visible);

    // alice sees her own, the internal one, not bob's.
    let (_, page) = get(
        app(&counting, 1000),
        "/api/v1/projects?limit=100",
        Some(session(alice.user_id)),
    )
    .await;
    assert_eq!(
        ids(&page),
        [a1, a2, guest, open, reporter]
            .iter()
            .map(|p| p.project_id.get())
            .collect::<Vec<_>>()
    );
    t.drop_now().await;
}

#[tokio::test]
async fn groups_are_listed_with_the_callers_role() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", alice.user_id).await;
    let beta = group(&t, "beta", bob.user_id).await;
    let counting = Arc::new(Counting {
        db: t.db.clone(), // clone: the pool handle is shared
        pages: AtomicUsize::new(0),
    });
    let pages = walk(&counting, 1000, "/api/v1/groups?limit=1", bob.user_id).await;
    let items: Vec<Value> = pages
        .iter()
        .flat_map(|p| p["items"].as_array().expect("items").clone())
        .collect();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], acme.group_id.get());
    assert_eq!(items[0]["path"], "acme");
    assert_eq!(items[0]["name"], "acme");
    assert_eq!(items[0]["role"], Value::Null);
    assert_eq!(items[1]["id"], beta.group_id.get());
    assert_eq!(items[1]["role"], "owner");
    assert_eq!(items[1]["namespace_id"], beta.namespace_id.get());
    t.drop_now().await;
}

#[tokio::test]
async fn a_cursor_of_another_caller_or_list_or_a_tampered_one_is_request_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    for name in ["p1", "p2", "p3"] {
        project(&t, &alice, name, Visibility::Internal).await;
    }
    group(&t, "g1", alice.user_id).await;
    group(&t, "g2", alice.user_id).await;
    let counting = Arc::new(Counting {
        db: t.db.clone(), // clone: the pool handle is shared
        pages: AtomicUsize::new(0),
    });
    let (_, page) = get(
        app(&counting, 1000),
        "/api/v1/projects?limit=1",
        Some(session(alice.user_id)),
    )
    .await;
    let cursor = page["next_cursor"].as_str().expect("a cursor").to_owned();
    let mut tampered = cursor.clone().into_bytes();
    let last = tampered.len() - 2;
    tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(tampered).expect("ascii");
    let before = counting.pages.load(Ordering::SeqCst);
    let cases = [
        (
            format!("/api/v1/projects?cursor={cursor}"),
            bob.user_id,
            "another caller",
        ),
        (
            format!("/api/v1/groups?cursor={cursor}"),
            alice.user_id,
            "another list",
        ),
        (
            format!("/api/v1/projects?cursor={tampered}"),
            alice.user_id,
            "tampered",
        ),
        (
            "/api/v1/projects?cursor=%%%".to_owned(),
            alice.user_id,
            "not base64url",
        ),
    ];
    for (uri, who, label) in cases {
        let (status, body) = get(app(&counting, 1000), &uri, Some(session(who))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {body}");
        assert_eq!(
            body["error_code"],
            ErrorCode::RequestInvalid.id(),
            "{label}"
        );
        assert!(body.get("items").is_none());
    }
    assert_eq!(
        counting.pages.load(Ordering::SeqCst),
        before,
        "a bad cursor loads nothing"
    );
    // The owner's own cursor continues.
    let (status, page) = get(
        app(&counting, 1000),
        &format!("/api/v1/projects?limit=1&cursor={cursor}"),
        Some(session(alice.user_id)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(ids(&page), [2]);
    t.drop_now().await;
}

#[tokio::test]
async fn paging_is_stable_under_concurrent_inserts() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let mut created = Vec::new();
    for name in ["p1", "p2", "p3", "p4"] {
        created.push(
            project(&t, &alice, name, Visibility::Private)
                .await
                .project_id
                .get(),
        );
    }
    let counting = Arc::new(Counting {
        db: t.db.clone(), // clone: the pool handle is shared
        pages: AtomicUsize::new(0),
    });
    let base = "/api/v1/projects?limit=2";
    let (_, first) = get(app(&counting, 1000), base, Some(session(alice.user_id))).await;
    assert_eq!(ids(&first), created[..2]);
    // Rows created while the client pages.
    for name in ["n1", "n2", "n3"] {
        created.push(
            project(&t, &alice, name, Visibility::Private)
                .await
                .project_id
                .get(),
        );
    }
    let mut seen = ids(&first);
    let mut cursor = first["next_cursor"].as_str().expect("cursor").to_owned();
    loop {
        let (status, page) = get(
            app(&counting, 1000),
            &format!("{base}&cursor={cursor}"),
            Some(session(alice.user_id)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        seen.extend(ids(&page));
        match page["next_cursor"].as_str() {
            Some(next) => cursor = next.to_owned(),
            None => break,
        }
    }
    assert_eq!(
        seen, created,
        "new rows appear after the cursor, none repeated"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn bad_limits_anonymous_and_repository_tokens_are_refused_before_any_load() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let counting = Arc::new(Counting {
        db: t.db.clone(), // clone: the pool handle is shared
        pages: AtomicUsize::new(0),
    });
    for uri in [
        "/api/v1/projects?limit=0",
        "/api/v1/projects?limit=101",
        "/api/v1/projects?limit=x",
        "/api/v1/groups?limit=1&limit=2",
        "/api/v1/groups?page=2",
    ] {
        let (status, body) = get(app(&counting, 1000), uri, Some(session(alice.user_id))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert_eq!(body["error_code"], ErrorCode::RequestInvalid.id(), "{uri}");
    }
    let repo_only = Principal::authenticated(
        alice.user_id,
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
    );
    let disabled = Principal::authenticated(alice.user_id, false, false, Credential::Session);
    for path in ["/api/v1/projects", "/api/v1/groups"] {
        let (status, body) = get(app(&counting, 1000), path, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(body["error_code"], ErrorCode::Unauthenticated.id());
        let (status, body) = get(app(&counting, 1000), path, Some(repo_only.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(body["error_code"], ErrorCode::InsufficientScope.id());
        let (status, body) = get(app(&counting, 1000), path, Some(disabled.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(body["error_code"], ErrorCode::AccountDisabled.id());
    }
    assert_eq!(
        counting.pages.load(Ordering::SeqCst),
        0,
        "nothing was loaded"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_server_without_lists_fails_loudly() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let db = t.db.clone(); // clone: the pool handle is shared
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db),
        },
    );
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    let (status, body) = get(router, "/api/v1/projects", Some(session(alice.user_id))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error_code"], ErrorCode::Internal.id());
    t.drop_now().await;
}

/// A project loader that waits for a gate (and can fail), so requests can be
/// held in flight.
struct Gated {
    gate: Arc<tokio::sync::Semaphore>,
    fail: bool,
}

impl forge_core::ProjectPageLoader for Gated {
    fn project_page<'a>(
        &'a self,
        _: Option<ProjectId>,
        _: UserId,
        _: u32,
    ) -> LookupFuture<'a, Vec<ProjectRecord>> {
        Box::pin(async move {
            let permit = self.gate.acquire().await.expect("gate open");
            permit.forget();
            if self.fail {
                Err(forge_errors::ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "synthetic_loader_failure"))
            } else {
                Ok(Vec::new())
            }
        })
    }
}

impl forge_core::GroupPageLoader for Gated {
    fn group_page<'a>(
        &'a self,
        _: Option<GroupId>,
        _: UserId,
        _: u32,
    ) -> LookupFuture<'a, Vec<GroupListing>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

fn gated_app(t: &FreshDb, gated: &Arc<Gated>, total: u32, per_user: u32) -> axum::Router {
    let db = t.db.clone(); // clone: the pool handle is shared
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db),
        },
    )
    .with_lists(
        Lists::new(
            Arc::clone(gated) as Arc<dyn forge_core::ProjectPageLoader>,
            Arc::clone(gated) as Arc<dyn forge_core::GroupPageLoader>,
            keys(),
        )
        .with_concurrency(total, per_user),
    );
    forge_web::app(state, AppConfig::default()).expect("router")
}

async fn busy(router: axum::Router, who: UserId) -> (StatusCode, Option<String>) {
    let mut req = Request::builder()
        .method("GET")
        .uri("/api/v1/projects")
        .body(Body::empty())
        .expect("request");
    req.extensions_mut().insert(session(who));
    let res = router.oneshot(req).await.expect("infallible");
    let retry = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (res.status(), retry)
}

#[tokio::test]
async fn list_requests_over_the_per_user_or_global_cap_get_503_and_slots_are_returned() {
    let t = FreshDb::migrated().await;
    let (alice, bob, carol) = (
        user(&t, "alice").await,
        user(&t, "bob").await,
        user(&t, "carol").await,
    );
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let gated = Arc::new(Gated {
        gate: Arc::clone(&gate),
        fail: false,
    });
    // Total 3, two per user.
    let router = gated_app(&t, &gated, 3, 2);
    let hold = |who: UserId| tokio::spawn(busy(router.clone(), who));
    let (a1, a2) = (hold(alice.user_id), hold(alice.user_id));
    // Let both reach the loader (they park on the gate).
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    // A third from alice: per-user cap, refused at once with Retry-After.
    let (status, retry) = busy(router.clone(), alice.user_id).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(retry.as_deref(), Some("1"));
    // Bob takes the last global slot; carol is then refused globally.
    let b1 = hold(bob.user_id);
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    let (status, retry) = busy(router.clone(), carol.user_id).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(retry.as_deref(), Some("1"));
    // Completion returns every slot.
    gate.add_permits(3);
    for h in [a1, a2, b1] {
        assert_eq!(h.await.expect("task").0, StatusCode::OK);
    }
    gate.add_permits(2);
    assert_eq!(busy(router.clone(), alice.user_id).await.0, StatusCode::OK);
    assert_eq!(busy(router, carol.user_id).await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_failing_list_request_returns_its_slots() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let gate = Arc::new(tokio::sync::Semaphore::new(100));
    let gated = Arc::new(Gated { gate, fail: true });
    let router = gated_app(&t, &gated, 1, 1);
    // One slot only: if the failure leaked it, the second call would be 503.
    for _ in 0..3 {
        let (status, _) = busy(router.clone(), alice.user_id).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }
}

/// M2 MR6: the filters `membership` and `state=pending_deletion`
/// narrow what `authorize()` allowed (pending deletion is decided with
/// `ProjectRestore`: only what the caller may restore), and a cursor is
/// bound to the filters it was handed out under.
#[tokio::test]
async fn the_filters_narrow_what_authorize_allowed_and_bind_the_cursor() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let first = project(&t, &bob, "first", Visibility::Private).await;
    let sys = project(&t, &bob, "sys", Visibility::Private).await;
    let open = project(&t, &alice, "open", Visibility::Internal).await;
    let rep = project(&t, &alice, "rep", Visibility::Private).await;
    let _hidden = project(&t, &alice, "hidden", Visibility::Private).await;
    let gone = project(&t, &bob, "gone", Visibility::Private).await;
    projects::add_member(
        &t.db,
        &ctx("req-member"),
        rep.project_id,
        bob.user_id,
        Role::Reporter,
    )
    .await
    .expect("member");
    t.commit_audited(
        ("project", gone.project_id.get()),
        &[format!(
            "UPDATE forge.projects SET deleted_at = now(), \
             purge_after = now() + interval '7 days' WHERE id = {}",
            gone.project_id.get()
        )],
    )
    .await;
    let g_bob = group(&t, "gbob", bob.user_id).await;
    let _g_alice = group(&t, "galice", alice.user_id).await;
    let counting = Arc::new(Counting {
        db: t.db.clone(), // clone: the pool handle is shared
        pages: AtomicUsize::new(0),
    });
    let page = |uri: &'static str, who: UserId| {
        let counting = Arc::clone(&counting);
        async move { get(app(&counting, 1000), uri, Some(session(who))).await }
    };
    let (status, all) = page("/api/v1/projects", bob.user_id).await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(
        ids(&all),
        [
            first.project_id.get(),
            sys.project_id.get(),
            open.project_id.get(),
            rep.project_id.get()
        ]
    );
    // ADR-0023: the repository-kind filter is gone; it is an unknown
    // parameter (rejected, never silently ignored).
    for kind in ["spec", "system"] {
        let uri: &'static str = if kind == "spec" {
            "/api/v1/projects?kind=spec"
        } else {
            "/api/v1/projects?kind=system"
        };
        let (status, bad) = page(uri, bob.user_id).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {bad}");
        assert_eq!(bad["error_code"], ErrorCode::RequestInvalid.id());
    }
    assert!(all["items"][0].get("kind").is_none(), "no kind in the body");
    let (_, mine) = page("/api/v1/projects?membership=true", bob.user_id).await;
    assert_eq!(
        ids(&mine),
        [
            first.project_id.get(),
            sys.project_id.get(),
            rep.project_id.get()
        ],
        "not the internal project without a role"
    );
    let (_, pending) = page("/api/v1/projects?state=pending_deletion", bob.user_id).await;
    assert_eq!(ids(&pending), [gone.project_id.get()]);
    let (_, none) = page("/api/v1/projects?state=pending_deletion", alice.user_id).await;
    assert_eq!(
        ids(&none),
        Vec::<i64>::new(),
        "another's project pending deletion stays hidden"
    );
    let (_, groups) = page("/api/v1/groups?membership=true", bob.user_id).await;
    assert_eq!(ids(&groups), [g_bob.group_id.get()]);
    let (status, bad) = page("/api/v1/groups?kind=spec", bob.user_id).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");

    // A cursor handed out under membership=true opens only under it.
    let (_, page_one) = get(
        app(&counting, 1),
        "/api/v1/projects?membership=true&limit=1",
        Some(session(bob.user_id)),
    )
    .await;
    let cursor = page_one["next_cursor"]
        .as_str()
        .expect("a cursor")
        .to_owned();
    let (status, next) = get(
        app(&counting, 1000),
        &format!("/api/v1/projects?membership=true&cursor={cursor}"),
        Some(session(bob.user_id)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{next}");
    for other in ["state=live&", "", "state=pending_deletion&"] {
        let (status, refused) = get(
            app(&counting, 1000),
            &format!("/api/v1/projects?{other}cursor={cursor}"),
            Some(session(bob.user_id)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{other}: {refused}");
        assert_eq!(refused["error_code"], ErrorCode::RequestInvalid.id());
    }
    t.drop_now().await;
}
