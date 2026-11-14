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
use forge_core::act_as::ActAsKeys;
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{ObjectId, ProjectId, RefName, RepoId, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::ci_runners::lease;
use forge_db::ci_runners::{register, NewRunner};
use forge_db::groups::{self, NewGroup};
use forge_db::pipelines::{
    self as pipelines, NewCiJob, NewPipeline, PipelineRequest, Planned, Source,
};
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

/// The act-as keys of the synthetic kanflow integration.
const KEYS: &str = "1:BwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSY= active";

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
        // Real tokens (runner, job, service account) are authenticated by
        // forge-db; people are injected as request extensions.
        Arc::new(db.clone()),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
    .with_review(Review::new(db.clone(), Arc::new(NoBranches)))
    .with_act_as(forge_integ::ActAs::new(
        db.clone(),
        Some(ActAsKeys::parse(KEYS).expect("keys")),
    ));
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
    send(db, uri, Some(who), &[]).await
}

/// `GET uri` as `who` (injected) or with `headers` only (a bearer token).
async fn send(db: &Db, uri: &str, who: Option<Principal>, headers: &[(&str, &str)]) -> Answer {
    let mut builder = Request::builder().method("GET").uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let mut req = builder.body(Body::empty()).expect("request");
    if let Some(who) = who {
        req.extensions_mut().insert(who);
    }
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
    assert_eq!(a.json[scope]["truncated"], false, "{}", a.json);
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

#[tokio::test]
async fn a_count_that_stops_at_its_scan_bound_says_so() {
    let t = FreshDb::migrated().await;
    let dave = user(&t, "dave").await;
    let hugo = user(&t, "hugo").await;
    let mine = project(&t, "acme", dave, &[]).await;
    // 1001 merge requests of dave's in hugo's project (dave cannot see
    // it): more candidates than one count scans (10 pages of 100), none
    // visible. The newest is one dave sees.
    let hidden = project(&t, "secret", hugo, &[]).await;
    let h = hidden.get();
    t.commit_audited(
        ("project", h),
        &[
            format!(
                "INSERT INTO forge.merge_request_counters (project_id, last_iid) \
                 VALUES ({h}, 1001)"
            ),
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch, head_sha) SELECT {h}, i, 'Change ' || i, {}, \
                 'refs/heads/b' || i, 'refs/heads/main', lpad(to_hex(i), 40, '0') \
                 FROM generate_series(1, 1001) AS i",
                dave.get()
            ),
        ],
    )
    .await;
    let _visible = mr(&t, mine, 1, dave, false).await;
    let counts = get(&t.db, COUNTS, session(dave)).await;
    assert_eq!(counts.status, StatusCode::OK, "{}", counts.json);
    // 1000 candidates scanned, 999 hidden ones of them: a lower bound,
    // said as such.
    assert_eq!(counts.json["authored"]["count"], 1, "{}", counts.json);
    assert_eq!(
        counts.json["authored"]["truncated"], true,
        "{}",
        counts.json
    );
    assert_eq!(counts.json["authored"]["capped"], false);
    assert_eq!(counts.json["approvable"]["truncated"], false);
    // hugo's own count is not cut short (his candidates fit the scan).
    let hugos = get(&t.db, COUNTS, session(hugo)).await;
    assert_eq!(hugos.json["authored"]["truncated"], false, "{}", hugos.json);
    // The page says more may follow even when it shows nothing.
    let page = get(
        &t.db,
        &format!("{LIST}?scope=authored&limit=100"),
        session(dave),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.json);
    assert!(page.json["next_before"].is_i64(), "{}", page.json);
}

#[tokio::test]
async fn runner_job_and_act_as_credentials_are_refused_on_both_routes() {
    let t = FreshDb::migrated().await;
    let olga = users::create(&t.db, &ctx("req-user-olga"), &{
        let mut n = NewUser::human("olga", "olga", "olga@example.test");
        n.is_admin = true;
        n
    })
    .await
    .expect("olga")
    .user_id;
    let web = project(&t, "acme", olga, &[]).await;
    let _one = mr(&t, web, 1, olga, false).await;
    // A runner's token, and the token of a job it claimed.
    let runner = register(
        &t.db,
        &ChangeContext::new(
            Actor::User(olga),
            RequestId::parse("req-runner").expect("id"),
        ),
        &NewRunner {
            name: "linux-1".to_owned(),
            description: String::new(),
            tags: Vec::new(),
        },
    )
    .await
    .expect("runner");
    let _pipeline = pipelines::create(
        &t.db,
        &NewPipeline {
            ai_eval_job: None,
            request: PipelineRequest {
                project: web,
                git_ref: RefName::parse("refs/heads/main").expect("ref"),
                sha: ObjectId::parse(&"b".repeat(40)).expect("sha"),
                source: Source::Manual,
                merge_request: None,
                triggered_by: None,
            },
            request_id: "req-seed".to_owned(),
            definition_hash: Some(pipelines::definition_hash(b"version: 1")),
            protected: false,
            copy_mark: pipelines::CopyMark::Open,
            planned: Planned::Jobs(vec![NewCiJob {
                name: "build".to_owned(),
                stage: "build".to_owned(),
                wave: 0,
                needs: Vec::new(),
                image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
                timeout_s: 600,
                tags: Vec::new(),
                secrets: Vec::new(),
            }]),
        },
    )
    .await
    .expect("pipeline");
    let job = lease::claim(&t.db, runner.runner.id)
        .await
        .expect("claim")
        .expect("a job")
        .token
        .reveal();
    // A service account's act-as token, acting for olga.
    let sa = forge_db::service_accounts::create(
        &t.db,
        &ctx("req-sa"),
        &forge_db::service_accounts::NewServiceAccount {
            username: "kanflow-sync".to_owned(),
            display_name: "kanflow".to_owned(),
            owner: olga,
            purpose: "synthetic act-as caller".to_owned(),
        },
        olga,
    )
    .await
    .expect("sa")
    .user_id;
    let _integration = forge_db::integrations::create(
        &t.db,
        &ctx("req-int"),
        &forge_db::integrations::NewIntegration {
            kind: forge_core::IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        olga,
    )
    .await
    .expect("integration");
    let kanflow = forge_db::tokens::issue_for_service_account(
        &t.db,
        &ctx("req-token"),
        &forge_db::tokens::NewToken {
            user: sa,
            name: "kanflow".to_owned(),
            scopes: vec![Scope::ReadApi, Scope::ActAs],
            expires_in: None,
            is_agent: false,
        },
        olga,
    )
    .await
    .expect("token")
    .token
    .reveal();

    let runner_auth = format!("Bearer {}", runner.token.reveal());
    let job_auth = format!("Bearer {job}");
    let kanflow_auth = format!("Bearer {kanflow}");
    let act_as = format!("user:{}", olga.get());
    for uri in [format!("{LIST}?scope=authored"), COUNTS.to_owned()] {
        // The person herself is allowed (the control).
        let own = get(&t.db, &uri, session(olga)).await;
        assert_eq!(own.status, StatusCode::OK, "{uri}: {}", own.json);
        for (who, headers) in [
            ("runner", vec![("authorization", runner_auth.as_str())]),
            ("job", vec![("authorization", job_auth.as_str())]),
            (
                "act-as",
                vec![
                    ("authorization", kanflow_auth.as_str()),
                    ("forge-act-as", act_as.as_str()),
                ],
            ),
        ] {
            let a = send(&t.db, &uri, None, &headers).await;
            assert!(
                a.status == StatusCode::FORBIDDEN || a.status == StatusCode::NOT_FOUND,
                "{who} {uri}: {} {}",
                a.status,
                a.json
            );
            assert!(
                a.code().starts_with("FG-AUTH-") || a.code() == "FG-REPO-0005",
                "{who} {uri}: {}",
                a.json
            );
            assert!(a.json.get("merge_requests").is_none(), "{who} {uri}");
            assert!(a.json.get("authored").is_none(), "{who} {uri}");
        }
    }
}
