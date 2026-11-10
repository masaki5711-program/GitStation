#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! Plan 06 D-5 safety net (ADR-0026 decision of 2026-11-10): an AI agent
//! audience — an `is_agent` token, a service account, a bot — cannot clone,
//! fetch or download an archive of a repository whose default branch's tip
//! has a confidential file; a person can; a measurement that times out or
//! hits a bound refuses. The real `git` client against the product router
//! served on 127.0.0.1:0 (tokens by a scripted authenticator, the project
//! by the spec API's scripted world). Synthetic content; no database.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, CopyMark, Credential, Principal, Scope};
use forge_checks::confidential::ConfidentialLimits;
use forge_core::authenticator::{AuthFuture, TokenIdentity};
use forge_core::{RepoId, RequestId, UserId, UserKind};
use forge_errors::ErrorCode;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::{CopyMarks, Measured, MAX_MARKS};
use crate::app::{build, AppConfig, AppState, Lookups};
use crate::routes::browse::tests::{plumb, Answer};
use crate::routes::git::{GitHttpLimits, SmartHttp};
use crate::routes::specs::tests::{commit, World};

const READER: UserId = UserId::new(5);
const SECRET_TEXT: &str = "the hidden text";
const REPO: RepoId = RepoId::new(70);

/// Tokens of the scripted authenticator: `<kind>-token`.
struct Tokens;

impl forge_core::Authenticator for Tokens {
    fn authenticate<'a>(&'a self, _: &'a RequestId, presented: &'a str) -> AuthFuture<'a> {
        let (kind, is_agent) = match presented {
            "person-token" => (UserKind::Human, false),
            "agent-token" => (UserKind::Human, true),
            "sa-token" => (UserKind::ServiceAccount, false),
            "bot-token" => (UserKind::Bot, false),
            _ => {
                return Box::pin(async {
                    Err(forge_errors::ForgeError::new(ErrorCode::Unauthenticated)
                        .with_context("reason", "unknown"))
                })
            }
        };
        Box::pin(async move {
            Ok(TokenIdentity {
                token_id: 1,
                user_id: READER,
                username: "reader".to_owned(),
                display_name: "Reader".to_owned(),
                is_admin: false,
                scopes: vec![forge_core::Scope::ReadRepository],
                is_agent,
                kind,
            })
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    _home: tempfile::TempDir,
    router: axum::Router,
    smart: Arc<SmartHttp>,
    repo: PathBuf,
    main: String,
}

fn doc(id: &str, extra: &str) -> String {
    format!("---\nid: {id}\ndoctype: note\ntitle: T\n{extra}---\n{SECRET_TEXT}\n")
}

const NOTE_TYPE: (&str, &str) = (".forge/doctypes/note.yml", "name: note\n");
const OLDER_SECRET: &str = "---\nspec: app/secret\ntitle: Secret\nstatus: approved\nconfidential: true\n---\n::: req APP-SEC-1\nthe hidden text\n:::\n";

/// `acme/app` (7, repository 70): `main` with `files` (plus the note type).
async fn fixture_with(files: &[(&str, &str)], limits: Option<ConfidentialLimits>) -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = forge_git::Git::new(forge_git::GitConfig::new("git", home.path()));
    let storage = forge_git::StorageRoot::open(root.path()).expect("root");
    let app = storage
        .init_bare(&git, REPO, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let main = if files.is_empty() {
        String::new()
    } else {
        let mut all = vec![NOTE_TYPE, ("README.md", "# app\n")];
        all.extend_from_slice(files);
        let main = commit(&app, &all, None);
        plumb(&app, &["update-ref", "refs/heads/main", &main], None);
        main
    };
    plumb(&app, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
    let world = Arc::new(World {
        calls: Mutex::new(Vec::new()),
    });
    let lookups = Lookups {
        namespaces: Arc::clone(&world) as Arc<dyn forge_core::NamespaceResolver>,
        projects: Arc::clone(&world) as Arc<dyn forge_core::ProjectAccessLoader>,
    };
    let mut smart = SmartHttp::new(git, storage, GitHttpLimits::default());
    if let Some(limits) = limits {
        smart = smart.with_copy_check_limits(limits);
    }
    let state = AppState::new("test", Vec::new(), Arc::new(Tokens), lookups).with_smart_http(smart);
    let smart = Arc::clone(state.smart_http.as_ref().expect("smart http"));
    let router = build(crate::app::registry(), state, AppConfig::default()).expect("router");
    Fixture {
        _root: root,
        _home: home,
        router,
        smart,
        repo: app.to_path_buf(),
        main,
    }
}

async fn fixture(files: &[(&str, &str)]) -> Fixture {
    fixture_with(files, None).await
}

/// Move `main` to a new commit with `files` (as a push would; the ref
/// cache is dropped the way receive-pack drops it).
fn move_main(f: &Fixture, files: &[(&str, &str)]) -> String {
    let mut all = vec![NOTE_TYPE, ("README.md", "# app\n")];
    all.extend_from_slice(files);
    let parent = plumb(&f.repo, &["rev-parse", "refs/heads/main"], None);
    let next = commit(&f.repo, &all, Some(parent.trim()));
    plumb(&f.repo, &["update-ref", "refs/heads/main", &next], None);
    f.smart.browse.invalidate(REPO);
    next
}

// ── the real git client ────────────────────────────────────────────────

struct Served {
    addr: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    client_home: tempfile::TempDir,
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(()); // ignore: the server may have stopped already
        }
    }
}

async fn serve(f: &Fixture) -> Served {
    let listener = crate::server::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(crate::server::serve(
        listener,
        f.router.clone(),
        crate::ServerLimits::default(),
        async {
            let _ = rx.await; // ignore: a dropped sender also means "stop"
        },
    ));
    Served {
        addr,
        stop: Some(tx),
        client_home: tempfile::tempdir().expect("client home"),
    }
}

impl Served {
    fn url(&self, token: &str) -> String {
        format!("http://reader:{token}@{}/acme/app.git", self.addr)
    }

    /// Run the git client (blocking, off the runtime's workers).
    async fn git(&self, dir: &Path, args: &[&str]) -> Output {
        let home = self.client_home.path().to_path_buf();
        let dir = dir.to_path_buf();
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        tokio::task::spawn_blocking(move || {
            Command::new("git")
                .current_dir(dir)
                .env("HOME", home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("LANG", "C")
                .env("LC_ALL", "C")
                .env_remove("GIT_ASKPASS")
                .env_remove("SSH_ASKPASS")
                .args(args)
                .output()
                .expect("git client")
        })
        .await
        .expect("join")
    }

    async fn clone(&self, token: &str, into: &Path, extra: &[&str]) -> Output {
        let url = self.url(token);
        let target = into.to_string_lossy().into_owned();
        let mut args: Vec<&str> = extra.to_vec();
        args.extend(["clone", "--quiet", url.as_str(), target.as_str()]);
        self.git(Path::new("/"), &args).await
    }
}

fn refused_by_git(out: &Output, what: &str) {
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{what}: must fail");
    assert!(err.contains("remote error"), "{what}: {err}");
    assert!(err.contains("request_id: "), "{what}: {err}");
    // The user message (ja by default; the en one says the same).
    assert!(err.contains("AI"), "{what}: {err}");
}

fn cloned(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── one request through the router ─────────────────────────────────────

fn agent() -> Principal {
    Principal::authenticated(
        READER,
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
        AccountKind::Human,
    )
    .as_agent(true)
}

fn token_of(kind: AccountKind) -> Principal {
    Principal::authenticated(
        READER,
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
        kind,
    )
}

async fn call(f: &Fixture, method: &str, uri: &str, who: Principal, body: &[u8]) -> Answer {
    let mut req = Request::builder().method(method).uri(uri);
    if method == "POST" {
        req = req.header("content-type", "application/x-git-upload-pack-request");
    }
    let mut req = req.body(Body::from(body.to_vec())).expect("request");
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

const INFO_REFS: &str = "/acme/app.git/info/refs?service=git-upload-pack";
const UPLOAD_PACK: &str = "/acme/app.git/git-upload-pack";
const ARCHIVE: &str = "/acme/app/-/archive/main.tar.gz";

/// The in-band refusal: 200, one `ERR` packet with the message and the
/// request id, nothing of the repository.
fn err_packet(a: &Answer, what: &str) {
    assert_eq!(a.status, StatusCode::OK, "{what}");
    let text = String::from_utf8_lossy(&a.bytes);
    assert!(text.get(4..8) == Some("ERR "), "{what}: {text}");
    assert!(text.contains("request_id: "), "{what}: {text}");
    assert!(!text.contains("refs/heads"), "{what}: {text}");
}

fn archive_refused(a: &Answer, what: &str) {
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{what}: {:?}", a.json());
    assert_eq!(a.code(), "FG-AUTH-0022", "{what}");
    assert!(a.json()["request_id"].is_string(), "{what}");
}

// ── the tests ──────────────────────────────────────────────────────────

/// The error catalog's test for `FG-AUTH-0022`: clone (protocol v2 and
/// v0), fetch and archive, each mark, each kind of agent; a person
/// throughout; the answer follows the tip (the cache is keyed by it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_cannot_clone_fetch_or_archive_a_repository_with_a_confidential_document() {
    let classified = doc("NT-1", "classification: confidential\n");
    let f = fixture(&[("docs/NT-1.md", &classified)]).await;
    let s = serve(&f).await;
    let work = tempfile::tempdir().expect("work");
    let at = |name: &str| work.path().join(name);

    // `classification: confidential` at the tip.
    for token in ["agent-token", "sa-token", "bot-token"] {
        let out = s.clone(token, &at(token), &[]).await;
        refused_by_git(&out, token);
        let v0 = s
            .clone(
                token,
                &at(&format!("{token}-v0")),
                &["-c", "protocol.version=0"],
            )
            .await;
        refused_by_git(&v0, "protocol v0");
    }
    for who in [
        agent(),
        token_of(AccountKind::ServiceAccount),
        token_of(AccountKind::Bot),
    ] {
        archive_refused(&call(&f, "GET", ARCHIVE, who, b"").await, "archive");
    }
    cloned(
        &s.clone("person-token", &at("person"), &[]).await,
        "a person",
    );
    let person_archive = call(&f, "GET", ARCHIVE, token_of(AccountKind::Human), b"").await;
    assert_eq!(person_archive.status, StatusCode::OK);
    assert_eq!(
        f.smart.copy_marks.len(),
        1,
        "the confidential answer is kept"
    );

    // The older mark (`confidential: true` of a specification document).
    move_main(&f, &[("spec/secret.md", OLDER_SECRET)]);
    refused_by_git(
        &s.clone("agent-token", &at("older"), &[]).await,
        "older mark",
    );
    archive_refused(&call(&f, "GET", ARCHIVE, agent(), b"").await, "older mark");

    // Removed from the tip: the new tip is measured, the agent clones.
    move_main(&f, &[("docs/open.md", "# open notes\n")]);
    cloned(
        &s.clone("agent-token", &at("agent-open"), &[]).await,
        "open tip",
    );
    let open_archive = call(&f, "GET", ARCHIVE, agent(), b"").await;
    assert_eq!(open_archive.status, StatusCode::OK, "open tip archive");
    assert_eq!(f.smart.copy_marks.len(), 1, "one tip per repository");

    // Marked again: the agent's fetch is refused, the person's is not.
    move_main(&f, &[("docs/NT-1.md", &classified)]);
    let fetch = s
        .git(&at("agent-open"), &["fetch", "--quiet", "origin"])
        .await;
    refused_by_git(&fetch, "fetch");
    cloned(
        &s.git(&at("person"), &["fetch", "--quiet", "origin"]).await,
        "a person's fetch",
    );
}

/// The advertisement and the RPC answer the refusal in-band (git's `ERR`
/// line, as an unreachable want), the archive with the JSON 403; an
/// internal document leaves the repository open; push is not a copy.
#[tokio::test]
async fn the_refusal_is_a_git_error_line_and_an_open_repository_is_served() {
    let classified = doc("NT-1", "classification: confidential\n");
    let f = fixture(&[("docs/NT-1.md", &classified)]).await;
    err_packet(&call(&f, "GET", INFO_REFS, agent(), b"").await, "info/refs");
    err_packet(
        &call(&f, "POST", UPLOAD_PACK, agent(), b"0000").await,
        "upload-pack",
    );
    let head = call(&f, "HEAD", INFO_REFS, agent(), b"").await;
    assert_eq!(head.status, StatusCode::OK, "HEAD starts no git");
    let person = call(&f, "GET", INFO_REFS, token_of(AccountKind::Human), b"").await;
    assert_eq!(person.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&person.bytes).contains("refs/heads/main"));

    let internal = doc("NT-2", "classification: internal\n");
    let open = fixture(&[("docs/NT-2.md", &internal)]).await;
    let served = call(&open, "GET", INFO_REFS, agent(), b"").await;
    assert_eq!(served.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&served.bytes).contains("refs/heads/main"));
    assert_eq!(
        call(&open, "GET", ARCHIVE, agent(), b"").await.status,
        StatusCode::OK
    );
}

/// The measurement's timeout and its bounds refuse (fail closed) and are
/// never kept; an unborn default branch with other refs refuses; a
/// repository with no ref at all has nothing to copy.
#[tokio::test]
async fn a_timeout_a_bound_or_an_unborn_default_branch_refuses_the_agent() {
    let classified = doc("NT-1", "classification: confidential\n");
    let internal = doc("NT-2", "classification: internal\n");
    let zero = ConfidentialLimits {
        timeout: Duration::ZERO,
        ..ConfidentialLimits::default()
    };
    let timed = fixture_with(&[("docs/NT-2.md", &internal)], Some(zero)).await;
    archive_refused(&call(&timed, "GET", ARCHIVE, agent(), b"").await, "timeout");
    err_packet(
        &call(&timed, "GET", INFO_REFS, agent(), b"").await,
        "timeout",
    );
    assert_eq!(timed.smart.copy_marks.len(), 0, "a failure is never kept");
    assert_eq!(
        call(&timed, "GET", ARCHIVE, token_of(AccountKind::Human), b"")
            .await
            .status,
        StatusCode::OK,
        "a person is not measured"
    );

    let mut tiny = ConfidentialLimits::default();
    tiny.doc.max_files = 0;
    let bounded = fixture_with(&[("docs/NT-2.md", &internal)], Some(tiny)).await;
    archive_refused(&call(&bounded, "GET", ARCHIVE, agent(), b"").await, "bound");
    assert_eq!(bounded.smart.copy_marks.len(), 0);

    let unborn = fixture(&[("docs/NT-1.md", &classified)]).await;
    plumb(
        &unborn.repo,
        &["symbolic-ref", "HEAD", "refs/heads/unborn"],
        None,
    );
    unborn.smart.browse.invalidate(REPO);
    err_packet(
        &call(&unborn, "GET", INFO_REFS, agent(), b"").await,
        "unborn",
    );
    let m = super::measure(
        &unborn.smart,
        REPO,
        "user:5",
        &ConfidentialLimits::default(),
    )
    .await;
    assert_eq!(m.mark, CopyMark::Unborn);
    assert!(!unborn.main.is_empty());

    let empty = fixture(&[]).await;
    let m = super::measure(&empty.smart, REPO, "user:5", &ConfidentialLimits::default()).await;
    assert_eq!(m.mark, CopyMark::Open, "nothing to copy");
}

#[test]
fn the_cache_keeps_decided_answers_only_one_tip_per_repository_and_is_bounded() {
    let marks = CopyMarks::default();
    let decided = |mark, tip: &str| Measured {
        mark,
        tip: Some(tip.to_owned()),
        paths: Vec::new(),
        error: None,
    };
    marks.keep(REPO, &decided(CopyMark::Undecided, "a"));
    marks.keep(REPO, &Measured::plain(CopyMark::Open));
    assert_eq!(
        marks.len(),
        0,
        "neither a failure nor an answer without a tip"
    );
    marks.keep(REPO, &decided(CopyMark::Confidential, "a"));
    assert_eq!(
        marks.get(REPO, "a").map(|m| m.mark),
        Some(CopyMark::Confidential)
    );
    marks.keep(REPO, &decided(CopyMark::Open, "b"));
    assert!(marks.get(REPO, "a").is_none(), "the older tip is gone");
    assert_eq!(marks.get(REPO, "b").map(|m| m.mark), Some(CopyMark::Open));
    for i in 0..(MAX_MARKS as i64 + 10) {
        marks.keep(RepoId::new(1000 + i), &decided(CopyMark::Open, "t"));
    }
    assert_eq!(marks.len(), MAX_MARKS);
}
