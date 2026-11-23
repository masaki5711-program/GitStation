#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `GET /api/v1/search` (plan 20 K3): strict parameters, every hit decided
//! by `authorize()` before anything of it is built, no count, the cursor
//! bound to its caller, an AI audience never sees a confidential hit, and a
//! failed read is `FG-KNOW-0001`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::search::{CandidateFilter, SearchCandidate, SearchReader, SourceKind};
use forge_core::{
    CursorKeys, GroupId, GroupListing, GroupPageLoader, NamespaceId, NamespacePath, ProjectAccess,
    ProjectId, ProjectPageLoader, ProjectRecord, Role, UserId, Visibility,
};
use forge_errors::{ErrorCode, ForgeError};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use super::*;
use crate::app::{build, AppConfig, AppState, Lookups};
use crate::routes::lists::Lists;

const READABLE: i64 = 1; // private, Reporter
const HIDDEN: i64 = 2; // private, no role
const GUEST: i64 = 3; // private, Guest: not readable (as in the project list)
const DELETED: i64 = 4; // pending deletion, Developer
const GONE: i64 = 5; // no record any more

fn rule(err: &ForgeError) -> (ErrorCode, String, String) {
    let get = |k: &str| {
        err.context()
            .iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| v.to_string())
            .unwrap_or_default()
    };
    (err.code(), get("param"), get("rule"))
}

#[test]
fn parameters_are_strict() {
    let r = parse(Some(
        "q=%E8%A8%AD%E5%82%99+pump&scope=merge_request,project&project=7&limit=5",
    ))
    .expect("parse");
    assert_eq!(r.query.tsquery_literal(), "'設備' & 'pump':*");
    assert_eq!(r.kinds, vec![SourceKind::Project, SourceKind::MergeRequest]);
    assert_eq!(r.project, Some(ProjectId::new(7)));
    assert_eq!(r.limit, 5);
    assert_eq!(parse(Some("q=ab")).expect("default").limit, 20);
    let cases = [
        ("", "q", "missing"),
        ("q=", "q", "empty"),
        ("q=a", "q", "too_short"),
        ("q=%E8%A8%AD", "q", "too_short"),
        ("q=%ZZ", "q", "malformed"),
        ("q=a%0Ab", "q", "control_character"),
        ("q=ab&q=cd", "q", "repeated"),
        ("q=ab&scope=issue", "scope", "invalid_value"),
        ("q=ab&project=0", "project", "not_positive_decimal"),
        ("q=ab&limit=51", "limit", "out_of_range"),
        ("q=ab&cursor=", "cursor", "empty"),
        ("q=ab&page=2", "query", "unknown_parameter"),
    ];
    for (query, param, expected) in cases {
        let err = parse(Some(query)).expect_err(query);
        assert_eq!(
            rule(&err),
            (
                ErrorCode::RequestInvalid,
                param.to_owned(),
                expected.to_owned()
            ),
            "{query}"
        );
    }
    let long = format!("q={}", "a".repeat(201));
    assert_eq!(rule(&parse(Some(&long)).expect_err("long")).2, "too_long");
}

fn record(id: i64, role: Option<Role>, deleted: bool) -> ProjectRecord {
    ProjectRecord {
        id: ProjectId::new(id),
        access: ProjectAccess {
            visibility: Visibility::Private,
            role,
            pending_deletion: deleted,
        },
        namespace_id: NamespaceId::new(id * 10),
        path: NamespacePath::parse(&format!("acme/p{id}")).expect("path"),
        display_name: format!("Project {id}"),
        description: String::new(),
        created_at: "2026-11-23T00:00:00Z".to_owned(),
        created_by: None,
        repository: forge_core::ProjectRepository {
            id: forge_core::RepoId::new(id),
            state: forge_core::RepositoryState::Ready,
        },
    }
}

fn records() -> Vec<ProjectRecord> {
    vec![
        record(READABLE, Some(Role::Reporter), false),
        record(HIDDEN, None, false),
        record(GUEST, Some(Role::Guest), false),
        record(DELETED, Some(Role::Developer), true),
    ]
}

fn candidate(kind: SourceKind, project: i64, key: &str, title: &str) -> SearchCandidate {
    SearchCandidate {
        kind,
        project: ProjectId::new(project),
        key: key.to_owned(),
        title: title.to_owned(),
        body: format!("{title} の設備の説明"),
        classification: "internal".to_owned(),
        truncated: false,
        updated_at: "2026-11-23T00:00:00Z".to_owned(),
    }
}

/// Every kind in every project, best first.
fn candidates() -> Vec<SearchCandidate> {
    let mut out = Vec::new();
    for p in [HIDDEN, READABLE, GUEST, DELETED, GONE] {
        out.push(candidate(
            SourceKind::Project,
            p,
            "",
            &format!("secret-or-not {p}"),
        ));
        out.push(candidate(
            SourceKind::MergeRequest,
            p,
            "1",
            &format!("mr {p}"),
        ));
    }
    out
}

fn person() -> Principal {
    Principal::authenticated(
        UserId::new(5),
        false,
        true,
        Credential::Session,
        AccountKind::Human,
    )
}

fn other_person() -> Principal {
    Principal::authenticated(
        UserId::new(6),
        false,
        true,
        Credential::Session,
        AccountKind::Human,
    )
}

fn agent() -> Principal {
    Principal::authenticated(
        UserId::new(5),
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
        AccountKind::Human,
    )
    .as_agent(true)
}

fn query() -> SearchQuery {
    SearchQuery::parse("設備").expect("query")
}

#[test]
fn invisible_project_title_never_returned() {
    let decided = walk(&person(), &candidates(), &records(), &query(), 50, 200).expect("walk");
    let shown: Vec<(String, i64)> = decided
        .items
        .iter()
        .map(|h| (h.kind.clone(), h.project_id))
        .collect();
    assert_eq!(
        shown,
        vec![
            ("project".to_owned(), READABLE),
            ("merge_request".to_owned(), READABLE),
        ]
    );
    assert!(decided.items.iter().all(|h| h.project_id == READABLE));
    assert_eq!(decided.consumed, 10);
    assert_eq!(decided.hidden, 8);
    // The snippet is cut from the body with character ranges.
    let first = &decided.items[0];
    assert_eq!(first.iid, None);
    assert_eq!(decided.items[1].iid, Some(1));
    assert!(!first.snippet.highlights.is_empty());
    assert_eq!(first.project_path, "acme/p1");
}

#[test]
fn guest_on_private_project_gets_no_hits() {
    let only_guest: Vec<SearchCandidate> = candidates()
        .into_iter()
        .filter(|c| c.project == ProjectId::new(GUEST))
        .collect();
    let decided = walk(&person(), &only_guest, &records(), &query(), 50, 200).expect("walk");
    assert!(decided.items.is_empty());
}

#[test]
fn agent_never_sees_confidential_hit() {
    let mut list = candidates();
    for c in &mut list {
        c.classification = "confidential".to_owned();
    }
    let decided = walk(&agent(), &list, &records(), &query(), 50, 200).expect("walk");
    assert!(decided.items.is_empty());
    let person = walk(&person(), &list, &records(), &query(), 50, 200).expect("walk");
    assert_eq!(person.items.len(), 2);
}

#[test]
fn the_walk_stops_at_the_limit_and_the_budget() {
    let decided = walk(&person(), &candidates(), &records(), &query(), 1, 200).expect("walk");
    assert_eq!(decided.items.len(), 1);
    assert_eq!(decided.consumed, 3, "two hidden, then the first shown");
    let cut = walk(&person(), &candidates(), &records(), &query(), 50, 2).expect("walk");
    assert!(cut.items.is_empty());
    assert_eq!(cut.consumed, 2);
}

// --- the route -------------------------------------------------------------

/// Scripted reads; records the filter each call got.
#[derive(Default)]
struct Fake {
    candidates: Vec<SearchCandidate>,
    fail: Option<ErrorCode>,
    hang: bool,
    calls: AtomicUsize,
    pages: AtomicUsize,
    filters: Mutex<Vec<(CandidateFilter, u32)>>,
}

impl ProjectPageLoader for Fake {
    /// Every scripted project, whatever the viewer may see (in id order).
    fn project_page<'a>(
        &'a self,
        after: Option<ProjectId>,
        _: UserId,
        _: Option<UserId>,
        max_rows: u32,
    ) -> LookupFuture<'a, Vec<ProjectRecord>> {
        self.pages.fetch_add(1, Ordering::SeqCst);
        let rows: Vec<ProjectRecord> = records()
            .into_iter()
            .filter(|r| after.is_none_or(|a| r.id > a))
            .take(usize::try_from(max_rows).expect("fits"))
            .collect();
        Box::pin(async move { Ok(rows) })
    }
}

impl GroupPageLoader for Fake {
    fn group_page<'a>(
        &'a self,
        _: Option<GroupId>,
        _: UserId,
        _: u32,
    ) -> LookupFuture<'a, Vec<GroupListing>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

impl SearchReader for Fake {
    fn candidates<'a>(
        &'a self,
        _: &'a str,
        filter: &'a CandidateFilter,
        offset: u32,
        max_rows: u32,
    ) -> LookupFuture<'a, Vec<SearchCandidate>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.filters
            .lock()
            .expect("lock")
            .push((filter.clone(), offset));
        let rows: Vec<SearchCandidate> = self
            .candidates
            .iter()
            .filter(|c| !(filter.exclude_confidential && c.classification == "confidential"))
            // As the SQL: only the caller's readable sets, then the project.
            .filter(|c| match c.kind {
                SourceKind::Project => filter.readable_projects.contains(&c.project),
                SourceKind::MergeRequest => filter.readable_merge_requests.contains(&c.project),
            })
            .filter(|c| filter.project.is_none_or(|p| p == c.project))
            .skip(usize::try_from(offset).expect("fits"))
            .take(usize::try_from(max_rows).expect("fits"))
            .cloned()
            .collect();
        let (fail, hang) = (self.fail, self.hang);
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            match fail {
                Some(code) => Err(ForgeError::new(code).with_context("reason", "scripted")),
                None => Ok(rows),
            }
        })
    }
}

fn app(fake: &Arc<Fake>) -> axum::Router {
    struct NoLookups;
    impl forge_core::NamespaceResolver for NoLookups {
        fn resolve<'a>(
            &'a self,
            _: &'a forge_core::LookupPath,
            _: Option<UserId>,
        ) -> LookupFuture<'a, forge_core::Resolution> {
            Box::pin(async { Ok(forge_core::Resolution::Absent) })
        }
    }
    impl forge_core::ProjectAccessLoader for NoLookups {
        fn load_project<'a>(
            &'a self,
            _: ProjectId,
            _: Option<UserId>,
        ) -> LookupFuture<'a, Option<ProjectRecord>> {
            Box::pin(async { Ok(None) })
        }
    }
    let none = Arc::new(NoLookups);
    let lookups = Lookups {
        namespaces: Arc::clone(&none) as Arc<dyn forge_core::NamespaceResolver>,
        projects: none as Arc<dyn forge_core::ProjectAccessLoader>,
    };
    let keys = CursorKeys::parse(&format!("1 {}\n", "ab".repeat(32))).expect("keys");
    let lists = Lists::new(
        Arc::clone(fake) as Arc<dyn ProjectPageLoader>,
        Arc::clone(fake) as Arc<dyn GroupPageLoader>,
        Arc::new(keys),
    )
    .with_search(Arc::clone(fake) as Arc<dyn SearchReader>);
    let state = AppState::new("test", Vec::new(), crate::tests::no_tokens(), lookups)
        .with_lookup_timeout(Duration::from_millis(100))
        .with_lists(lists);
    build(crate::app::registry(), state, AppConfig::default()).expect("router")
}

async fn get(fake: &Arc<Fake>, uri: &str, who: Option<Principal>) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    if let Some(p) = who {
        req.extensions_mut().insert(p);
    }
    let res = app(fake).oneshot(req).await.expect("infallible");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn fake() -> Arc<Fake> {
    Arc::new(Fake {
        candidates: candidates(),
        ..Fake::default()
    })
}

const URI: &str = "/api/v1/search?q=%E8%A8%AD%E5%82%99&limit=1";

#[tokio::test]
async fn no_total_count_in_response() {
    let fake = fake();
    let (status, body) = get(&fake, URI, Some(person())).await;
    assert_eq!(status, StatusCode::OK);
    let keys: Vec<&str> = body
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, vec!["has_more", "items", "next_cursor"]);
    assert_eq!(body["items"][0]["project_id"], READABLE);
    assert_eq!(body["has_more"], true);
}

#[tokio::test]
async fn the_cursor_continues_after_the_consumed_candidates() {
    let fake = fake();
    let (_, first) = get(&fake, URI, Some(person())).await;
    let cursor = first["next_cursor"].as_str().expect("cursor").to_owned();
    let (status, second) = get(&fake, &format!("{URI}&cursor={cursor}"), Some(person())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["items"][0]["kind"], "merge_request");
    let offsets: Vec<u32> = fake
        .filters
        .lock()
        .expect("lock")
        .iter()
        .map(|(_, o)| *o)
        .collect();
    assert_eq!(
        offsets,
        vec![0, 1],
        "hidden rows are never read, so never skipped"
    );
}

#[tokio::test]
async fn cursor_of_other_user_rejected() {
    let fake = fake();
    let (_, first) = get(&fake, URI, Some(person())).await;
    let cursor = first["next_cursor"].as_str().expect("cursor").to_owned();
    let (status, body) = get(
        &fake,
        &format!("{URI}&cursor={cursor}"),
        Some(other_person()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error_code"], "FG-WEB-0006");
    // Another query is another purpose too.
    let other = format!("/api/v1/search?q=pump&limit=1&cursor={cursor}");
    let (status, _) = get(&fake, &other, Some(person())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn agent_search_excludes_confidential_rows_from_the_read() {
    let fake = fake();
    let (status, _) = get(&fake, URI, Some(agent())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(&fake, URI, Some(person())).await;
    assert_eq!(status, StatusCode::OK);
    let excluded: Vec<bool> = fake
        .filters
        .lock()
        .expect("lock")
        .iter()
        .map(|(f, _)| f.exclude_confidential)
        .collect();
    assert_eq!(excluded, vec![true, false]);
}

#[tokio::test]
async fn denied_for_anonymous_and_narrow_tokens() {
    let fake = fake();
    let (status, body) = get(&fake, URI, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error_code"], "FG-AUTH-0001");
    let repo_only = Principal::authenticated(
        UserId::new(5),
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
        AccountKind::Human,
    );
    let (status, _) = get(&fake, URI, Some(repo_only)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "nothing was read");
}

#[tokio::test]
async fn search_store_failure_is_search_unavailable() {
    let fake = Arc::new(Fake {
        fail: Some(ErrorCode::DbQueryFailed),
        ..Fake::default()
    });
    let (status, body) = get(&fake, URI, Some(person())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error_code"], "FG-KNOW-0001");
    let hanging = Arc::new(Fake {
        hang: true,
        ..Fake::default()
    });
    let (status, body) = get(&hanging, URI, Some(person())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error_code"], "FG-KNOW-0001");
}

#[tokio::test]
async fn a_bad_query_is_refused_before_any_read() {
    let fake = fake();
    let (status, body) = get(&fake, "/api/v1/search?q=a", Some(person())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error_code"], "FG-WEB-0006");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

// --- review of K3 (HIGH): hidden matches are not countable ------------------

/// `n` merge requests in `project`, titled `mr <project>-<i>`.
fn many(project: i64, n: usize) -> Vec<SearchCandidate> {
    (1..=n)
        .map(|i| {
            candidate(
                SourceKind::MergeRequest,
                project,
                &i.to_string(),
                &format!("mr {project}-{i}"),
            )
        })
        .collect()
}

#[tokio::test]
async fn hidden_only_matches_give_an_empty_last_page() {
    let mut hidden = many(HIDDEN, 400);
    hidden.extend(many(GUEST, 50));
    hidden.extend(many(DELETED, 50));
    hidden.extend(many(GONE, 50));
    let fake = Arc::new(Fake {
        candidates: hidden,
        ..Fake::default()
    });
    let (status, body) = get(&fake, URI, Some(person())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"], serde_json::json!([]));
    assert_eq!(body["has_more"], false, "nothing hints at hidden matches");
    assert_eq!(body["next_cursor"], Value::Null);
    // The read was narrowed to what the caller may read, before ranking.
    let filters = fake.filters.lock().expect("lock");
    let (filter, _) = &filters[0];
    assert_eq!(filter.readable_projects, vec![ProjectId::new(READABLE)]);
    assert_eq!(
        filter.readable_merge_requests,
        vec![ProjectId::new(READABLE)]
    );
}

#[tokio::test]
async fn hidden_matches_never_shorten_a_page() {
    // 300 hidden matches rank before 5 readable ones.
    let mut list = many(HIDDEN, 300);
    list.extend(many(READABLE, 5));
    let fake = Arc::new(Fake {
        candidates: list,
        ..Fake::default()
    });
    let uri = "/api/v1/search?q=%E8%A8%AD%E5%82%99&limit=5";
    let (status, body) = get(&fake, uri, Some(person())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().expect("items").len(), 5);
    assert_eq!(body["has_more"], false);
}

#[tokio::test]
async fn private_project_filter_is_like_a_missing_one() {
    let mut list = many(HIDDEN, 30);
    list.extend(many(READABLE, 3));
    let fake = Arc::new(Fake {
        candidates: list,
        ..Fake::default()
    });
    let hidden = format!("/api/v1/search?q=%E8%A8%AD%E5%82%99&limit=1&project={HIDDEN}");
    let missing = "/api/v1/search?q=%E8%A8%AD%E5%82%99&limit=1&project=999";
    let a = get(&fake, &hidden, Some(person())).await;
    let b = get(&fake, missing, Some(person())).await;
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(a, b);
    assert_eq!(a.1["has_more"], false);
}
