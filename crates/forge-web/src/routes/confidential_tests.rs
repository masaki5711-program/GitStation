#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! Plan 06 D-5 / ADR-0026 §6 on the repository read routes: an agent
//! audience is given no file of a plan 13 document that is `confidential`
//! by `classification`, by its type's default, by both keys written, or by
//! a front matter that cannot be read — blob, raw and blame refuse it
//! (`FG-AUTH-0021`), compare and a commit leave it out — while a person
//! reads them all and an `internal` document stays open to the agent
//! ([`crate::routes::context::marked_files`]). Synthetic content; no
//! database (the spec API's scripted world).

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
const SECRET_TEXT: &str = "the hidden text";

/// Confidential by each rule (path, why).
const CONFIDENTIAL: &[&str] = &[
    "docs/NT-1.md", // classification: confidential
    "docs/HR-1.md", // the type's default (confidential)
    "docs/NT-2.md", // both keys written
    "docs/NT-3.md", // a front matter that cannot be read
    "docs/XX-1.md", // a type with no definition
];
const OPEN: &str = "docs/NT-4.md"; // classification: internal

struct Fixture {
    _root: tempfile::TempDir,
    _home: tempfile::TempDir,
    router: axum::Router,
    repo: std::path::PathBuf,
    main: String,
}

fn doc(id: &str, ty: &str, extra: &str) -> String {
    format!("---\nid: {id}\ndoctype: {ty}\ntitle: T\n{extra}---\n{SECRET_TEXT}\n")
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let app = storage
        .init_bare(&git, RepoId::new(70), uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let base = commit(&app, &[("README.md", "# app\n")], None);
    let nt1 = doc("NT-1", "note", "classification: confidential\n");
    let hr1 = doc("HR-1", "hr", "");
    let nt2 = doc(
        "NT-2",
        "note",
        "classification: internal\nconfidential: false\n",
    );
    let nt3 = format!("---\nid: NT-3\ndoctype: [note\n---\n{SECRET_TEXT}\n");
    let xx1 = doc("XX-1", "missing", "");
    let nt4 = "---\nid: NT-4\ndoctype: note\ntitle: T\nclassification: internal\n---\nopen text\n";
    let main = commit(
        &app,
        &[
            ("README.md", "# app\n"),
            (".forge/doctypes/note.yml", "name: note\n"),
            (
                ".forge/doctypes/hr.yml",
                "name: hr\ndefaults: { classification: confidential }\n",
            ),
            ("docs/NT-1.md", &nt1),
            ("docs/HR-1.md", &hr1),
            ("docs/NT-2.md", &nt2),
            ("docs/NT-3.md", &nt3),
            ("docs/XX-1.md", &xx1),
            ("docs/NT-4.md", nt4),
        ],
        Some(&base),
    );
    plumb(&app, &["update-ref", "refs/heads/main", &main], None);
    plumb(&app, &["update-ref", "refs/tags/base", &base], None);
    plumb(&app, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
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
        repo: app.to_path_buf(),
        main,
    }
}

fn person() -> Principal {
    Principal::authenticated(READER, false, true, Credential::Session, AccountKind::Human)
}

fn agent() -> Principal {
    Principal::authenticated(
        READER,
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
        AccountKind::Human,
    )
    .as_agent(true)
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

fn refused(a: &Answer, what: &str) {
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{what}: {:?}", a.json());
    assert_eq!(a.code(), "FG-AUTH-0021", "{what}");
    assert!(
        !String::from_utf8_lossy(&a.bytes).contains(SECRET_TEXT),
        "{what}"
    );
}

fn served(a: &Answer, what: &str) {
    assert_eq!(a.status, StatusCode::OK, "{what}: {:?}", a.json());
}

#[tokio::test]
async fn blob_raw_and_blame_refuse_an_agent_every_confidential_document() {
    let f = fixture().await;
    for path in CONFIDENTIAL {
        let blob = format!("/api/v1/projects/7/repository/blob?ref=main&path={path}");
        let blame = format!("/api/v1/projects/7/repository/blame?ref=main&path={path}");
        // The web raw route takes a session: the agent is the header's.
        let raw = format!("/acme/app/-/raw/main/{path}");
        refused(&get_with(&f, &blob, agent(), None).await, &blob);
        refused(&get_with(&f, &blame, agent(), None).await, &blame);
        refused(&get_with(&f, &raw, person(), Some("agent")).await, &raw);
        served(&get_with(&f, &blob, person(), None).await, &blob);
        served(&get_with(&f, &blame, person(), None).await, &blame);
        let person_raw = get_with(&f, &raw, person(), None).await;
        served(&person_raw, &raw);
        assert!(String::from_utf8_lossy(&person_raw.bytes).contains(SECRET_TEXT));
    }
    // An internal document (and a file that is no document) stays open.
    for path in [OPEN, "README.md"] {
        let blob = format!("/api/v1/projects/7/repository/blob?ref=main&path={path}");
        served(&get_with(&f, &blob, agent(), None).await, &blob);
        let raw = format!("/acme/app/-/raw/main/{path}");
        served(&get_with(&f, &raw, person(), Some("agent")).await, &raw);
    }
}

fn paths(body: &Value) -> Vec<String> {
    body["diff"]["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|d| d["new_path"].as_str().unwrap_or("").to_owned())
        .collect()
}

#[tokio::test]
async fn compare_and_a_commit_leave_every_confidential_document_out_for_an_agent() {
    let f = fixture().await;
    let compare = "/api/v1/projects/7/repository/compare?from=base&to=main";
    let commit = format!("/api/v1/projects/7/repository/commits/{}", f.main);
    for uri in [compare, commit.as_str()] {
        let all = get_with(&f, uri, person(), None).await;
        served(&all, uri);
        let all = all.json();
        for path in CONFIDENTIAL {
            assert!(paths(&all).contains(&(*path).to_owned()), "{uri} {path}");
        }
        let mine = get_with(&f, uri, agent(), None).await;
        served(&mine, uri);
        let mine = mine.json();
        for path in CONFIDENTIAL {
            assert!(!paths(&mine).contains(&(*path).to_owned()), "{uri} {path}");
        }
        assert!(paths(&mine).contains(&OPEN.to_owned()), "{uri}");
        assert_eq!(
            mine["diff"]["withheld"]["files"],
            CONFIDENTIAL.len(),
            "{uri}"
        );
        assert!(!mine.to_string().contains(SECRET_TEXT), "{uri}");
    }
}

/// No default branch born: nothing to measure a revision against, so an
/// agent is given no document (fail closed, as the document API); other
/// files stay readable, and a person reads everything.
#[tokio::test]
async fn without_a_default_tip_an_agent_is_given_no_document_file() {
    let f = fixture().await;
    let blob = |path: &str| format!("/api/v1/projects/7/repository/blob?ref=main&path={path}");
    // HEAD → an unborn branch.
    plumb(
        &f.repo,
        &["symbolic-ref", "HEAD", "refs/heads/unborn"],
        None,
    );
    for path in [OPEN, "README.md"] {
        refused(&get_with(&f, &blob(path), agent(), None).await, path);
        served(&get_with(&f, &blob(path), person(), None).await, path);
    }
    served(
        &get_with(&f, &blob(".forge/doctypes/note.yml"), agent(), None).await,
        "a file that is no document",
    );
}
