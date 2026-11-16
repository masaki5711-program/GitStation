#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The caller's activity (plan 15 MR 8) through the product router against
//! the development database (ADR-0011), with the real loaders and
//! forge-review: `GET /api/v1/user/activity`. Only projects the caller is
//! involved in (a role); a private project without a role yields nothing
//! and its name never appears, nor does an internal project the caller
//! merely reads; maintainer-class events (`member.added`), an unknown
//! stored type and an integration-class type the list does not show stay
//! hidden; a Guest sees nothing and the hidden rows are logged as a count
//! with the request id; pages chain by `next_before` across events of one
//! timestamp; the query is strict (`FG-WEB-0006`); a token without
//! `read_api`, an agent's token, a service account, a bot, a runner, a job
//! and an act-as request are refused. Principals are inserted as request
//! extensions unless a real token is named. Synthetic data only.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Role, Scope};
use forge_core::act_as::ActAsKeys;
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{ObjectId, ProjectId, RefName, RepoId, RequestId, UserId, Visibility};
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
use forge_review::{BranchProbe, Review, SHOWN_TYPES};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

const ACTIVITY: &str = "/api/v1/user/activity";

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

fn principal(user: UserId, credential: Credential, kind: AccountKind) -> Principal {
    Principal::authenticated(user, false, true, credential, kind)
}

fn session(user: UserId) -> Principal {
    principal(user, Credential::Session, AccountKind::Human)
}

fn token(user: UserId, scopes: &[Scope]) -> Principal {
    principal(user, Credential::Token(scopes.to_vec()), AccountKind::Human)
}

/// A project `<group>/web` of `visibility` owned by `owner`, with `members`.
async fn project(
    t: &FreshDb,
    group: &str,
    owner: UserId,
    visibility: Visibility,
    members: &[(UserId, Role)],
) -> ProjectId {
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
    let mut new = NewProject::new("web", g.namespace_id, "Web", owner);
    new.visibility = visibility;
    let p = projects::create(&t.db, &ctx(&format!("req-{group}-web")), &new)
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

/// The one merge request `iid` of `project`; its id.
async fn mr(t: &FreshDb, project: ProjectId, iid: i64, author: UserId) -> i64 {
    let p = project.get();
    t.commit_audited(
        ("project", p),
        &[
            format!(
                "INSERT INTO forge.merge_request_counters (project_id, last_iid) \
                 VALUES ({p}, {iid})"
            ),
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch, head_sha) VALUES ({p}, {iid}, 'Change {iid}', \
                 {}, 'refs/heads/b{iid}', 'refs/heads/main', '{}')",
                author.get(),
                "a".repeat(40)
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

async fn sequence(t: &FreshDb) {
    forge_db::sequencer::run_once(
        &t.db,
        forge_db::sequencer::BatchSize::MAX,
        forge_db::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
}

/// `events` (type, payload) of `project` by `actor`, in one transaction
/// (one `occurred_at`) with its audit row; then every event is numbered.
async fn events(t: &FreshDb, project: ProjectId, actor: UserId, events: &[(&str, String)]) {
    let p = project.get();
    let values: Vec<String> = events
        .iter()
        .map(|(kind, payload)| {
            format!(
                "(gen_random_uuid(), '{kind}', 1, 'project', {p}, {p}, 'user', {a}, 'r-act', \
                 '{payload}'::jsonb)",
                a = actor.get()
            )
        })
        .collect();
    t.exec_superuser_tx(&[
        format!(
            "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, \
             resource_id, request_id) VALUES ('system', 'probe.activity', 'succeeded', \
             'project', {p}, 'r-act')"
        ),
        format!(
            "INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
             aggregate_id, project_id, actor_kind, actor_user_id, request_id, payload) VALUES {}",
            values.join(", ")
        ),
    ])
    .await;
    sequence(t).await;
}

struct Answer {
    status: StatusCode,
    json: Value,
    raw: String,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }

    fn list(&self) -> &Vec<Value> {
        self.json["events"]
            .as_array()
            .unwrap_or_else(|| panic!("no list: {}", self.json))
    }

    fn types(&self) -> Vec<&str> {
        self.list()
            .iter()
            .map(|e| e["type"].as_str().expect("type"))
            .collect()
    }

    fn seqs(&self) -> Vec<i64> {
        self.list()
            .iter()
            .map(|e| e["seq"].as_i64().expect("seq"))
            .collect()
    }
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
        raw: String::from_utf8_lossy(&raw).into_owned(),
    }
}

async fn get(db: &Db, uri: &str, who: Principal) -> Answer {
    send(db, uri, Some(who), &[]).await
}

async fn ok(db: &Db, uri: &str, who: Principal) -> Answer {
    let a = get(db, uri, who).await;
    assert_eq!(a.status, StatusCode::OK, "{uri}: {}", a.json);
    a
}

/// The JSON log lines written while the guard lives.
#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn lines(&self) -> Vec<Value> {
        String::from_utf8(self.0.lock().expect("lock").clone())
            .expect("utf-8")
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

fn capture() -> (Buf, tracing::subscriber::DefaultGuard) {
    let buf = Buf::default();
    let writer = buf.clone(); // clone: the subscriber writes, the test reads
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(move || writer.clone())
        .finish();
    (buf, tracing::subscriber::set_default(subscriber))
}

#[tokio::test]
async fn the_activity_holds_only_shown_events_of_involved_projects() {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    let dave = user(&t, "dave").await;
    let gina = user(&t, "gina").await;
    let hugo = user(&t, "hugo").await;
    let web = project(
        &t,
        "acme",
        olga,
        Visibility::Private,
        &[(dave, Role::Reporter), (gina, Role::Guest)],
    )
    .await;
    // Projects dave is not involved in: a private one, and an internal one
    // he could read but holds no role on.
    let hidden = project(&t, "hushed", hugo, Visibility::Private, &[]).await;
    let open = project(&t, "lobby", hugo, Visibility::Internal, &[]).await;
    let mine = mr(&t, web, 1, olga).await;
    let theirs = mr(&t, hidden, 1, hugo).await;
    events(
        &t,
        web,
        olga,
        &[
            (
                "merge_request.opened",
                format!(r#"{{"merge_request_id": {mine}, "iid": 1, "author_id": 1}}"#),
            ),
            (
                "repository.pushed",
                r#"{"repository_id": 1, "refs": [{"ref": "refs/heads/zz-branch-secret", "kind": "create"}], "total_refs": 1, "refs_truncated": false}"#.to_owned(),
            ),
            // Not shown: an unknown type, an integration-class type the list
            // does not carry, and a maintainer-class one.
            ("zzz.leaked", r#"{"note": "zz-unknown-secret"}"#.to_owned()),
            (
                "merge_request.conflict_predicted",
                format!(r#"{{"merge_request_id": {mine}, "iid": 1}}"#),
            ),
            ("member.role_changed", r#"{"user_id": 1, "role": "owner"}"#.to_owned()),
        ],
    )
    .await;
    events(
        &t,
        hidden,
        hugo,
        &[(
            "merge_request.opened",
            format!(r#"{{"merge_request_id": {theirs}, "iid": 1}}"#),
        )],
    )
    .await;
    events(
        &t,
        open,
        hugo,
        &[("repository.pushed", r#"{"total_refs": 3}"#.to_owned())],
    )
    .await;

    let a = ok(&t.db, ACTIVITY, session(dave)).await;
    // Newest first: dave's project only; its creation (system) included.
    assert_eq!(
        a.types(),
        vec![
            "repository.pushed",
            "merge_request.opened",
            "project.created"
        ],
        "{}",
        a.json
    );
    assert!(a.seqs().windows(2).all(|w| w[0] > w[1]), "{}", a.json);
    assert!(a.types().iter().all(|t| SHOWN_TYPES.contains(t)));
    assert!(a.list().iter().all(|e| e["project_path"] == "acme/web"));
    assert_eq!(a.json["next_before"], Value::Null);
    assert_eq!(a.json["truncated"], false);
    // No other project, no hidden type, no payload passes through.
    for absent in [
        "hushed",
        "lobby",
        "zzz.leaked",
        "zz-unknown-secret",
        "zz-branch-secret",
        "member.",
        "conflict_predicted",
        "refs_truncated",
        "repository_id",
    ] {
        assert!(!a.raw.contains(absent), "{absent}: {}", a.raw);
    }
    let pushed = &a.list()[0];
    assert_eq!(pushed["ref_count"], 1);
    assert_eq!(pushed["merge_request"], Value::Null);
    assert_eq!(pushed["actor"]["username"], "olga");
    let opened = &a.list()[1];
    assert_eq!(
        opened["merge_request"],
        serde_json::json!({"iid": 1, "title": "Change 1"})
    );
    assert_eq!(opened["ref_count"], Value::Null);
    let created = &a.list()[2];
    assert_eq!(created["actor"], Value::Null, "{created}");
    assert_eq!(created["project_id"], web.get());
    // Exactly the allowed fields.
    let mut keys: Vec<&str> = opened
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "actor",
            "merge_request",
            "occurred_at",
            "project_id",
            "project_path",
            "ref_count",
            "seq",
            "type"
        ]
    );

    // hugo is involved in his two projects only.
    let h = ok(&t.db, ACTIVITY, session(hugo)).await;
    assert!(
        h.list()
            .iter()
            .all(|e| e["project_path"] == "hushed/web" || e["project_path"] == "lobby/web"),
        "{}",
        h.json
    );
    assert!(!h.raw.contains("acme"), "{}", h.raw);

    // A Guest holds a membership but reads no events: the project is
    // refused before any event is read (no scan, no page slot), and the
    // refusal is counted in the log with the request id — never named.
    let (logs, guard) = capture();
    let g = ok(&t.db, ACTIVITY, session(gina)).await;
    drop(guard);
    assert!(g.list().is_empty(), "{}", g.json);
    let lines = logs.lines();
    let read: Vec<&Value> = lines
        .iter()
        .filter(|l| l["message"] == "forge.user_activity.read")
        .collect();
    assert_eq!(read.len(), 1, "{lines:?}");
    assert_eq!(read[0]["projects"], 0, "{}", read[0]);
    assert_eq!(read[0]["projects_hidden"], 1, "{}", read[0]);
    assert_eq!(read[0]["rows_scanned"], 0, "{}", read[0]);
    assert_eq!(read[0]["rows_hidden"], 0, "{}", read[0]);
    assert!(read[0]["request_id"]
        .as_str()
        .is_some_and(|r| !r.is_empty()));
    let text = read[0].to_string();
    assert!(!text.contains("acme"), "{text}");
    assert!(!text.contains("merge_request"), "{text}");

    // dave loses the project: nothing of it is listed any more.
    t.commit_audited(
        ("project", web.get()),
        &[format!(
            "DELETE FROM forge.project_members WHERE project_id = {} AND user_id = {}",
            web.get(),
            dave.get()
        )],
    )
    .await;
    let gone = ok(&t.db, ACTIVITY, session(dave)).await;
    assert!(gone.list().is_empty(), "{}", gone.json);
}

#[tokio::test]
async fn pages_chain_by_seq_across_events_of_one_timestamp() {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    let web = project(&t, "acme", olga, Visibility::Private, &[]).await;
    let one = mr(&t, web, 1, olga).await;
    let opened = format!(r#"{{"merge_request_id": {one}, "iid": 1}}"#);
    // Five events of one transaction: one `occurred_at`, five seqs.
    let batch: Vec<(&str, String)> = (0..5)
        .map(|_| ("merge_request.commented", opened.clone()))
        .collect();
    events(&t, web, olga, &batch).await;
    let all = ok(&t.db, ACTIVITY, session(olga)).await;
    let expected = all.seqs();
    // Five comments and the project's creation.
    assert_eq!(expected.len(), 6, "{}", all.json);
    let mut seen = Vec::new();
    let mut uri = format!("{ACTIVITY}?limit=2");
    for _ in 0..5 {
        let page = ok(&t.db, &uri, session(olga)).await;
        seen.extend(page.seqs());
        match page.json["next_before"].as_i64() {
            Some(before) => uri = format!("{ACTIVITY}?limit=2&before={before}"),
            None => break,
        }
    }
    assert_eq!(seen, expected);
    let stamps: Vec<&Value> = all.list()[..5].iter().map(|e| &e["occurred_at"]).collect();
    assert!(stamps.iter().all(|s| *s == stamps[0]), "{}", all.json);
    // `before` is exclusive, even within one timestamp.
    let below = ok(
        &t.db,
        &format!("{ACTIVITY}?before={}", expected[1]),
        session(olga),
    )
    .await;
    assert_eq!(below.seqs(), expected[2..].to_vec());
}

#[tokio::test]
async fn the_query_is_strict_and_only_a_persons_reading_credential_passes() {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    for uri in [
        format!("{ACTIVITY}?limit=0"),
        format!("{ACTIVITY}?limit=101"),
        format!("{ACTIVITY}?before=x"),
        format!("{ACTIVITY}?before=0"),
        format!("{ACTIVITY}?limit=5&limit=5"),
        format!("{ACTIVITY}?scope=authored"),
    ] {
        let a = get(&t.db, &uri, session(olga)).await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::BAD_REQUEST, "FG-WEB-0006"),
            "{uri}: {}",
            a.json
        );
    }
    let read_api = get(&t.db, ACTIVITY, token(olga, &[Scope::ReadApi])).await;
    assert_eq!(read_api.status, StatusCode::OK, "{}", read_api.json);
    let repo_only = get(&t.db, ACTIVITY, token(olga, &[Scope::ReadRepository])).await;
    assert_eq!(
        (repo_only.status, repo_only.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
        "{}",
        repo_only.json
    );
    let anonymous = get(&t.db, ACTIVITY, Principal::anonymous()).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    // People only: an agent's token, a service account and a bot.
    for (who, p) in [
        ("agent", token(olga, &[Scope::ReadApi]).as_agent(true)),
        (
            "service account",
            principal(
                olga,
                Credential::Token(vec![Scope::ReadApi]),
                AccountKind::ServiceAccount,
            ),
        ),
        (
            "bot",
            principal(
                olga,
                Credential::Token(vec![Scope::ReadApi]),
                AccountKind::Bot,
            ),
        ),
    ] {
        let a = get(&t.db, ACTIVITY, p).await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
            "{who}: {}",
            a.json
        );
        assert!(a.json.get("events").is_none(), "{who}");
    }
}

#[tokio::test]
async fn runner_job_and_act_as_credentials_are_refused() {
    let t = FreshDb::migrated().await;
    let olga = users::create(&t.db, &ctx("req-user-olga"), &{
        let mut n = NewUser::human("olga", "olga", "olga@example.test");
        n.is_admin = true;
        n
    })
    .await
    .expect("olga")
    .user_id;
    let web = project(&t, "acme", olga, Visibility::Private, &[]).await;
    // Its `project.created` is the control's row.
    sequence(&t).await;
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

    let own = ok(&t.db, ACTIVITY, session(olga)).await;
    assert!(!own.list().is_empty(), "the control: {}", own.json);
    let runner_auth = format!("Bearer {}", runner.token.reveal());
    let job_auth = format!("Bearer {job}");
    let kanflow_auth = format!("Bearer {kanflow}");
    let act_as = format!("user:{}", olga.get());
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
        // The service account's own token, without acting for anyone.
        (
            "service account",
            vec![("authorization", kanflow_auth.as_str())],
        ),
    ] {
        let a = send(&t.db, ACTIVITY, None, &headers).await;
        assert!(
            a.status == StatusCode::FORBIDDEN || a.status == StatusCode::NOT_FOUND,
            "{who}: {} {}",
            a.status,
            a.json
        );
        assert!(
            a.code().starts_with("FG-AUTH-") || a.code() == "FG-REPO-0005",
            "{who}: {}",
            a.json
        );
        assert!(a.json.get("events").is_none(), "{who}");
    }
}
