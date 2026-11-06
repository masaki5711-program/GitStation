#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! The context API's requirement route and the confidential boundary of
//! plan 06 D-5 on the spec API (M3 X5, plan 03 §6, ADR-0023 Amendment X5),
//! end to end through the product router: the spec API's scripted
//! resolver / project loader (no database) and a real bare repository
//! (synthetic content).

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::{RepoId, UserId};
use forge_git::browse::BrowseLimits;
use forge_git::{Git, GitConfig, StorageRoot};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::app::{build, AppConfig, AppState, Lookups};
use crate::routes::browse::tests::{plumb, Answer};
use crate::routes::git::{GitHttpLimits, SmartHttp};
use crate::routes::specs::tests::{commit, World};

const READER: UserId = UserId::new(5);

const OPEN: &str = "---\nspec: app/core\ntitle: Core\nstatus: approved\n---\n";
const SECRET: &str =
    "---\nspec: app/secret\ntitle: Secret\nstatus: approved\nconfidential: true\n---\n";

struct Fixture {
    _root: tempfile::TempDir,
    _home: tempfile::TempDir,
    router: axum::Router,
    v1: String,
    main: String,
}

/// `acme/app` (7, repository 70): `v1.0` with APP-CORE-1 "first"; `main`
/// (= `v1.1`) with APP-CORE-1 changed, APP-CORE-2, the confidential
/// APP-SEC-1, and tests naming APP-CORE-1 and APP-SEC-1.
async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let app = storage
        .init_bare(&git, RepoId::new(70), uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let one = format!("{OPEN}::: req APP-CORE-1\nfirst\n:::\n");
    let v1 = commit(&app, &[("spec/core.md", &one)], None);
    plumb(&app, &["update-ref", "refs/tags/v1.0", &v1], None);
    let two = format!(
        "{OPEN}::: req APP-CORE-1 priority=must\nfirst, changed\n:::\n\n::: req APP-CORE-2\nsecond\n:::\n"
    );
    let secret = format!("{SECRET}::: req APP-SEC-1\nthe secret rule\n:::\n");
    let main = commit(
        &app,
        &[
            ("spec/core.md", &two),
            ("spec/secret.md", &secret),
            ("tests/t.rs", "// @req APP-CORE-1\n// @req APP-SEC-1\n"),
        ],
        Some(&v1),
    );
    plumb(&app, &["update-ref", "refs/heads/main", &main], None);
    plumb(&app, &["update-ref", "refs/tags/v1.1", &main], None);
    plumb(&app, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
    // A branch whose copy of the secret document carries no mark (review
    // X5 MEDIUM a: an agent picks the revision; the default tip marks it).
    let unmarked_doc = "---\nspec: app/secret\ntitle: Secret\nstatus: approved\n---\n::: req APP-SEC-1\nthe secret rule\n:::\n";
    let unmarked = commit(
        &app,
        &[("spec/core.md", &two), ("spec/secret.md", unmarked_doc)],
        Some(&main),
    );
    plumb(
        &app,
        &["update-ref", "refs/heads/unmarked", &unmarked],
        None,
    );
    let world = Arc::new(World {
        calls: Mutex::new(Vec::new()),
    });
    let lookups = Lookups {
        namespaces: Arc::clone(&world) as Arc<dyn forge_core::NamespaceResolver>,
        projects: Arc::clone(&world) as Arc<dyn forge_core::ProjectAccessLoader>,
    };
    let smart = SmartHttp::new(git, storage, GitHttpLimits::default())
        .with_browse_limits(BrowseLimits::default());
    let state = AppState::new("test", Vec::new(), crate::tests::no_tokens(), lookups)
        .with_smart_http(smart);
    let router = build(crate::app::registry(), state, AppConfig::default()).expect("router");
    Fixture {
        _root: root,
        _home: home,
        router,
        v1,
        main,
    }
}

fn person() -> Principal {
    Principal::authenticated(READER, false, true, Credential::Session, AccountKind::Human)
}

fn token(kind: AccountKind) -> Principal {
    Principal::authenticated(
        READER,
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
        kind,
    )
}

fn agent() -> Principal {
    token(AccountKind::Human).as_agent(true)
}

async fn get_with(f: &Fixture, uri: &str, who: Principal, header: Option<&str>) -> Answer {
    let mut req = Request::builder().method("GET").uri(uri);
    if let Some(value) = header {
        req = req.header("forge-context-audience", value);
    }
    let mut req = req.body(Body::empty()).expect("request");
    req.extensions_mut().insert(who);
    let res = f.router.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    Answer {
        status,
        headers,
        bytes,
    }
}

async fn get(f: &Fixture, uri: &str, who: Principal) -> Answer {
    get_with(f, uri, who, None).await
}

fn ok_body(a: &Answer) -> Value {
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    assert_eq!(a.header("cache-control"), "private, no-store");
    a.json()
}

fn ids(list: &Value, key: &str) -> Vec<String> {
    list.as_array()
        .expect("a list")
        .iter()
        .map(|r| r[key].as_str().expect("id").to_owned())
        .collect()
}

const REQ: &str = "/api/v1/projects/7/context/requirement";

#[tokio::test]
async fn requirement_context_carries_text_tests_and_history_with_provenance() {
    let f = fixture().await;
    let body = ok_body(&get(&f, &format!("{REQ}?id=APP-CORE-1"), person()).await);
    assert_eq!(body["ref"]["name"], "main");
    assert_eq!(body["ref"]["commit"], f.main.as_str());
    let r = &body["requirement"];
    assert_eq!(r["id"], "APP-CORE-1");
    assert_eq!(r["body"], "first, changed");
    assert_eq!(r["priority"], "must");
    assert_eq!(r["deprecated"], false);
    let source = &r["source"];
    assert_eq!(source["project_id"], 7);
    assert_eq!(source["project"], "acme/app");
    assert_eq!(source["version"], "main");
    assert_eq!(source["commit"], f.main.as_str());
    assert_eq!(source["path"], "spec/core.md");
    assert_eq!(source["line"], 6);
    assert_eq!(body["document"]["spec"], "app/core");
    assert_eq!(body["document"]["confidential"], false);
    assert_eq!(body["test_count"], 1);
    assert_eq!(body["tests"][0]["source"]["path"], "tests/t.rs");
    assert_eq!(body["tests"][0]["source"]["line"], 1);
    assert_eq!(body["tests"][0]["source"]["commit"], f.main.as_str());
    // Newest version first; each with where it was then.
    assert_eq!(ids(&body["versions"], "name"), ["v1.1", "v1.0"]);
    assert_eq!(body["versions"][0]["status"], "same");
    assert_eq!(body["versions"][1]["status"], "changed");
    assert_eq!(body["versions"][1]["source"]["commit"], f.v1.as_str());
    assert_eq!(body["versions"][1]["source"]["line"], 6);
    assert_eq!(body["versions_truncated"], false);
    assert_eq!(body["incomplete"], Value::Null);
    // At a version: there it is the first text; APP-CORE-2 is absent then.
    let old = ok_body(&get(&f, &format!("{REQ}?id=APP-CORE-1&ref=v1.0"), person()).await);
    assert_eq!(old["requirement"]["body"], "first");
    assert_eq!(old["requirement"]["source"]["version"], "v1.0");
    let two = ok_body(&get(&f, &format!("{REQ}?id=APP-CORE-2"), person()).await);
    assert_eq!(two["versions"][1]["status"], "absent");
    assert_eq!(two["versions"][1]["source"], Value::Null);
    assert_eq!(two["test_count"], 0);
}

#[tokio::test]
async fn an_unknown_requirement_is_not_found() {
    let f = fixture().await;
    let missing = get(&f, &format!("{REQ}?id=APP-CORE-9"), person()).await;
    assert_eq!(
        missing.status,
        StatusCode::NOT_FOUND,
        "{:?}",
        missing.json()
    );
    assert_eq!(missing.code(), "FG-REPO-0032");
    // Defined later, not at the version asked for.
    let later = get(&f, &format!("{REQ}?id=APP-CORE-2&ref=v1.0"), person()).await;
    assert_eq!(later.code(), "FG-REPO-0032");
    for bad in [
        "",
        "?id=",
        "?id=app-core-1",
        "?id=PBI-1",
        "?id=APP-CORE-1&x=1",
    ] {
        let a = get(&f, &format!("{REQ}{bad}"), person()).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{bad}: {:?}", a.json());
        assert_eq!(a.code(), "FG-WEB-0006", "{bad}");
    }
}

/// Plan 06 D-5: a confidential document's requirement is never handed to
/// an AI agent — a token issued with `is_agent`, a service account, a bot,
/// or a request that says so — while a person reads it.
#[tokio::test]
async fn a_confidential_item_is_withheld_from_an_agent() {
    let f = fixture().await;
    let uri = format!("{REQ}?id=APP-SEC-1");
    let read = ok_body(&get(&f, &uri, person()).await);
    assert_eq!(read["requirement"]["body"], "the secret rule");
    assert_eq!(read["document"]["confidential"], true);
    ok_body(&get(&f, &uri, token(AccountKind::Human)).await);
    for (who, header) in [
        (agent(), None),
        (token(AccountKind::ServiceAccount), None),
        (token(AccountKind::Bot), None),
        (person(), Some("agent")),
    ] {
        let a = get_with(&f, &uri, who, header).await;
        assert_eq!(a.status, StatusCode::FORBIDDEN, "{:?}", a.json());
        assert_eq!(a.code(), "FG-AUTH-0021");
        assert!(!String::from_utf8_lossy(&a.bytes).contains("secret rule"));
    }
    // An open requirement is still given to an agent.
    ok_body(&get(&f, &format!("{REQ}?id=APP-CORE-1"), agent()).await);
    // Only `agent` is a known audience.
    let bad = get_with(&f, &uri, person(), Some("robot")).await;
    assert_eq!(bad.code(), "FG-WEB-0006");
}

/// Plan 06 D-5 on the spec API (the MCP's search and trace use it): an
/// agent's lists leave the confidential documents out and count them.
#[tokio::test]
async fn the_spec_api_leaves_confidential_documents_out_for_an_agent() {
    let f = fixture().await;
    let api = "/api/v1/projects/7/specs";
    let all = ok_body(&get(&f, &format!("{api}/requirements"), person()).await);
    assert_eq!(
        ids(&all["requirements"], "id"),
        ["APP-CORE-1", "APP-CORE-2", "APP-SEC-1"]
    );
    assert_eq!(all["withheld"], Value::Null);
    let secret_doc = all["documents"]
        .as_array()
        .expect("documents")
        .iter()
        .find(|d| d["path"] == "spec/secret.md")
        .expect("listed for a person")
        .clone();
    assert_eq!(secret_doc["confidential"], true);
    let mine = ok_body(&get(&f, &format!("{api}/requirements"), agent()).await);
    assert_eq!(
        ids(&mine["requirements"], "id"),
        ["APP-CORE-1", "APP-CORE-2"]
    );
    assert_eq!(ids(&mine["documents"], "path"), ["spec/core.md"]);
    assert_eq!(mine["withheld"]["error_code"], "FG-AUTH-0021");
    assert_eq!(mine["withheld"]["documents"], 1);
    assert_eq!(mine["withheld"]["requirements"], 1);
    let found = ok_body(&get(&f, &format!("{api}/requirements?q=secret"), agent()).await);
    assert_eq!(found["requirements"], serde_json::json!([]));
    let trace = ok_body(&get(&f, &format!("{api}/trace"), agent()).await);
    assert_eq!(
        ids(&trace["requirements"], "id"),
        ["APP-CORE-1", "APP-CORE-2"]
    );
    assert_eq!(trace["withheld"]["requirements"], 1);
    let person_trace = ok_body(&get(&f, &format!("{api}/trace"), person()).await);
    assert_eq!(
        person_trace["requirements"].as_array().expect("rows").len(),
        3
    );
    let diff = ok_body(
        &get_with(
            &f,
            &format!("{api}/diff?from=v1.0&to=v1.1"),
            person(),
            Some("agent"),
        )
        .await,
    );
    assert_eq!(ids(&diff["changes"], "id"), ["APP-CORE-1", "APP-CORE-2"]);
    assert_eq!(diff["withheld"]["requirements"], 1);
}

/// Review X5 (security HIGH-2): the repository read routes give an agent
/// audience no content of a confidential specification document — blob,
/// raw and blame refuse it (FG-AUTH-0021), a compare leaves the file out
/// and counts it — while a person reads it all.
#[tokio::test]
async fn the_read_routes_give_an_agent_no_confidential_spec_content() {
    let f = fixture().await;
    let blob = "/api/v1/projects/7/repository/blob?ref=main&path=spec/secret.md";
    ok_body(&get(&f, blob, person()).await);
    let refused = get(&f, blob, agent()).await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "{:?}",
        refused.json()
    );
    assert_eq!(refused.code(), "FG-AUTH-0021");
    assert!(!String::from_utf8_lossy(&refused.bytes).contains("secret rule"));
    // An open document is still read.
    ok_body(
        &get(
            &f,
            "/api/v1/projects/7/repository/blob?ref=main&path=spec/core.md",
            agent(),
        )
        .await,
    );
    // The same refusal through the header.
    let header = get_with(&f, blob, person(), Some("agent")).await;
    assert_eq!(header.code(), "FG-AUTH-0021");
    // Raw (the web route) and blame.
    let raw = "/acme/app/-/raw/main/spec/secret.md";
    let person_raw = get(&f, raw, person()).await;
    assert_eq!(person_raw.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&person_raw.bytes).contains("secret rule"));
    // The web route takes a session (tokens are refused): the header.
    let agent_raw = get_with(&f, raw, person(), Some("agent")).await;
    assert_eq!(agent_raw.status, StatusCode::FORBIDDEN);
    assert_eq!(agent_raw.code(), "FG-AUTH-0021");
    assert!(!String::from_utf8_lossy(&agent_raw.bytes).contains("secret rule"));
    let blame = get(
        &f,
        "/api/v1/projects/7/repository/blame?ref=main&path=spec/secret.md",
        agent(),
    )
    .await;
    assert_eq!(blame.code(), "FG-AUTH-0021");
    // A compare leaves the file out for an agent and counts it.
    let compare = "/api/v1/projects/7/repository/compare?from=v1.0&to=v1.1";
    let all = ok_body(&get(&f, compare, person()).await);
    let paths = |b: &Value| -> Vec<String> {
        b["diff"]["files"]
            .as_array()
            .expect("files")
            .iter()
            .map(|d| d["new_path"].as_str().unwrap_or("").to_owned())
            .collect()
    };
    assert!(paths(&all).contains(&"spec/secret.md".to_owned()));
    assert!(all["diff"].get("withheld").is_none_or(Value::is_null));
    let mine = ok_body(&get(&f, compare, agent()).await);
    assert!(!paths(&mine).contains(&"spec/secret.md".to_owned()));
    assert!(paths(&mine).contains(&"spec/core.md".to_owned()));
    assert_eq!(mine["diff"]["withheld"]["files"], 1);
    assert_eq!(mine["diff"]["withheld"]["error_code"], "FG-AUTH-0021");
    assert!(!mine.to_string().contains("secret rule"));
}

/// Review X5 (MEDIUM a): an agent cannot pick a revision where the mark is
/// missing: a path marked at the default branch tip is confidential at
/// every revision for it.
#[tokio::test]
async fn a_path_marked_at_the_default_tip_is_withheld_at_any_revision() {
    let f = fixture().await;
    let req = format!("{REQ}?id=APP-SEC-1&ref=unmarked");
    ok_body(&get(&f, &req, person()).await);
    assert_eq!(get(&f, &req, agent()).await.code(), "FG-AUTH-0021");
    let blob = "/api/v1/projects/7/repository/blob?ref=unmarked&path=spec/secret.md";
    assert_eq!(get(&f, blob, agent()).await.code(), "FG-AUTH-0021");
    let list = ok_body(
        &get(
            &f,
            "/api/v1/projects/7/specs/requirements?ref=unmarked",
            agent(),
        )
        .await,
    );
    assert_eq!(
        ids(&list["requirements"], "id"),
        ["APP-CORE-1", "APP-CORE-2"]
    );
    assert_eq!(list["withheld"]["documents"], 1);
}

/// Review X5 (f): the audience header at most once, and a caller without
/// `read_api` reads no context.
#[tokio::test]
async fn the_audience_header_is_strict_and_read_api_is_needed() {
    let f = fixture().await;
    let uri = format!("{REQ}?id=APP-CORE-1");
    let mut req = Request::builder()
        .method("GET")
        .uri(uri.as_str())
        .header("forge-context-audience", "agent")
        .header("forge-context-audience", "agent")
        .body(Body::empty())
        .expect("request");
    req.extensions_mut().insert(person());
    let res = f.router.clone().oneshot(req).await.expect("infallible");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["error_code"], "FG-WEB-0006");
    let git_only = Principal::authenticated(
        READER,
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
        AccountKind::Human,
    );
    let refused = get(&f, &uri, git_only).await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "{:?}",
        refused.json()
    );
    assert_eq!(
        refused.code(),
        forge_errors::ErrorCode::InsufficientScope.id()
    );
}
