#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The merge requests of the caller's own work (plan 15 MR 6) through the
//! product router against the development database (ADR-0011), with the
//! real loaders and forge-review: `GET /api/v1/user/merge-requests?scope=`
//! and `…/counts`. A merge request in a project the caller cannot see is
//! neither listed nor counted; a head the caller approved is not
//! approvable, nor is the caller's own merge request or a draft; a
//! Reporter approves nothing; `attention` keeps the caller's own merge
//! requests with a conflict or a failed required check only; pages chain
//! by `next_before`; the query is strict (`FG-WEB-0006`); a token without
//! `read_api` and an anonymous caller are refused (act-as, runner and job
//! credentials: the permission matrix's generated rows). Principals are
//! inserted as request extensions (the authentication layer is tested
//! elsewhere). Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal, Role, Scope};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{ProjectId, RefName, RepoId, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, NewGroup};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{BranchProbe, Review};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

const LIST: &str = "/api/v1/user/merge-requests";
const COUNTS: &str = "/api/v1/user/merge-requests/counts";

struct NoTokens;

impl forge_core::Authenticator for NoTokens {
    fn authenticate<'a>(
        &'a self,
        _: &'a RequestId,
        _: &'a str,
    ) -> forge_core::authenticator::AuthFuture<'a> {
        Box::pin(async { Err(ForgeError::new(ErrorCode::Unauthenticated)) })
    }
}

/// No branch is looked up by these reads.
struct NoBranches;

impl BranchProbe for NoBranches {
    fn branch_head<'a>(
        &'a self,
        _repo: RepoId,
        _branch: &'a RefName,
        _caller: &'a str,
    ) -> LookupFuture<'a, forge_core::ObjectId> {
        Box::pin(async { Err(ForgeError::new(ErrorCode::Internal)) })
    }
}

fn router(db: &Db) -> axum::Router {
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
    .with_review(Review::new(db.clone(), Arc::new(NoBranches)));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn user(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
    .user_id
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
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

/// A private project `<group>/web` owned by `owner`, with `members`.
async fn project(t: &FreshDb, group: &str, owner: UserId, members: &[(UserId, Role)]) -> ProjectId {
    let g = groups::create(
        &t.db,
        &ctx(&format!("req-{group}")),
        &NewGroup {
            segment: group.into(),
            parent: None,
            display_name: group.into(),
            description: String::new(),
            owner,
        },
    )
    .await
    .expect("group");
    let p = projects::create(
        &t.db,
        &ctx(&format!("req-{group}-web")),
        &NewProject::new("web", g.namespace_id, "Web", owner),
    )
    .await
    .expect("project")
    .project_id;
    for (who, role) in members {
        projects::add_member(&t.db, &ctx("req-member"), p, *who, *role)
            .await
            .expect("member");
    }
    p
}

fn head(n: i64) -> String {
    format!("{n:040x}")
}

/// Merge request `iid` of `project` by `author` on head `head(iid)`.
async fn mr(t: &FreshDb, project: ProjectId, iid: i64, author: UserId, draft: bool) -> i64 {
    let p = project.get();
    let counter = if iid == 1 {
        format!("INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 1)")
    } else {
        format!("UPDATE forge.merge_request_counters SET last_iid = {iid} WHERE project_id = {p}")
    };
    t.commit_audited(
        ("project", p),
        &[
            counter,
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch, draft, head_sha) VALUES ({p}, {iid}, \
                 'Change {iid}', {}, 'refs/heads/b{iid}', 'refs/heads/main', {draft}, '{}')",
                author.get(),
                head(iid)
            ),
        ],
    )
    .await;
    let rows = t
        .json_rows(&format!(
            "SELECT id FROM forge.merge_requests WHERE project_id = {p} AND iid = {iid}"
        ))
        .await;
    rows[0]["id"].as_i64().expect("id")
}

/// A live approval of `approver` on `head_sha` (revoked with `revoked`).
async fn approval(
    t: &FreshDb,
    project: ProjectId,
    mr_id: i64,
    approver: UserId,
    head_sha: &str,
    revoked: Option<&str>,
) {
    let p = project.get();
    let (at, reason) = match revoked {
        Some(r) => ("now()".to_owned(), format!("'{r}'")),
        None => ("NULL".to_owned(), "NULL".to_owned()),
    };
    t.commit_audited(
        ("project", p),
        &[format!(
            "INSERT INTO forge.merge_request_approvals (merge_request_id, project_id, \
             approver_id, head_sha, target_branch, revoked_at, revoked_reason) VALUES \
             ({mr_id}, {p}, {}, '{head_sha}', 'refs/heads/main', {at}, {reason})",
            approver.get()
        )],
    )
    .await;
}

struct Answer {
    status: StatusCode,
    json: Value,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }

    /// `(project_id, iid)` of every listed row.
    fn rows(&self) -> Vec<(i64, i64)> {
        self.json["merge_requests"]
            .as_array()
            .unwrap_or_else(|| panic!("no list: {}", self.json))
            .iter()
            .map(|m| {
                (
                    m["project_id"].as_i64().expect("project"),
                    m["iid"].as_i64().expect("iid"),
                )
            })
            .collect()
    }
}

async fn get(db: &Db, uri: &str, who: Principal) -> Answer {
    let mut req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    req.extensions_mut().insert(who);
    let res = router(db).oneshot(req).await.expect("infallible");
    let status = res.status();
    let raw = res.into_body().collect().await.expect("body").to_bytes();
    Answer {
        status,
        json: serde_json::from_slice(&raw).unwrap_or(Value::Null),
    }
}

async fn scope(db: &Db, scope: &str, who: Principal) -> Answer {
    let a = get(db, &format!("{LIST}?scope={scope}"), who).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["scope"], scope);
    a
}

fn count(a: &Answer, scope: &str) -> i64 {
    assert_eq!(a.json[scope]["capped"], false, "{}", a.json);
    a.json[scope]["count"].as_i64().expect("count")
}

#[tokio::test]
async fn the_lists_and_counts_hold_only_what_the_caller_may_see_and_do() {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    let dave = user(&t, "dave").await;
    let rita = user(&t, "rita").await;
    let hugo = user(&t, "hugo").await;
    let web = project(
        &t,
        "acme",
        olga,
        &[(dave, Role::Developer), (rita, Role::Reporter)],
    )
    .await;
    // A project dave cannot see (hugo's), holding a merge request of dave's
    // and one dave would otherwise be offered to approve.
    let hidden = project(&t, "secret", hugo, &[]).await;
    let w = web.get();

    // dave's own: 1 (nothing wrong), 6 (conflict), 7 (failed required
    // check), 8 (a failed check no rule requires).
    let _one = mr(&t, web, 1, dave, false).await;
    // olga's: 2 approvable, 3 draft, 4 approved by dave on its head, 5
    // approved by dave on an older head only.
    let _two = mr(&t, web, 2, olga, false).await;
    let _three = mr(&t, web, 3, olga, true).await;
    let four = mr(&t, web, 4, olga, false).await;
    let five = mr(&t, web, 5, olga, false).await;
    let six = mr(&t, web, 6, dave, false).await;
    let _seven = mr(&t, web, 7, dave, false).await;
    let _eight = mr(&t, web, 8, dave, false).await;
    let _h1 = mr(&t, hidden, 1, dave, false).await;
    let _h2 = mr(&t, hidden, 2, hugo, false).await;
    approval(&t, web, four, dave, &head(4), None).await;
    approval(&t, web, five, dave, &head(99), Some("new_commits")).await;
    t.commit_audited(
        ("project", w),
        &[
            format!(
                "INSERT INTO forge.merge_request_mergeability (merge_request_id, project_id, \
                 status, checked_head_sha, last_decided) VALUES ({six}, {w}, \
                 'cannot_be_merged', '{}', 'cannot_be_merged')",
                head(6)
            ),
            format!(
                "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
                 VALUES ({w}, 'refs/heads/main', 'developer')"
            ),
            format!(
                "INSERT INTO forge.protected_ref_merge_rules (protected_ref_id, project_id, \
                 required_checks) SELECT id, project_id, ARRAY['ci/build']::text[] \
                 FROM forge.protected_refs WHERE project_id = {w}"
            ),
            format!(
                "INSERT INTO forge.check_runs (project_id, head_sha, name, status, conclusion, \
                 completed_at, reported_by, request_id) VALUES \
                 ({w}, '{}', 'ci/build', 'completed', 'failure', now(), {o}, 'req-check'), \
                 ({w}, '{}', 'ci/lint', 'completed', 'failure', now(), {o}, 'req-check'), \
                 ({w}, '{}', 'ci/build', 'completed', 'success', now(), {o}, 'req-check')",
                head(7),
                head(8),
                head(1),
                o = olga.get()
            ),
        ],
    )
    .await;

    // authored: dave's own in projects he sees, newest first; the hidden
    // project's is not listed.
    let authored = scope(&t.db, "authored", session(dave)).await;
    assert_eq!(authored.rows(), vec![(w, 8), (w, 7), (w, 6), (w, 1)]);
    assert_eq!(authored.json["next_before"], Value::Null);
    let first = &authored.json["merge_requests"][0];
    assert_eq!(first["project_path"], "acme/web");
    assert_eq!(first["author"]["username"], "dave");
    assert_eq!(first["target_branch"], "main");
    assert_eq!(first["attention"], serde_json::json!([]));

    // approvable: never the caller's own, a draft, or a head already
    // approved; an approval of an older head does not count.
    let approvable = scope(&t.db, "approvable", session(dave)).await;
    assert_eq!(approvable.rows(), vec![(w, 5), (w, 2)]);
    // The author never sees their own; a Reporter may approve nothing.
    let olgas = scope(&t.db, "approvable", session(olga)).await;
    assert_eq!(olgas.rows(), vec![(w, 8), (w, 7), (w, 6), (w, 1)]);
    let ritas = scope(&t.db, "approvable", session(rita)).await;
    assert_eq!(ritas.rows(), Vec::<(i64, i64)>::new());
    // A token without `merge_approve` may approve nothing either (the same
    // trial as the merge request screen's button).
    let read_api = scope(&t.db, "approvable", token(dave, &[Scope::ReadApi])).await;
    assert_eq!(read_api.rows(), Vec::<(i64, i64)>::new());

    // attention: the conflict and the failed required check, with why.
    let attention = scope(&t.db, "attention", session(dave)).await;
    assert_eq!(attention.rows(), vec![(w, 7), (w, 6)]);
    assert_eq!(
        attention.json["merge_requests"][0]["attention"],
        serde_json::json!(["required_check_failed"])
    );
    assert_eq!(
        attention.json["merge_requests"][1]["attention"],
        serde_json::json!(["conflict"])
    );

    // counts: the same rows, the hidden project's never counted.
    let counts = get(&t.db, COUNTS, session(dave)).await;
    assert_eq!(counts.status, StatusCode::OK, "{}", counts.json);
    assert_eq!(count(&counts, "authored"), 4);
    assert_eq!(count(&counts, "approvable"), 2);
    assert_eq!(count(&counts, "attention"), 2);
    let hugos = get(&t.db, COUNTS, session(hugo)).await;
    assert_eq!(count(&hugos, "authored"), 1);
    // dave's merge request in hugo's project is hugo's to approve.
    assert_eq!(count(&hugos, "approvable"), 1);
    assert_eq!(count(&hugos, "attention"), 0);

    // dave loses the project: nothing of it is listed or counted any more.
    t.commit_audited(
        ("project", w),
        &[format!(
            "DELETE FROM forge.project_members WHERE project_id = {w} AND user_id = {}",
            dave.get()
        )],
    )
    .await;
    let gone = get(&t.db, COUNTS, session(dave)).await;
    assert_eq!(count(&gone, "authored"), 0, "{}", gone.json);
    assert_eq!(count(&gone, "approvable"), 0);
    assert_eq!(count(&gone, "attention"), 0);
    let none = scope(&t.db, "authored", session(dave)).await;
    assert_eq!(none.rows(), Vec::<(i64, i64)>::new());
}

#[tokio::test]
async fn pages_chain_by_next_before() {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    let web = project(&t, "acme", olga, &[]).await;
    let w = web.get();
    for iid in 1..=3 {
        let _id = mr(&t, web, iid, olga, false).await;
    }
    let mut seen = Vec::new();
    let mut uri = format!("{LIST}?scope=authored&limit=2");
    for _ in 0..3 {
        let page = get(&t.db, &uri, session(olga)).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.json);
        seen.extend(page.rows());
        match page.json["next_before"].as_i64() {
            Some(before) => uri = format!("{LIST}?scope=authored&limit=2&before={before}"),
            None => break,
        }
    }
    assert_eq!(seen, vec![(w, 3), (w, 2), (w, 1)]);
}

#[tokio::test]
async fn the_query_is_strict_and_only_the_persons_reading_credential_passes() {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    for uri in [
        LIST.to_owned(),
        format!("{LIST}?scope=mine"),
        format!("{LIST}?scope=authored&limit=0"),
        format!("{LIST}?scope=authored&before=x"),
        format!("{LIST}?scope=authored&page=2"),
    ] {
        let a = get(&t.db, &uri, session(olga)).await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::BAD_REQUEST, "FG-WEB-0006"),
            "{uri}: {}",
            a.json
        );
    }
    for uri in [format!("{LIST}?scope=authored"), COUNTS.to_owned()] {
        let read_api = get(&t.db, &uri, token(olga, &[Scope::ReadApi])).await;
        assert_eq!(read_api.status, StatusCode::OK, "{uri}: {}", read_api.json);
        let repo_only = get(&t.db, &uri, token(olga, &[Scope::ReadRepository])).await;
        assert_eq!(
            (repo_only.status, repo_only.code()),
            (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
            "{uri}: {}",
            repo_only.json
        );
        let anonymous = get(&t.db, &uri, Principal::anonymous()).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{uri}");
    }
}
