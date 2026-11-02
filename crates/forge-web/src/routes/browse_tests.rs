#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! The browse API end to end through the product router: a scripted
//! project loader / resolver (no database) and a real bare repository
//! (synthetic content). The fixture is shared with `content_tests.rs`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use forge_auth::{Credential, Principal, Scope};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    LookupPath, NamespaceId, NamespacePath, ProjectAccess, ProjectId, ProjectRecord,
    ProjectRepository, RepoId, RepositoryState, Resolution, Resolved, ResolvedTarget, Role, UserId,
    Visibility,
};
use forge_errors::ErrorCode;
use forge_git::browse::BrowseLimits;
use forge_git::{Git, GitConfig, StorageRoot};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::app::{build, AppConfig, AppState, Lookups};
use crate::routes::git::{GitHttpLimits, SmartHttp};

pub(crate) const PROJECT: i64 = 7;
pub(crate) const REPO: i64 = 70;

/// Who the caller is to project 7 (`acme/app`).
#[derive(Clone, Copy)]
pub(crate) struct Facts {
    pub(crate) role: Option<Role>,
    pub(crate) visibility: Visibility,
    pub(crate) state: RepositoryState,
    pub(crate) deleted: bool,
}

impl Default for Facts {
    fn default() -> Self {
        Self {
            role: Some(Role::Reporter),
            visibility: Visibility::Private,
            state: RepositoryState::Ready,
            deleted: false,
        }
    }
}

/// Project 7 at `acme/app` with the scripted facts; nothing else exists.
struct OneProject(Facts);

impl OneProject {
    fn access(&self) -> ProjectAccess {
        ProjectAccess {
            visibility: self.0.visibility,
            role: self.0.role,
            pending_deletion: self.0.deleted,
        }
    }

    fn repository(&self) -> ProjectRepository {
        ProjectRepository {
            id: RepoId::new(REPO),
            state: self.0.state,
        }
    }
}

impl forge_core::NamespaceResolver for OneProject {
    fn resolve<'a>(
        &'a self,
        path: &'a LookupPath,
        _viewer: Option<UserId>,
    ) -> LookupFuture<'a, Resolution> {
        let answer = if path.full_path() == "acme/app" {
            Resolution::Live(Resolved {
                namespace_id: NamespaceId::new(71),
                target: ResolvedTarget::Project {
                    id: ProjectId::new(PROJECT),
                    access: self.access(),
                    repository: self.repository(),
                },
            })
        } else {
            Resolution::Absent
        };
        Box::pin(async move { Ok(answer) })
    }
}

impl forge_core::ProjectAccessLoader for OneProject {
    fn load_project<'a>(
        &'a self,
        project: ProjectId,
        _viewer: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>> {
        let answer = (project.get() == PROJECT).then(|| ProjectRecord {
            id: project,
            access: self.access(),
            namespace_id: NamespaceId::new(71),
            path: NamespacePath::parse("acme/app").expect("path"),
            display_name: "App".to_owned(),
            description: "synthetic".to_owned(),
            created_at: "2026-11-01T00:00:00Z".to_owned(),
            repository: self.repository(),
        });
        Box::pin(async move { Ok(answer) })
    }
}

/// Plumbing on the bare repository (test setup only; synthetic content).
pub(crate) fn plumb(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

pub(crate) fn blob(repo: &Path, content: &[u8]) -> String {
    plumb(repo, &["hash-object", "-w", "--stdin"], Some(content))
}

pub(crate) const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR synthetic";
pub(crate) const SVG: &[u8] =
    b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>\n";

/// The repository: `main` (HEAD) and `feature/x` on one commit, tag `v1`.
/// Files: `README.md`, `src/lib.rs`, `bin.dat` (NUL bytes), `logo.png`
/// (PNG bytes), `fake.png` (text), `pic.svg`, `仕様.md`, `-n`, `--output=x`.
pub(crate) struct Repo {
    pub(crate) _root: tempfile::TempDir,
    pub(crate) _home: tempfile::TempDir,
    pub(crate) git: Git,
    pub(crate) storage: StorageRoot,
    pub(crate) path: PathBuf,
    pub(crate) commit: String,
}

pub(crate) async fn repo() -> Repo {
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, RepoId::new(REPO), uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let p = path.as_path();
    let lib = blob(p, b"pub fn f() {}\n");
    let src = plumb(
        p,
        &["mktree"],
        Some(format!("100644 blob {lib}\tlib.rs\n").as_bytes()),
    );
    let files: Vec<(&str, String)> = vec![
        ("README.md", blob(p, b"# synthetic\n")),
        ("bin.dat", blob(p, b"\0\x01\x02binary")),
        ("logo.png", blob(p, PNG)),
        ("fake.png", blob(p, b"not a png\n")),
        ("pic.svg", blob(p, SVG)),
        ("仕様.md", blob(p, "日本語の仕様\n".as_bytes())),
        ("-n", blob(p, b"dash-n\n")),
        ("--output=x", blob(p, b"option-like\n")),
    ];
    let mut listing = format!("040000 tree {src}\tsrc\n");
    for (name, oid) in &files {
        listing.push_str(&format!("100644 blob {oid}\t{name}\n"));
    }
    let tree = plumb(p, &["mktree"], Some(listing.as_bytes()));
    let commit = plumb(p, &["commit-tree", &tree, "-m", "synthetic"], None);
    for name in ["refs/heads/main", "refs/heads/feature/x", "refs/tags/v1"] {
        plumb(p, &["update-ref", name, &commit], None);
    }
    plumb(p, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
    Repo {
        _root: root,
        _home: home,
        git,
        storage,
        path,
        commit,
    }
}

pub(crate) fn router(repo: &Repo, facts: Facts, limits: BrowseLimits) -> axum::Router {
    let one = Arc::new(OneProject(facts));
    let lookups = Lookups {
        namespaces: Arc::clone(&one) as Arc<dyn forge_core::NamespaceResolver>,
        projects: one as Arc<dyn forge_core::ProjectAccessLoader>,
    };
    let smart = SmartHttp::new(
        repo.git.clone(),     // clone: a shared handle
        repo.storage.clone(), // clone: a path
        GitHttpLimits::default(),
    )
    .with_browse_limits(limits);
    let state = AppState::new("test", Vec::new(), crate::tests::no_tokens(), lookups)
        .with_smart_http(smart);
    build(crate::app::registry(), state, AppConfig::default()).expect("router")
}

pub(crate) fn session() -> Principal {
    Principal::authenticated(UserId::new(5), false, true, Credential::Session)
}

pub(crate) fn token(scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        UserId::new(5),
        false,
        true,
        Credential::Token(scopes.to_vec()),
    )
}

pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) bytes: Vec<u8>,
}

impl Answer {
    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }

    pub(crate) fn code(&self) -> Value {
        self.json()["error_code"].clone()
    }

    pub(crate) fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

pub(crate) async fn send(
    router: axum::Router,
    method: &str,
    uri: &str,
    principal: Option<Principal>,
) -> Answer {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    if let Some(p) = principal {
        req.extensions_mut().insert(p);
    }
    let res = router.oneshot(req).await.expect("infallible");
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

fn reason_logged_never(answer: &Answer, secret: &str) {
    let text = String::from_utf8_lossy(&answer.bytes);
    assert!(!text.contains(secret), "{text}");
}

const API: &str = "/api/v1/projects/7/repository";

async fn get(r: &Repo, uri: &str) -> Answer {
    send(
        router(r, Facts::default(), BrowseLimits::default()),
        "GET",
        uri,
        Some(session()),
    )
    .await
}

#[tokio::test]
async fn refs_list_branches_and_tags_with_the_default_branch() {
    let r = repo().await;
    let a = get(&r, &format!("{API}/refs")).await;
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    let body = a.json();
    assert_eq!(body["default_branch"], "main");
    assert_eq!(body["truncated"], false);
    let names: Vec<&str> = body["refs"]
        .as_array()
        .expect("refs")
        .iter()
        .map(|r| r["full_name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        ["refs/heads/feature/x", "refs/heads/main", "refs/tags/v1"]
    );
    assert_eq!(body["refs"][0]["name"], "feature/x");
    assert_eq!(body["refs"][0]["type"], "branch");
    assert_eq!(body["refs"][0]["commit"], r.commit);
    assert_eq!(a.header("cache-control"), "private, no-store");
    assert_eq!(a.header("vary"), "Authorization, Cookie");

    let tags = get(&r, &format!("{API}/refs?type=tag")).await.json();
    assert_eq!(tags["refs"].as_array().expect("tags").len(), 1);
    let bad = get(&r, &format!("{API}/refs?type=note")).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad.code(), ErrorCode::RequestInvalid.id());
}

#[tokio::test]
async fn resolve_splits_a_spec_into_the_longest_ref_and_a_path() {
    let r = repo().await;
    let a = get(&r, &format!("{API}/resolve?spec=feature/x/src/lib.rs")).await;
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    assert_eq!(a.json()["ref"]["name"], "feature/x");
    assert_eq!(a.json()["ref"]["full_name"], "refs/heads/feature/x");
    assert_eq!(a.json()["path"], "src/lib.rs");
    // Percent-encoded slashes are slashes; a commit id resolves first.
    let a = get(&r, &format!("{API}/resolve?spec={}%2FREADME.md", r.commit)).await;
    assert_eq!(a.json()["ref"]["type"], "commit");
    assert_eq!(a.json()["ref"]["full_name"], Value::Null);
    assert_eq!(a.json()["path"], "README.md");
    let missing = get(&r, &format!("{API}/resolve")).await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    let none = get(&r, &format!("{API}/resolve?spec=nope/README.md")).await;
    assert_eq!(none.status, StatusCode::NOT_FOUND);
    assert_eq!(none.code(), ErrorCode::RepositoryPathNotResolved.id());
    reason_logged_never(&none, "nope");
}

#[tokio::test]
async fn tree_lists_one_level_and_refuses_bad_parameters() {
    let r = repo().await;
    let a = get(&r, &format!("{API}/tree?ref=main")).await;
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    let entries = a.json()["entries"].as_array().expect("entries").clone();
    let src = entries.iter().find(|e| e["name"] == "src").expect("src");
    assert_eq!(src["type"], "tree");
    assert_eq!(src["size"], Value::Null);
    assert!(entries.iter().any(|e| e["name"] == "仕様.md"));
    let sub = get(&r, &format!("{API}/tree?ref=v1&path=src")).await.json();
    assert_eq!(sub["entries"][0]["path"], "src/lib.rs");
    assert_eq!(sub["ref"]["type"], "tag");

    for query in [
        "path=src",
        "ref=main&path=a/../b",
        "ref=main&path=a//b",
        "ref=main&ref=v1",
        "ref=main&x=1",
        "ref=%zz",
        "ref=",
    ] {
        let a = get(&r, &format!("{API}/tree?{query}")).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(a.code(), ErrorCode::RequestInvalid.id(), "{query}");
    }
    let file = get(&r, &format!("{API}/tree?ref=main&path=README.md")).await;
    assert_eq!(file.status, StatusCode::NOT_FOUND);
    assert_eq!(file.code(), ErrorCode::RepositoryPathNotResolved.id());
    // Option-like values are names that do not exist.
    for query in ["ref=--output=x", "ref=-n", "ref=main&path=--output=written"] {
        let a = get(&r, &format!("{API}/tree?{query}")).await;
        assert_eq!(
            a.code(),
            ErrorCode::RepositoryPathNotResolved.id(),
            "{query}"
        );
    }
    assert!(!r.path.join("written").exists());
}

#[tokio::test]
async fn trees_past_the_cap_are_truncated() {
    let r = repo().await;
    let limits = BrowseLimits {
        max_tree_entries: 2,
        ..BrowseLimits::default()
    };
    let a = send(
        router(&r, Facts::default(), limits),
        "GET",
        &format!("{API}/tree?ref=main"),
        Some(session()),
    )
    .await;
    assert_eq!(a.json()["entries"].as_array().expect("entries").len(), 2);
    assert_eq!(a.json()["truncated"], true);
}

#[tokio::test]
async fn blob_returns_text_binary_without_content_and_truncates() {
    let r = repo().await;
    let a = get(&r, &format!("{API}/blob?ref=main&path=README.md")).await;
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    assert_eq!(a.json()["content"], "# synthetic\n");
    assert_eq!(a.json()["binary"], false);
    assert_eq!(a.json()["size"], 12);
    let a = get(
        &r,
        &format!("{API}/blob?ref=main&path=%E4%BB%95%E6%A7%98.md"),
    )
    .await;
    assert_eq!(a.json()["content"], "日本語の仕様\n");
    let a = get(&r, &format!("{API}/blob?ref=main&path=bin.dat")).await;
    assert_eq!(a.json()["binary"], true);
    assert_eq!(a.json()["content"], Value::Null);
    let a = get(&r, &format!("{API}/blob?ref=main&path=--output=x")).await;
    assert_eq!(a.json()["content"], "option-like\n");
    let a = get(&r, &format!("{API}/blob?ref=main&path=src")).await;
    assert_eq!(a.code(), ErrorCode::RepositoryPathNotResolved.id());

    // Cut inside a multi-byte character: the head stays text.
    let limits = BrowseLimits {
        max_blob_bytes: 4,
        ..BrowseLimits::default()
    };
    let a = send(
        router(&r, Facts::default(), limits),
        "GET",
        &format!("{API}/blob?ref=main&path=%E4%BB%95%E6%A7%98.md"),
        Some(session()),
    )
    .await;
    assert_eq!(a.json()["truncated"], true);
    assert_eq!(a.json()["content"], "日");
    assert_eq!(a.json()["binary"], false);
}

#[tokio::test]
async fn the_browse_api_needs_read_api_and_a_reader() {
    let r = repo().await;
    let cases: Vec<(Facts, Option<Principal>, StatusCode, Option<ErrorCode>)> = vec![
        (
            Facts::default(),
            Some(token(&[Scope::ReadApi])),
            StatusCode::OK,
            None,
        ),
        (
            Facts::default(),
            Some(token(&[Scope::ReadRepository])),
            StatusCode::FORBIDDEN,
            Some(ErrorCode::InsufficientScope),
        ),
        (
            Facts {
                role: Some(Role::Guest),
                ..Facts::default()
            },
            Some(session()),
            StatusCode::FORBIDDEN,
            Some(ErrorCode::Forbidden),
        ),
        (
            Facts {
                role: None,
                ..Facts::default()
            },
            Some(session()),
            StatusCode::NOT_FOUND,
            Some(ErrorCode::NotFound),
        ),
        (
            Facts {
                role: None,
                visibility: Visibility::Internal,
                ..Facts::default()
            },
            Some(session()),
            StatusCode::OK,
            None,
        ),
        (
            Facts {
                deleted: true,
                ..Facts::default()
            },
            Some(session()),
            StatusCode::GONE,
            Some(ErrorCode::PendingDeletion),
        ),
        (
            Facts {
                state: RepositoryState::Provisioning,
                ..Facts::default()
            },
            Some(session()),
            StatusCode::SERVICE_UNAVAILABLE,
            Some(ErrorCode::RepositoryNotReady),
        ),
        (
            Facts::default(),
            None,
            StatusCode::UNAUTHORIZED,
            Some(ErrorCode::Unauthenticated),
        ),
    ];
    // Every one of the four routes decides the same way (review MR7).
    let routes = [
        "refs".to_owned(),
        "resolve?spec=main/README.md".to_owned(),
        "tree?ref=main".to_owned(),
        "blob?ref=main&path=README.md".to_owned(),
    ];
    for route in &routes {
        for (facts, principal, status, code) in &cases {
            let a = send(
                router(&r, *facts, BrowseLimits::default()),
                "GET",
                &format!("{API}/{route}"),
                principal.clone(), // clone: each route sends its own
            )
            .await;
            if let Some(code) = code {
                assert_eq!(a.code(), code.id(), "{route}: {:?}", a.json());
                assert_eq!(a.status.as_u16(), code.http_status(), "{route} {code:?}");
            } else {
                assert_eq!(a.status, *status, "{route}: {:?}", a.json());
            }
        }
        // Another project id: the same 404 as an invisible one.
        let a = get(&r, &format!("/api/v1/projects/8/repository/{route}")).await;
        assert_eq!(a.code(), ErrorCode::NotFound.id(), "{route}");
        let a = get(&r, &format!("/api/v1/projects/07/repository/{route}")).await;
        assert_eq!(a.code(), ErrorCode::RequestInvalid.id(), "{route}");
    }
}
