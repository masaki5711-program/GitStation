#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! The spec API `GET /api/v1/projects/{id}/specs/…` end to end through the
//! product router (M3 X4, plan 03 §7.2): a scripted resolver / project
//! loader (no database) and real bare repositories (synthetic content).

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    LookupPath, NamespaceId, NamespacePath, ProjectAccess, ProjectId, ProjectRecord,
    ProjectRepository, RepoId, RepositoryState, Resolution, Resolved, ResolvedTarget, Role, UserId,
    Visibility,
};
use forge_git::browse::BrowseLimits;
use forge_git::{Git, GitConfig, StorageRoot};
use serde_json::Value;

use crate::app::{build, AppConfig, AppState, Lookups};
use crate::routes::browse::tests::{blob, plumb, send, Answer};
use crate::routes::git::{GitHttpLimits, SmartHttp};

const READER: UserId = UserId::new(5);
const API: &str = "/api/v1/projects/7/specs";

/// `acme/app` (7, Reporter, repository 70: the project read),
/// `specs/engine` (8, Reporter, 80: a referenced specification the reader
/// may read) and `specs/secret` (9, no role: the reader may not).
fn known(path: &str) -> Option<(i64, Option<Role>, i64)> {
    Some(match path {
        "acme/app" => (7, Some(Role::Reporter), 70),
        "specs/engine" => (8, Some(Role::Reporter), 80),
        "specs/secret" => (9, None, 90),
        _ => return None,
    })
}

fn access(role: Option<Role>) -> ProjectAccess {
    ProjectAccess {
        visibility: Visibility::Private,
        role,
        pending_deletion: false,
    }
}

struct World {
    calls: Mutex<Vec<(String, Option<UserId>)>>,
}

impl forge_core::NamespaceResolver for World {
    fn resolve<'a>(
        &'a self,
        path: &'a LookupPath,
        viewer: Option<UserId>,
    ) -> LookupFuture<'a, Resolution> {
        let full = path.full_path();
        self.calls.lock().unwrap().push((full.clone(), viewer));
        let answer = known(&full).map_or(Resolution::Absent, |(id, role, repo)| {
            Resolution::Live(Resolved {
                namespace_id: NamespaceId::new(id * 10 + 1),
                target: ResolvedTarget::Project {
                    id: ProjectId::new(id),
                    access: access(role),
                    repository: ProjectRepository {
                        id: RepoId::new(repo),
                        state: RepositoryState::Ready,
                    },
                },
            })
        });
        Box::pin(async move { Ok(answer) })
    }
}

impl forge_core::ProjectAccessLoader for World {
    fn load_project<'a>(
        &'a self,
        project: ProjectId,
        _viewer: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>> {
        let answer = (project.get() == 7).then(|| ProjectRecord {
            id: project,
            access: access(Some(Role::Reporter)),
            namespace_id: NamespaceId::new(71),
            path: NamespacePath::parse("acme/app").expect("path"),
            display_name: "App".to_owned(),
            description: "synthetic".to_owned(),
            created_at: "2026-11-01T00:00:00Z".to_owned(),
            repository: ProjectRepository {
                id: RepoId::new(70),
                state: RepositoryState::Ready,
            },
        });
        Box::pin(async move { Ok(answer) })
    }
}

/// A commit of nested `files`.
fn commit(repo: &Path, files: &[(&str, &str)], parent: Option<&str>) -> String {
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Dir {
        files: BTreeMap<String, String>,
        dirs: BTreeMap<String, Dir>,
    }
    fn write(repo: &Path, d: &Dir) -> String {
        let mut listing = String::new();
        for (name, sub) in &d.dirs {
            listing.push_str(&format!("040000 tree {}\t{name}\n", write(repo, sub)));
        }
        for (name, b) in &d.files {
            listing.push_str(&format!("100644 blob {b}\t{name}\n"));
        }
        plumb(repo, &["mktree"], Some(listing.as_bytes()))
    }
    let mut root = Dir::default();
    for (path, content) in files {
        let b = blob(repo, content.as_bytes());
        let mut segs: Vec<&str> = path.split('/').collect();
        let name = segs.pop().expect("name");
        let mut d = &mut root;
        for s in segs {
            d = d.dirs.entry(s.to_owned()).or_default();
        }
        d.files.insert(name.to_owned(), b);
    }
    let tree = write(repo, &root);
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

const FRONT: &str = "---\nspec: app/core\ntitle: Core\nstatus: approved\n---\n";

const REPO_YML: &str = "specs:
  - repo: specs/engine
    version: v1.4
    scope: [ZEVA-CFG-*]
  - repo: specs/secret
    version: v1
  - path: spec/
";

struct Fixture {
    _root: tempfile::TempDir,
    _home: tempfile::TempDir,
    world: Arc<World>,
    router: axum::Router,
    v1: String,
    main: String,
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
    let one = format!("{FRONT}::: req APP-CORE-1\nfirst\n:::\n");
    let v1 = commit(&app, &[("spec/core.md", &one)], None);
    plumb(&app, &["update-ref", "refs/tags/v1.0", &v1], None);
    plumb(&app, &["update-ref", "refs/tags/stable", &v1], None);
    let two = format!(
        "{FRONT}::: req APP-CORE-1 priority=must\nfirst, changed\n:::\n\n::: req APP-CORE-2\nsecond reload rule\n:::\n"
    );
    let main = commit(
        &app,
        &[
            (".forge/repo.yml", REPO_YML),
            ("spec/core.md", &two),
            ("spec/README.md", "# Specs\n"),
            ("tests/t.rs", "// @req APP-CORE-1\n// @req ZEVA-CFG-12\n"),
        ],
        Some(&v1),
    );
    plumb(&app, &["update-ref", "refs/heads/main", &main], None);
    plumb(&app, &["update-ref", "refs/tags/v1.1", &main], None);
    plumb(&app, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
    let engine = storage
        .init_bare(&git, RepoId::new(80), uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let config = "---\nspec: engine/config\ntitle: Config\nstatus: approved\n---\n::: req ZEVA-CFG-12\nreload\n:::\n::: req ZEVA-CFG-13\nkeep\n:::\n::: req OTHER-X-1\nout of scope\n:::\n";
    let e = commit(&engine, &[("spec/config.md", config)], None);
    plumb(&engine, &["update-ref", "refs/tags/v1.4", &e], None);
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
        world,
        router,
        v1,
        main,
    }
}

fn person() -> Principal {
    Principal::authenticated(READER, false, true, Credential::Session, AccountKind::Human)
}

async fn get(f: &Fixture, path: &str, who: Principal) -> Answer {
    send(f.router.clone(), "GET", &format!("{API}/{path}"), Some(who)).await
}

fn ok_body(a: &Answer) -> Value {
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    assert_eq!(a.header("cache-control"), "private, no-store");
    a.json()
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .expect("a list")
        .iter()
        .map(|r| r["id"].as_str().expect("id").to_owned())
        .collect()
}

#[tokio::test]
async fn requirements_lists_the_own_specification_at_a_revision() {
    let f = fixture().await;
    let body = ok_body(&get(&f, "requirements", person()).await);
    assert_eq!(body["ref"]["name"], "main");
    assert_eq!(body["ref"]["commit"], f.main.as_str());
    assert_eq!(body["config"], "valid");
    assert_eq!(body["dirs"], serde_json::json!(["spec/"]));
    assert_eq!(ids(&body["requirements"]), ["APP-CORE-1", "APP-CORE-2"]);
    let first = &body["requirements"][0];
    assert_eq!(first["path"], "spec/core.md");
    assert_eq!(first["line"], 6);
    assert_eq!(first["priority"], "must");
    assert_eq!(first["deprecated"], false);
    assert_eq!(first["spec"], "app/core");
    assert_eq!(first["body"], "first, changed");
    assert_eq!(body["documents"][0]["path"], "spec/core.md");
    assert_eq!(body["documents"][0]["requirements"], 2);
    assert_eq!(body["problems"], serde_json::json!([]));
    assert_eq!(body["incomplete"], Value::Null);
    assert_eq!(body["truncated"], false);
    // A search, a version, a limit.
    let found = ok_body(&get(&f, "requirements?q=RELOAD", person()).await);
    assert_eq!(ids(&found["requirements"]), ["APP-CORE-2"]);
    let old = ok_body(&get(&f, "requirements?ref=v1.0", person()).await);
    assert_eq!(ids(&old["requirements"]), ["APP-CORE-1"]);
    assert_eq!(old["config"], "absent");
    let one = ok_body(&get(&f, "requirements?limit=1", person()).await);
    assert_eq!(ids(&one["requirements"]), ["APP-CORE-1"]);
    assert_eq!(one["truncated"], true);
    let bad = get(&f, "requirements?limit=0", person()).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{:?}", bad.json());
    assert_eq!(bad.json()["error_code"], "FG-WEB-0006");
}

#[tokio::test]
async fn versions_lists_the_version_tags_newest_first() {
    let f = fixture().await;
    let body = ok_body(&get(&f, "versions", person()).await);
    let names: Vec<&str> = body["versions"]
        .as_array()
        .expect("versions")
        .iter()
        .map(|v| v["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["v1.1", "v1.0"], "`stable` is no version");
    assert_eq!(body["versions"][1]["commit"], f.v1.as_str());
    assert_eq!(body["versions"][1]["full_name"], "refs/tags/v1.0");
    assert_eq!(body["truncated"], false);
}

#[tokio::test]
async fn diff_compares_two_revisions_requirement_by_requirement() {
    let f = fixture().await;
    let body = ok_body(&get(&f, "diff?from=v1.0&to=v1.1", person()).await);
    assert_eq!(body["from"]["commit"], f.v1.as_str());
    assert_eq!(body["to"]["commit"], f.main.as_str());
    let changes: Vec<(String, String)> = body["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .map(|c| {
            (
                c["id"].as_str().expect("id").to_owned(),
                c["kind"].as_str().expect("kind").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        changes,
        [
            ("APP-CORE-1".to_owned(), "changed".to_owned()),
            ("APP-CORE-2".to_owned(), "added".to_owned()),
        ]
    );
    let missing = get(&f, "diff?from=v1.0", person()).await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    assert_eq!(missing.json()["error_code"], "FG-WEB-0006");
    let unknown = get(&f, "diff?from=v9&to=main", person()).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.json()["error_code"], "FG-REPO-0023");
}

#[tokio::test]
async fn trace_lists_every_requirement_with_its_tests_and_follows_references_for_the_reader() {
    let f = fixture().await;
    let body = ok_body(&get(&f, "trace", person()).await);
    let rows: Vec<(String, String, u64)> = body["requirements"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| {
            (
                r["id"].as_str().expect("id").to_owned(),
                r["source"].as_str().expect("source").to_owned(),
                r["test_count"].as_u64().expect("count"),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("APP-CORE-1".to_owned(), "spec/".to_owned(), 1),
            ("APP-CORE-2".to_owned(), "spec/".to_owned(), 0),
            ("ZEVA-CFG-12".to_owned(), "specs/engine".to_owned(), 1),
            ("ZEVA-CFG-13".to_owned(), "specs/engine".to_owned(), 0),
        ],
        "OTHER-X-1 is outside the pinned scope"
    );
    assert_eq!(body["requirements"][0]["tests"][0]["path"], "tests/t.rs");
    assert_eq!(body["requirements"][0]["tests"][0]["line"], 1);
    let refs = body["references"].as_array().expect("references");
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0]["repo"], "specs/engine");
    assert_eq!(refs[0]["status"], "resolved");
    assert_eq!(refs[0]["requirements"], 2);
    assert_eq!(refs[1]["repo"], "specs/secret");
    assert_eq!(refs[1]["status"], "unavailable");
    assert_eq!(refs[1]["error_code"], "FG-REPO-0026");
    assert_eq!(refs[1]["requirements"], 0);
    // Every reference was looked up for the reader.
    let calls = f.world.calls.lock().unwrap().clone();
    assert!(!calls.is_empty());
    assert!(
        calls.iter().all(|(_, viewer)| *viewer == Some(READER)),
        "{calls:?}"
    );
}

#[tokio::test]
async fn the_spec_api_needs_read_api_and_a_readable_project() {
    let f = fixture().await;
    let clone_only = Principal::authenticated(
        READER,
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
        AccountKind::Human,
    );
    let refused = get(&f, "requirements", clone_only).await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "{:?}",
        refused.json()
    );
    assert_eq!(refused.json()["error_code"], "FG-AUTH-0002");
    let other = send(
        f.router.clone(),
        "GET",
        "/api/v1/projects/99/specs/trace",
        Some(person()),
    )
    .await;
    assert_eq!(other.status, StatusCode::NOT_FOUND);
    let anonymous = send(f.router.clone(), "GET", &format!("{API}/versions"), None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
}
