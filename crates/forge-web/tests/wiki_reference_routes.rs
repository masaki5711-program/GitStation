#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `[[wiki:ID]]` in the document API (plan 20 W3, ADR-0040 §5) through the
//! product router, against the development database and real bare
//! repositories: the owner project's document names pages of its linked
//! wiki; the reference resolves against the wiki's default-branch tip for a
//! reader who may read the wiki, shows the id only (`unreadable`) for one
//! who may not, and is `no_wiki` once the wiki is unlinked. Synthetic data
//! only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::{RequestId, Scope, UserId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::Review;
use forge_web::{AppConfig, AppState, GitHttpLimits, Lookups, SmartHttp};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

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

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
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

fn git(repo: &std::path::Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
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

/// `main` = one commit of `files` (a flat `.forge/doctypes` folder and
/// top-level documents).
fn commit_main(repo: &std::path::Path, files: &[(&str, &str)]) {
    let mut types = String::new();
    let mut top = String::new();
    for (path, content) in files {
        let blob = git(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        match path.strip_prefix(".forge/doctypes/") {
            Some(name) => types.push_str(&format!("100644 blob {blob}\t{name}\n")),
            None => top.push_str(&format!("100644 blob {blob}\t{path}\n")),
        }
    }
    let doctypes = git(repo, &["mktree"], Some(types.as_bytes()));
    let forge = git(
        repo,
        &["mktree"],
        Some(format!("040000 tree {doctypes}\tdoctypes\n").as_bytes()),
    );
    top.push_str(&format!("040000 tree {forge}\t.forge\n"));
    let tree = git(repo, &["mktree"], Some(top.as_bytes()));
    let commit = git(repo, &["commit-tree", &tree, "-m", "synthetic"], None);
    git(repo, &["update-ref", "refs/heads/main", &commit], None);
    git(repo, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
}

const T_NOTE: &str = "name: note\nversion: 1\nlabel: { ja: メモ, en: Note, id: Catatan }\nid_pattern: '^N-[0-9]+$'\ndefaults: { classification: internal }\n";
const N1: &str = "---\ndoctype: note\nid: N-1\ntitle: 合成のメモ\nowner: qa\n---\n\n\
見る [[wiki:WIKI-0001]] と [[wiki:WIKI-0099]]。\n";
/// The wiki's first page title (the display name given at creation).
const WIKI_TITLE: &str = "Synthetic Wiki Title";

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    router: axum::Router,
    olga: Principal,
    nina: Principal,
    project: i64,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let mut ids = Vec::new();
    for name in ["olga", "nina"] {
        let u = users::create(
            &t.db,
            &ctx(&format!("req-{name}")),
            &NewUser::human(name, name, &format!("{name}@example.test")),
        )
        .await
        .expect("user");
        ids.push(u);
    }
    let mut new = NewProject::new("web", ids[0].namespace_id, "Web", ids[0].user_id);
    new.visibility = Visibility::Internal;
    let web = projects::create(&t.db, &ctx("req-web"), &new)
        .await
        .expect("web");
    let _ready = repositories::mark_ready(&t.db, &ctx("req-ready"), web.repository_id)
        .await
        .expect("ready");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let g = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&g, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    commit_main(
        &path,
        &[(".forge/doctypes/note.yml", T_NOTE), ("N-1.md", N1)],
    );
    let smart = SmartHttp::new(g.clone(), storage.clone(), GitHttpLimits::default())
        .with_push(Arc::new(t.db.clone()), Arc::new(t.db.clone()));
    let browser = smart.browse.clone();
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(t.db.clone()),
            projects: Arc::new(t.db.clone()),
        },
    )
    .with_smart_http(smart)
    .with_hosting(forge_hosting::Hosting::new(t.db.clone()))
    .with_review(Review::new(t.db.clone(), Arc::new(browser)));
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    World {
        olga: token(ids[0].user_id, &[Scope::Api]),
        nina: token(ids[1].user_id, &[Scope::ReadApi]),
        t,
        _home: home,
        _root: root,
        router,
        project: web.project_id.get(),
    }
}

impl World {
    async fn send(
        &self,
        method: &str,
        uri: &str,
        who: &Principal,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let mut req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .expect("request");
        req.extensions_mut().insert(who.clone());
        let res = self.router.clone().oneshot(req).await.expect("infallible");
        let status = res.status();
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// The wiki references of N-1 as `who` reads them: (doc, target,
    /// title, wiki_project_id), and the whole answer.
    async fn refs(&self, who: &Principal) -> (Vec<(String, String, Value, Value)>, String) {
        let uri = format!("/api/v1/projects/{}/documents/N-1", self.project);
        let (status, body) = self.send("GET", &uri, who, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let refs = body["refs"]
            .as_array()
            .expect("refs")
            .iter()
            .map(|r| {
                assert_eq!(r["wiki"], true, "{r}");
                (
                    r["doc"].as_str().expect("doc").to_owned(),
                    r["target"].as_str().expect("target").to_owned(),
                    r["title"].clone(),
                    r["wiki_project_id"].clone(),
                )
            })
            .collect();
        (refs, body.to_string())
    }

    async fn create_wiki(&self) -> i64 {
        let uri = format!("/api/v1/projects/{}/wiki", self.project);
        let (status, body) = self
            .send(
                "POST",
                &uri,
                &self.olga,
                Some(json!({"display_name": WIKI_TITLE})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["wiki_project_id"].as_i64().expect("id")
    }
}

#[tokio::test]
async fn readable_wiki_ref_resolves_against_the_wiki_tip() {
    let w = world().await;
    let (refs, _) = w.refs(&w.olga).await;
    assert_eq!(
        refs.iter().map(|r| r.1.as_str()).collect::<Vec<_>>(),
        vec!["no_wiki", "no_wiki"]
    );
    let wiki = w.create_wiki().await;
    let (refs, _) = w.refs(&w.olga).await;
    assert_eq!(
        refs,
        vec![
            (
                "WIKI-0001".to_owned(),
                "found".to_owned(),
                json!(WIKI_TITLE),
                json!(wiki)
            ),
            (
                "WIKI-0099".to_owned(),
                "broken".to_owned(),
                Value::Null,
                Value::Null
            ),
        ]
    );
    // Unlinked: no wiki again (the wiki project stays).
    let uri = format!("/api/v1/projects/{}/wiki", w.project);
    let (status, body) = w.send("DELETE", &uri, &w.olga, None).await;
    assert!(status.is_success(), "{body}");
    let (refs, _) = w.refs(&w.olga).await;
    assert!(refs.iter().all(|r| r.1 == "no_wiki"), "{refs:?}");
}

#[tokio::test]
async fn unreadable_wiki_ref_shows_id_only() {
    let w = world().await;
    let wiki = w.create_wiki().await;
    // The owner project is internal (Nina reads it); its wiki is made
    // private, and Nina is no member of either.
    w.t.commit_audited(
        ("project", wiki),
        &[format!(
            "UPDATE forge.projects SET visibility = 'private' WHERE id = {wiki}"
        )],
    )
    .await;
    let (refs, whole) = w.refs(&w.nina).await;
    assert_eq!(
        refs,
        vec![
            (
                "WIKI-0001".to_owned(),
                "unreadable".to_owned(),
                Value::Null,
                Value::Null
            ),
            (
                "WIKI-0099".to_owned(),
                "unreadable".to_owned(),
                Value::Null,
                Value::Null
            ),
        ]
    );
    // Nothing of the wiki: not its title, not its id.
    assert!(!whole.contains(WIKI_TITLE), "{whole}");
    assert!(
        !whole.contains(&format!("\"wiki_project_id\":{wiki}")),
        "{whole}"
    );
    // The owner still sees it.
    let (refs, _) = w.refs(&w.olga).await;
    assert_eq!(refs[0].1, "found");
}
