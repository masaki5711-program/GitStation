#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! `GET /api/v1/projects/{id}/repository/spec-refs` end to end through the
//! product router (M3 X1): a scripted resolver / project loader (no
//! database) that records who every lookup was made for, and real bare
//! repositories (synthetic content).

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    GroupId, LookupPath, NamespaceId, NamespacePath, ProjectAccess, ProjectId, ProjectRecord,
    ProjectRepository, RepoId, RepositoryState, Resolution, Resolved, ResolvedTarget, Role,
    TombstoneId, UserId, Visibility,
};
use forge_git::browse::BrowseLimits;
use forge_git::{Git, GitConfig, StorageRoot};
use serde_json::Value;

use crate::app::{build, AppConfig, AppState, Lookups};
use crate::routes::browse::tests::{blob, plumb, send, Answer};
use crate::routes::git::{GitHttpLimits, SmartHttp};

const READER: UserId = UserId::new(5);
const API: &str = "/api/v1/projects/7/repository/spec-refs";

/// A project the scripted resolver knows: its id, the reader's facts and
/// its repository.
#[derive(Clone, Copy)]
struct Known {
    id: i64,
    visibility: Visibility,
    role: Option<Role>,
    deleted: bool,
    repo: i64,
    state: RepositoryState,
}

impl Known {
    const fn private(id: i64, role: Option<Role>, repo: i64) -> Self {
        Self {
            id,
            visibility: Visibility::Private,
            role,
            deleted: false,
            repo,
            state: RepositoryState::Ready,
        }
    }

    fn access(&self) -> ProjectAccess {
        ProjectAccess {
            visibility: self.visibility,
            role: self.role,
            pending_deletion: self.deleted,
        }
    }

    fn repository(&self) -> ProjectRepository {
        ProjectRepository {
            id: RepoId::new(self.repo),
            state: self.state,
        }
    }
}

/// `acme/app` (7, the referencing project) and the spec repositories:
/// `specs/engine` (8, Reporter), `specs/secret` (9, no role),
/// `specs/open` (10, internal, no role), `specs/new` (11, still
/// provisioning), `specs/gone` (12, pending deletion), `specs/old` (a
/// moved tombstone of 8), `specs/deleted` (a deleted tombstone), `acme`
/// (a group). Every lookup is recorded with its viewer.
struct World {
    calls: Mutex<Vec<(String, Option<UserId>)>>,
}

fn known(path: &str) -> Option<Known> {
    Some(match path {
        "acme/app" => Known::private(7, Some(Role::Reporter), 70),
        "specs/engine" => Known::private(8, Some(Role::Reporter), 80),
        "specs/secret" => Known::private(9, None, 90),
        "specs/open" => Known {
            visibility: Visibility::Internal,
            ..Known::private(10, None, 100)
        },
        "specs/new" => Known {
            state: RepositoryState::Provisioning,
            ..Known::private(11, Some(Role::Reporter), 110)
        },
        "specs/gone" => Known {
            deleted: true,
            ..Known::private(12, Some(Role::Owner), 120)
        },
        // Readable, but its repository is missing from storage: git fails.
        "specs/broken" => Known::private(13, Some(Role::Reporter), 130),
        // Readable; its lookup takes 400 ms (the request budget test).
        "specs/slow" => Known::private(14, Some(Role::Reporter), 80),
        _ => return None,
    })
}

fn live(k: Known) -> Resolved {
    Resolved {
        namespace_id: NamespaceId::new(k.id * 10 + 1),
        target: ResolvedTarget::Project {
            id: ProjectId::new(k.id),
            access: k.access(),
            repository: k.repository(),
        },
    }
}

impl forge_core::NamespaceResolver for World {
    fn resolve<'a>(
        &'a self,
        path: &'a LookupPath,
        viewer: Option<UserId>,
    ) -> LookupFuture<'a, Resolution> {
        let full = path.full_path();
        self.calls.lock().unwrap().push((full.clone(), viewer));
        let slow = full == "specs/slow";
        let answer = match full.as_str() {
            "specs/old" => Resolution::Moved {
                resolved: live(known("specs/engine").unwrap()),
                tombstone: TombstoneId::new(3),
                current: NamespacePath::parse("specs/engine").unwrap(),
            },
            "specs/deleted" => Resolution::Deleted {
                tombstone: TombstoneId::new(4),
            },
            "acme" => Resolution::Live(Resolved {
                namespace_id: NamespaceId::new(2),
                target: ResolvedTarget::Group {
                    id: GroupId::new(2),
                    viewer_role: Some(Role::Owner),
                },
            }),
            other => known(other).map_or(Resolution::Absent, |k| Resolution::Live(live(k))),
        };
        Box::pin(async move {
            if slow {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
            Ok(answer)
        })
    }
}

impl forge_core::ProjectAccessLoader for World {
    fn load_project<'a>(
        &'a self,
        project: ProjectId,
        _viewer: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>> {
        let k = known("acme/app").unwrap();
        let answer = (project.get() == k.id).then(|| ProjectRecord {
            id: project,
            access: k.access(),
            namespace_id: NamespaceId::new(71),
            path: NamespacePath::parse("acme/app").expect("path"),
            display_name: "App".to_owned(),
            description: "synthetic".to_owned(),
            created_at: "2026-11-01T00:00:00Z".to_owned(),
            repository: k.repository(),
        });
        Box::pin(async move { Ok(answer) })
    }
}

fn mktree(repo: &Path, entries: &[(&str, &str, &str, &str)]) -> String {
    let mut input = String::new();
    for (mode, kind, oid, name) in entries {
        input.push_str(&format!("{mode} {kind} {oid}\t{name}\n"));
    }
    plumb(repo, &["mktree"], Some(input.as_bytes()))
}

/// A commit whose tree holds `.forge/repo.yml` = `yml` (none for `None`).
fn commit_with(repo: &Path, yml: Option<&str>) -> String {
    let readme = blob(repo, b"# synthetic\n");
    let mut root = vec![("100644", "blob", readme, "README.md".to_owned())];
    if let Some(text) = yml {
        let file = blob(repo, text.as_bytes());
        let forge = mktree(repo, &[("100644", "blob", &file, "repo.yml")]);
        root.push(("040000", "tree", forge, ".forge".to_owned()));
    }
    let entries: Vec<(&str, &str, &str, &str)> = root
        .iter()
        .map(|(m, k, o, n)| (*m, *k, o.as_str(), n.as_str()))
        .collect();
    let tree = mktree(repo, &entries);
    plumb(repo, &["commit-tree", &tree, "-m", "synthetic"], None)
}

struct Fixture {
    _root: tempfile::TempDir,
    _home: tempfile::TempDir,
    world: Arc<World>,
    router: axum::Router,
    /// `acme/app`'s `main`.
    app_main: String,
    /// `specs/engine`'s tag `v1.4` and branch `main`.
    v14: String,
    engine_main: String,
}

const MAIN_YML: &str = "\
# synthetic
specs:
  - repo: specs/engine
    version: v1.4
    scope: [ZEVA-CFG-*]
  - repo: Specs/Engine-Tracked
    track: main
  - path: spec/
";

async fn fixture(branches: &[(&str, Option<&str>)]) -> Fixture {
    fixture_with(branches, |s| s).await
}

async fn fixture_with(
    branches: &[(&str, Option<&str>)],
    tweak: impl FnOnce(AppState) -> AppState,
) -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let app = storage
        .init_bare(&git, RepoId::new(70), uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let app_main = commit_with(&app, Some(MAIN_YML));
    plumb(&app, &["update-ref", "refs/heads/main", &app_main], None);
    plumb(&app, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
    for (name, yml) in branches {
        let c = commit_with(&app, *yml);
        plumb(
            &app,
            &["update-ref", &format!("refs/heads/{name}"), &c],
            None,
        );
    }
    let engine = storage
        .init_bare(&git, RepoId::new(80), uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let v14 = commit_with(&engine, None);
    let engine_main = plumb(
        &engine,
        &[
            "commit-tree",
            &format!("{v14}^{{tree}}"),
            "-p",
            &v14,
            "-m",
            "next",
        ],
        None,
    );
    plumb(&engine, &["tag", "-a", "-m", "v1.4", "v1.4", &v14], None);
    plumb(
        &engine,
        &["update-ref", "refs/heads/main", &engine_main],
        None,
    );
    // specs/open (internal): an empty repository.
    storage
        .init_bare(&git, RepoId::new(100), uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let world = Arc::new(World {
        calls: Mutex::new(Vec::new()),
    });
    let lookups = Lookups {
        namespaces: Arc::clone(&world) as Arc<dyn forge_core::NamespaceResolver>,
        projects: Arc::clone(&world) as Arc<dyn forge_core::ProjectAccessLoader>,
    };
    let smart = SmartHttp::new(git, storage, GitHttpLimits::default())
        .with_browse_limits(BrowseLimits::default());
    let state = tweak(
        AppState::new("test", Vec::new(), crate::tests::no_tokens(), lookups)
            .with_smart_http(smart),
    );
    let router = build(crate::app::registry(), state, AppConfig::default()).expect("router");
    Fixture {
        _root: root,
        _home: home,
        world,
        router,
        app_main,
        v14,
        engine_main,
    }
}

fn person() -> Principal {
    Principal::authenticated(READER, false, true, Credential::Session, AccountKind::Human)
}

fn token(scopes: &[Scope], kind: AccountKind) -> Principal {
    Principal::authenticated(
        READER,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        kind,
    )
}

async fn get(f: &Fixture, query: &str, who: Principal) -> Answer {
    send(f.router.clone(), "GET", &format!("{API}{query}"), Some(who)).await
}

fn ok_body(a: &Answer) -> Value {
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json());
    assert_eq!(a.header("cache-control"), "private, no-store");
    a.json()
}

/// The entry naming `repo` (as written).
fn entry<'a>(body: &'a Value, repo: &str) -> &'a Value {
    body["specs"]
        .as_array()
        .expect("specs")
        .iter()
        .find(|e| e["repo"] == repo)
        .unwrap_or_else(|| panic!("no entry for {repo}: {body}"))
}

#[tokio::test]
async fn the_pinned_references_resolve_for_a_reader_who_may_read_them() {
    let f = fixture(&[(
        "pins",
        Some(
            "specs:
  - repo: specs/engine
    version: v1.4
    scope: [ZEVA-CFG-*]
  - path: spec/
",
        ),
    )])
    .await;
    let body = ok_body(&get(&f, "?ref=pins", person()).await);
    assert_eq!(body["config"], "valid");
    assert_eq!(body["path"], ".forge/repo.yml");
    assert_eq!(body["problem"], Value::Null);
    assert_eq!(body["ref"]["name"], "pins");
    let pinned = entry(&body, "specs/engine");
    assert_eq!(pinned["type"], "repo");
    assert_eq!(pinned["line"], 2);
    assert_eq!(pinned["status"], "resolved");
    assert_eq!(pinned["error_code"], Value::Null);
    assert_eq!(pinned["project_id"], 8);
    assert_eq!(pinned["pin"]["type"], "version");
    assert_eq!(pinned["pin"]["name"], "v1.4");
    assert_eq!(pinned["pin"]["ref_name"], "refs/tags/v1.4");
    // An annotated tag peels to its commit.
    assert_eq!(pinned["commit"], f.v14.as_str());
    assert_eq!(pinned["scope"], serde_json::json!(["ZEVA-CFG-*"]));
    let local = body["specs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "path")
        .expect("the local entry");
    assert_eq!(local["path"], "spec/");
    assert_eq!(local["status"], "local");
    assert_eq!(local["commit"], body["ref"]["commit"]);
    // Every lookup was made for the reader.
    let calls = f.world.calls.lock().unwrap().clone();
    assert!(!calls.is_empty());
    assert!(calls.iter().all(|(_, v)| *v == Some(READER)), "{calls:?}");
}

#[tokio::test]
async fn a_tracked_branch_resolves_to_its_tip_and_the_default_branch_is_read_without_ref() {
    let f = fixture(&[(
        "tracked",
        Some("specs:\n  - repo: Specs/Engine\n    track: main\n"),
    )])
    .await;
    let body = ok_body(&get(&f, "?ref=tracked", person()).await);
    let tracked = entry(&body, "Specs/Engine");
    assert_eq!(tracked["status"], "resolved");
    assert_eq!(tracked["pin"]["type"], "track");
    assert_eq!(tracked["pin"]["ref_name"], "refs/heads/main");
    assert_eq!(tracked["commit"], f.engine_main.as_str());

    // No `ref`: the default branch (main, whose file pins v1.4).
    let body = ok_body(&get(&f, "", person()).await);
    assert_eq!(body["ref"]["name"], "main");
    assert_eq!(body["ref"]["commit"], f.app_main.as_str());
    assert_eq!(entry(&body, "specs/engine")["commit"], f.v14.as_str());
}

/// Every reason a reference cannot be followed answers the same — no
/// project id, no commit, one code — so the answer is no existence oracle.
#[tokio::test]
async fn an_unreadable_spec_repository_is_spec_repository_unavailable() {
    let unreadable = [
        "specs/secret",
        "specs/new",
        "specs/gone",
        "specs/old",
        "specs/deleted",
        "acme",
        "specs/nothing-here",
    ];
    let mut yml = String::from("specs:\n");
    for (i, path) in unreadable.iter().enumerate() {
        yml.push_str(&format!("  - repo: {path}\n    version: v{i}\n"));
    }
    let f = fixture(&[("refs", Some(&yml))]).await;
    let body = ok_body(&get(&f, "?ref=refs", person()).await);
    assert_eq!(body["config"], "valid");
    for path in unreadable {
        let e = entry(&body, path);
        assert_eq!(e["status"], "unavailable", "{path}: {e}");
        assert_eq!(e["error_code"], "FG-REPO-0026", "{path}");
        assert_eq!(e["project_id"], Value::Null, "{path}");
        assert_eq!(e["commit"], Value::Null, "{path}");
        let mut keys: Vec<&String> = e.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "commit",
                "error_code",
                "line",
                "path",
                "pin",
                "project_id",
                "repo",
                "scope",
                "status",
                "type"
            ],
            "{path}"
        );
    }
    let text = String::from_utf8_lossy(&get(&f, "?ref=refs", person()).await.bytes).into_owned();
    // Nothing of the moved target leaks (its id 8, its current path).
    assert!(!text.contains("specs/engine"), "{text}");
    assert!(!text.contains("\"project_id\":8"), "{text}");
}

#[tokio::test]
async fn a_service_account_follows_only_projects_it_is_a_member_of() {
    let f = fixture(&[(
        "open",
        Some("specs:\n  - repo: specs/open\n    version: v1\n"),
    )])
    .await;
    // A person reads the internal project (its repository has no tag:
    // the version is not found, but the project was followed).
    let body = ok_body(&get(&f, "?ref=open", person()).await);
    let e = entry(&body, "specs/open");
    assert_eq!(e["project_id"], 10, "{e}");
    // A service account is no member: unavailable.
    let sa = token(&[Scope::ReadApi], AccountKind::ServiceAccount);
    let body = ok_body(&get(&f, "?ref=open", sa).await);
    let e = entry(&body, "specs/open");
    assert_eq!(e["status"], "unavailable");
    assert_eq!(e["error_code"], "FG-REPO-0026");
}

#[tokio::test]
async fn a_missing_spec_version_is_spec_version_not_found() {
    let f = fixture(&[(
        "missing",
        Some("specs:\n  - repo: specs/engine\n    version: v9.9\n  - repo: specs/open\n    track: nope\n"),
    )])
    .await;
    let body = ok_body(&get(&f, "?ref=missing", person()).await);
    let e = entry(&body, "specs/engine");
    assert_eq!(e["status"], "version_not_found");
    assert_eq!(e["error_code"], "FG-REPO-0025");
    assert_eq!(e["project_id"], 8);
    assert_eq!(e["commit"], Value::Null);
}

#[tokio::test]
async fn an_invalid_repo_yml_is_repo_config_invalid_with_line_and_reason() {
    let f = fixture(&[
        (
            "bad",
            Some("specs:\n  - repo: specs/engine\n    version: v1.4\n    kind: spec\n"),
        ),
        ("none", None),
    ])
    .await;
    let before = f.world.calls.lock().unwrap().len();
    let body = ok_body(&get(&f, "?ref=bad", person()).await);
    assert_eq!(body["config"], "invalid");
    assert_eq!(body["problem"]["error_code"], "FG-REPO-0024");
    assert_eq!(body["problem"]["line"], 4);
    assert_eq!(body["problem"]["reason"], "unknown_key");
    assert_eq!(body["specs"], serde_json::json!([]));
    // Nothing was followed from an invalid file.
    assert_eq!(f.world.calls.lock().unwrap().len(), before);

    let body = ok_body(&get(&f, "?ref=none", person()).await);
    assert_eq!(body["config"], "absent");
    assert_eq!(body["problem"], Value::Null);
    assert_eq!(body["specs"], serde_json::json!([]));
}

#[tokio::test]
async fn the_route_is_decided_on_the_referencing_project_and_strict_on_its_query() {
    let f = fixture(&[]).await;
    let a = send(f.router.clone(), "GET", API, None).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    let git_only = token(&[Scope::ReadRepository], AccountKind::Human);
    let a = get(&f, "", git_only).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN);
    assert_eq!(a.code(), "FG-AUTH-0002");
    let a = send(
        f.router.clone(),
        "GET",
        "/api/v1/projects/99/repository/spec-refs",
        Some(person()),
    )
    .await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    assert_eq!(a.code(), "FG-REPO-0005");
    let a = get(&f, "?ref=no-such-branch", person()).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    assert_eq!(a.code(), "FG-REPO-0023");
    for q in ["?bogus=1", "?ref=main&ref=main", "?ref="] {
        let a = get(&f, q, person()).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{q}");
        assert_eq!(a.code(), "FG-WEB-0006", "{q}");
    }
}

// ── review X1 (MEDIUM 1, 2; LOW 4) ─────────────────────────────────────

/// A git failure on a referenced repository (here: its directory is
/// missing) is that entry's `unavailable`, never the request's status.
#[tokio::test]
async fn a_git_failure_on_a_referenced_repository_is_unavailable_not_the_status() {
    let f = fixture(&[(
        "broken",
        Some(
            "specs:
  - repo: specs/broken
    version: v1
  - repo: specs/engine
    version: v1.4
",
        ),
    )])
    .await;
    let body = ok_body(&get(&f, "?ref=broken", person()).await);
    let e = entry(&body, "specs/broken");
    assert_eq!(e["status"], "unavailable", "{e}");
    assert_eq!(e["error_code"], "FG-REPO-0026");
    assert_eq!(e["project_id"], Value::Null);
    // The other entries are still resolved.
    assert_eq!(entry(&body, "specs/engine")["status"], "resolved");
}

/// One budget for the whole request: a slow lookup ends it with
/// FG-WEB-0002 (operation=spec_refs), never a worker held per entry.
#[tokio::test]
async fn the_whole_request_has_one_budget() {
    let f = fixture_with(
        &[(
            "slow",
            Some(
                "specs:
  - repo: specs/slow
    version: v1.4
",
            ),
        )],
        |s| s.with_spec_refs_budget(std::time::Duration::from_millis(100)),
    )
    .await;
    let a = get(&f, "?ref=slow", person()).await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "{:?}", a.json());
    assert_eq!(a.code(), "FG-WEB-0002");
}

/// Every followed reference counts against the caller's limit.
#[tokio::test]
async fn every_followed_reference_counts_against_the_callers_limit() {
    let f = fixture_with(
        &[(
            "three",
            Some(
                "specs:
  - repo: specs/engine
    version: v1.4
  - repo: specs/open
    version: v1
  - repo: specs/secret
    version: v1
  - path: spec/
",
            ),
        )],
        |s| {
            s.with_spec_refs_limiter(crate::routes::namespaces::CallerLimiter::new(
                "spec_refs",
                4,
                std::time::Duration::from_secs(60),
            ))
        },
    )
    .await;
    // Three references: within the limit of four.
    ok_body(&get(&f, "?ref=three", person()).await);
    // Three more: the fifth is refused.
    let a = get(&f, "?ref=three", person()).await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS, "{:?}", a.json());
    assert_eq!(a.code(), "FG-WEB-0010");
    assert!(!a.header("retry-after").is_empty());
}

#[test]
fn the_repo_config_path_is_a_tree_path() {
    let path = super::repo_config_path().expect("parses");
    assert_eq!(path.as_str(), ".forge/repo.yml");
}
