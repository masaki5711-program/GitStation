#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Document revisions and edit claims (plan 13 §7.5, stage 4 part A,
//! ADR-0027) through the product router, against the development database
//! and a real bare repository: opening a second merge request that revises
//! the same document is refused with `FG-DOC-0006` (another document, or
//! no document, is not); opening one releases its author's edit claim; the
//! claim is taken, extended, refused to another person (`FG-DOC-0007`),
//! released, revoked by a Maintainer only (audited); people only; the
//! versions are the applied merges that changed the document's blob.
//! Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::{RequestId, Role, Scope, UserId};
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

fn token(user: UserId, kind: forge_auth::AccountKind) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(vec![Scope::WriteRepository, Scope::Api]),
        kind,
    )
}

fn person(user: UserId) -> Principal {
    token(user, forge_auth::AccountKind::Human)
}

fn code(body: &Value) -> &str {
    body["error_code"].as_str().unwrap_or("")
}

fn git(
    repo: &std::path::Path,
    args: &[&str],
    input: Option<&[u8]>,
    index: Option<&std::path::Path>,
) -> String {
    use std::io::Write as _;
    let mut cmd = std::process::Command::new("git");
    cmd.arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    let mut child = cmd.spawn().expect("git");
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

/// A commit of `files` on `parent`.
fn commit(repo: &std::path::Path, parent: Option<&str>, files: &[(&str, &str)]) -> String {
    let mut listing = String::new();
    for (name, content) in files {
        let blob = git(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
            None,
        );
        listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let dir = tempfile::tempdir().expect("index dir");
    let index = dir.path().join("index");
    git(
        repo,
        &["update-index", "--add", "--index-info"],
        Some(listing.as_bytes()),
        Some(&index),
    );
    let tree = git(repo, &["write-tree"], None, Some(&index));
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    git(repo, &args, None, None)
}

fn doc(version: &str) -> String {
    format!(
        "---\ndoctype: qms.procedure\nid: QP-07-001\ntitle: Synthetic procedure\nowner: qa\nclassification: internal\n---\n\n# Purpose\n\n{version}\n"
    )
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    path: std::path::PathBuf,
    router: axum::Router,
    owner: UserId,
    maintainer: UserId,
    dev: UserId,
    dev2: UserId,
    reporter: UserId,
    project: i64,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let mut ids = Vec::new();
    for name in ["olga", "mia", "dave", "dina", "rita"] {
        let u = users::create(
            &t.db,
            &ctx(&format!("req-{name}")),
            &NewUser::human(name, name, &format!("{name}@example.test")),
        )
        .await
        .expect("user");
        ids.push(u);
    }
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", ids[0].namespace_id, "Web", ids[0].user_id),
    )
    .await
    .expect("web");
    for (who, role) in [
        (ids[1].user_id, Role::Maintainer),
        (ids[2].user_id, Role::Developer),
        (ids[3].user_id, Role::Developer),
        (ids[4].user_id, Role::Reporter),
    ] {
        projects::add_member(&t.db, &ctx("req-member"), web.project_id, who, role)
            .await
            .expect("member");
    }
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
    .with_review(Review::new(t.db.clone(), Arc::new(browser.clone())))
    .with_checks(forge_checks::Checks::new(t.db.clone(), Arc::new(browser)));
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    World {
        owner: ids[0].user_id,
        maintainer: ids[1].user_id,
        dev: ids[2].user_id,
        dev2: ids[3].user_id,
        reporter: ids[4].user_id,
        t,
        _home: home,
        _root: root,
        path,
        router,
        project: web.project_id.get(),
    }
}

impl World {
    fn branch(&self, name: &str, commit: &str) {
        git(
            &self.path,
            &["update-ref", &format!("refs/heads/{name}"), commit],
            None,
            None,
        );
    }

    async fn send(
        &self,
        method: &str,
        uri: &str,
        who: Principal,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let mut req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .expect("request");
        req.extensions_mut().insert(who);
        let res = self.router.clone().oneshot(req).await.expect("infallible");
        let status = res.status();
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn doc_uri(&self, suffix: &str) -> String {
        format!(
            "/api/v1/projects/{}/documents/QP-07-001/{suffix}",
            self.project
        )
    }

    async fn open(&self, who: UserId, source: &str) -> (StatusCode, Value) {
        self.send(
            "POST",
            &format!("/api/v1/projects/{}/merge-requests", self.project),
            person(who),
            Some(json!({"title": format!("revise via {source}"), "source_branch": source, "target_branch": "main"})),
        )
        .await
    }

    async fn status(&self, who: UserId) -> Value {
        let (s, body) = self
            .send("GET", &self.doc_uri("status"), person(who), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        body
    }
}

/// main: README + QP-07-001 v1; `a` and `b` change the document, `c` only
/// the README.
async fn with_branches() -> World {
    let w = world().await;
    let base = commit_files(&w.path, None, &files("# Web\n", "v1", &[]));
    w.branch("main", &base);
    let a = commit_files(&w.path, Some(&base), &files("# Web\n", "a", &[]));
    let b = commit_files(&w.path, Some(&base), &files("# Web\n", "b", &[]));
    let c = commit_files(&w.path, Some(&base), &files("# Web 2\n", "v1", &[]));
    w.branch("a", &a);
    w.branch("b", &b);
    w.branch("c", &c);
    w
}

/// The README, the type definition (a repository holds documents only
/// with one), QP-07-001 at `version`, and `extra` files.
fn files<'a>(
    readme: &'a str,
    version: &str,
    extra: &[(&'a str, &'a str)],
) -> Vec<(&'a str, String)> {
    let mut out = vec![
        ("README.md", readme.to_owned()),
        (
            ".forge/doctypes/qms.procedure.yml",
            "name: qms.procedure\n".to_owned(),
        ),
        ("docs/QP-07-001.md", doc(version)),
    ];
    out.extend(extra.iter().map(|(p, c)| (*p, (*c).to_owned())));
    out
}

#[tokio::test]
async fn a_second_merge_request_revising_a_document_is_refused_at_open() {
    let w = with_branches().await;
    let (s, first) = w.open(w.dev, "a").await;
    assert_eq!(s, StatusCode::CREATED, "{first}");
    let iid = first["iid"].as_i64().expect("iid");

    let (s, refused) = w.open(w.dev2, "b").await;
    assert_eq!(s, StatusCode::CONFLICT, "{refused}");
    assert_eq!(code(&refused), "FG-DOC-0006");
    // Nothing was created.
    let n: Vec<Value> =
        w.t.json_rows("SELECT iid FROM forge.merge_requests ORDER BY iid")
            .await;
    assert_eq!(n.len(), 1, "{n:?}");

    // A merge request that changes no document opens.
    let (s, other) = w.open(w.dev2, "c").await;
    assert_eq!(s, StatusCode::CREATED, "{other}");

    let status = w.status(w.reporter).await;
    assert_eq!(status["doc_id"], "QP-07-001");
    let revisions = status["revisions"].as_array().expect("revisions");
    assert_eq!(revisions.len(), 1, "{status}");
    assert_eq!(revisions[0]["merge_request_iid"], iid);
    assert_eq!(revisions[0]["holds"], true);
    assert_eq!(revisions[0]["lang"], Value::Null);
    // A Reporter may read, not claim.
    assert_eq!(
        status["viewer"],
        json!({"can_claim": false, "can_revoke": false})
    );

    // Closing the first frees the document.
    let (s, _) = w
        .send(
            "POST",
            &format!("/api/v1/projects/{}/merge-requests/{iid}/close", w.project),
            person(w.dev),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, second) = w.open(w.dev2, "b").await;
    assert_eq!(s, StatusCode::CREATED, "{second}");
}

#[tokio::test]
async fn a_claim_held_by_another_person_is_refused() {
    let w = with_branches().await;
    let (s, taken) = w
        .send(
            "PUT",
            &w.doc_uri("edit-claim"),
            person(w.dev),
            Some(json!({})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{taken}");
    assert_eq!(taken["extended"], false);
    assert_eq!(taken["claim"]["user_id"], w.dev.get());
    assert_eq!(taken["claim"]["merge_request_iid"], Value::Null);
    let (s, again) = w
        .send(
            "PUT",
            &w.doc_uri("edit-claim"),
            person(w.dev),
            Some(json!({})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(again["extended"], true);

    let (s, held) = w
        .send(
            "PUT",
            &w.doc_uri("edit-claim"),
            person(w.dev2),
            Some(json!({})),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{held}");
    assert_eq!(code(&held), "FG-DOC-0007");
    let (s, held) = w
        .send("DELETE", &w.doc_uri("edit-claim"), person(w.dev2), None)
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{held}");
    assert_eq!(code(&held), "FG-DOC-0007");

    let status = w.status(w.dev2).await;
    assert_eq!(status["edit_claim"]["user_id"], w.dev.get());
    assert_eq!(
        status["viewer"],
        json!({"can_claim": true, "can_revoke": false})
    );

    // The holder releases; then nothing to release.
    let (s, _) = w
        .send("DELETE", &w.doc_uri("edit-claim"), person(w.dev), None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, none) = w
        .send("DELETE", &w.doc_uri("edit-claim"), person(w.dev), None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{none}");
    assert_eq!(w.status(w.dev).await["edit_claim"], Value::Null);
}

#[tokio::test]
async fn only_a_maintainer_revokes_another_persons_claim_and_it_is_audited() {
    let w = with_branches().await;
    let (s, _) = w
        .send(
            "PUT",
            &w.doc_uri("edit-claim"),
            person(w.dev),
            Some(json!({})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, denied) = w
        .send(
            "POST",
            &w.doc_uri("edit-claim/revoke"),
            person(w.dev2),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{denied}");
    let (s, _) = w
        .send(
            "POST",
            &w.doc_uri("edit-claim/revoke"),
            person(w.maintainer),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let rows =
        w.t.json_rows(&format!(
            "SELECT details, actor_user_id FROM forge.audit_log \
             WHERE action = 'document.edit_claim.revoked' AND resource_id = {}",
            w.project
        ))
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["details"]["holder_id"], w.dev.get());
    assert_eq!(rows[0]["actor_user_id"], w.maintainer.get());
    let (s, _) = w
        .send(
            "POST",
            &w.doc_uri("edit-claim/revoke"),
            person(w.maintainer),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn claims_are_for_people_with_push_rights() {
    let w = with_branches().await;
    let (s, r) = w
        .send(
            "PUT",
            &w.doc_uri("edit-claim"),
            person(w.reporter),
            Some(json!({})),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{r}");
    let agent = token(w.dev, forge_auth::AccountKind::Bot);
    let (s, r) = w
        .send("PUT", &w.doc_uri("edit-claim"), agent, Some(json!({})))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{r}");
    let (s, r) = w
        .send(
            "PUT",
            &w.doc_uri("edit-claim"),
            person(w.dev),
            Some(json!({"merge_request_iid": 99})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{r}");
    let (s, r) = w
        .send(
            "PUT",
            &w.doc_uri("edit-claim"),
            person(w.dev),
            Some(json!({"unknown": 1})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    let bad = format!("/api/v1/projects/{}/documents/Q%20P/status", w.project);
    let (s, r) = w.send("GET", &bad, person(w.dev), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
}

#[tokio::test]
async fn opening_a_merge_request_releases_its_authors_claim() {
    let w = with_branches().await;
    let (s, _) = w
        .send(
            "PUT",
            &w.doc_uri("edit-claim"),
            person(w.dev),
            Some(json!({})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, opened) = w.open(w.dev, "a").await;
    assert_eq!(s, StatusCode::CREATED, "{opened}");
    let status = w.status(w.dev).await;
    assert_eq!(status["edit_claim"], Value::Null, "{status}");
    assert_eq!(status["revisions"][0]["merge_request_iid"], opened["iid"]);
}

#[tokio::test]
async fn the_versions_are_the_applied_merges_that_changed_the_document() {
    let w = world().await;
    let c0 = commit(&w.path, None, &[("README.md", "# Web\n")]);
    let c1 = commit(
        &w.path,
        Some(&c0),
        &[("README.md", "# Web\n"), ("docs/QP-07-001.md", &doc("v1"))],
    );
    let c2 = commit(
        &w.path,
        Some(&c1),
        &[
            ("README.md", "# Web 2\n"),
            ("docs/QP-07-001.md", &doc("v1")),
        ],
    );
    let c3 = commit(
        &w.path,
        Some(&c2),
        &[
            ("README.md", "# Web 2\n"),
            ("docs/QP-07-001.md", &doc("v2")),
        ],
    );
    w.branch("main", &c3);
    let mut iids = Vec::new();
    for (name, c) in [("m1", &c1), ("m2", &c2), ("m3", &c3)] {
        w.branch(name, c);
        let (s, mr) = w.open(w.dev, name).await;
        assert_eq!(s, StatusCode::CREATED, "{mr}");
        iids.push(mr["iid"].as_i64().expect("iid"));
    }
    for (i, (before, after)) in [(&c0, &c1), (&c1, &c2), (&c2, &c3)].into_iter().enumerate() {
        // An attempt starts queued and only moves forward (its guard).
        let sql = format!(
            "INSERT INTO forge.merge_request_merges (merge_request_id, project_id, method,              requested_by, credential_kind, credential_id, request_id, head_sha, target_branch,              delete_source_branch) SELECT id, project_id, 'merge_commit', {owner}, 'token', 1,              'req-m{i}', '{after}', 'refs/heads/main', false FROM forge.merge_requests              WHERE project_id = {project} AND iid = {iid};              UPDATE forge.merge_request_merges SET state = 'prepared', expected_old = '{before}',              new_sha = '{after}' WHERE request_id = 'req-m{i}';              UPDATE forge.merge_request_merges SET state = 'executing', executing_at = now()              WHERE request_id = 'req-m{i}';              UPDATE forge.merge_request_merges SET state = 'applied',              finished_at = now() + make_interval(secs => {i}) WHERE request_id = 'req-m{i}';",
            owner = w.owner.get(),
            project = w.project,
            iid = iids[i],
        );
        w.t.exec_superuser(&sql).await;
    }
    let (s, v) = w
        .send("GET", &w.doc_uri("versions"), person(w.reporter), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["path"], "docs/QP-07-001.md");
    assert_eq!(v["current_revision"], 2);
    let got: Vec<(i64, i64)> = v["versions"]
        .as_array()
        .expect("versions")
        .iter()
        .map(|x| {
            (
                x["revision"].as_i64().expect("r"),
                x["merge_request_iid"].as_i64().expect("iid"),
            )
        })
        .collect();
    assert_eq!(got, vec![(1, iids[0]), (2, iids[2])]);
    assert_eq!(v["versions"][1]["commit"], c3);
    assert_eq!(v["incomplete"], Value::Null);

    let missing = format!("/api/v1/projects/{}/documents/QP-99/versions", w.project);
    let (s, r) = w.send("GET", &missing, person(w.reporter), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{r}");
}

/// `files`, committed.
fn commit_files(repo: &std::path::Path, parent: Option<&str>, f: &[(&str, String)]) -> String {
    let borrowed: Vec<(&str, &str)> = f.iter().map(|(p, c)| (*p, c.as_str())).collect();
    commit(repo, parent, &borrowed)
}

/// Review HIGH 1: deriving the documents fails (a blob git cannot read):
/// the merge request is created all the same — the rule fails open.
#[tokio::test]
async fn a_document_that_cannot_be_read_does_not_stop_the_merge_request() {
    let w = with_branches().await;
    let (s, first) = w.open(w.dev, "a").await;
    assert_eq!(s, StatusCode::CREATED, "{first}");
    // `d` changes the document too, but its blob is gone from the store.
    let base = git(&w.path, &["rev-parse", "refs/heads/main"], None, None);
    let d = commit_files(&w.path, Some(&base), &files("# Web\n", "d", &[]));
    w.branch("d", &d);
    let blob = git(
        &w.path,
        &["hash-object", "--stdin"],
        Some(doc("d").as_bytes()),
        None,
    );
    std::fs::remove_file(w.path.join("objects").join(&blob[..2]).join(&blob[2..]))
        .expect("remove the blob");
    let (s, second) = w.open(w.dev2, "d").await;
    assert_eq!(s, StatusCode::CREATED, "{second}");
}

/// Review HIGH 1: a repository without type definitions (`.forge/doctypes/`)
/// holds no document: nothing is derived, nothing refused.
#[tokio::test]
async fn a_repository_without_type_definitions_is_not_checked() {
    let w = world().await;
    let base = commit(&w.path, None, &[("docs/QP-07-001.md", &doc("v1"))]);
    w.branch("main", &base);
    for name in ["x", "y"] {
        let c = commit(&w.path, Some(&base), &[("docs/QP-07-001.md", &doc(name))]);
        w.branch(name, &c);
        let (s, mr) = w.open(w.dev, name).await;
        assert_eq!(s, StatusCode::CREATED, "{mr}");
    }
    assert_eq!(w.status(w.dev).await["revisions"], json!([]));
}

fn confidential_doc() -> String {
    "---\ndoctype: qms.procedure\nid: QP-07-002\ntitle: Secret procedure\nowner: qa\nclassification: confidential\n---\n\n# Purpose\n\nx\n".to_owned()
}

/// Review HIGH 3 (plan 06 D-5): the status of a confidential or unknown
/// document is answered to an AI agent as absent; a person sees it.
#[tokio::test]
async fn an_agent_is_answered_a_confidential_document_status_as_absent() {
    let w = world().await;
    let secret = confidential_doc();
    let base = commit_files(
        &w.path,
        None,
        &files("# Web\n", "v1", &[("docs/QP-07-002.md", secret.as_str())]),
    );
    w.branch("main", &base);
    let agent = person(w.dev).as_agent(true);
    let uri = |id: &str| format!("/api/v1/projects/{}/documents/{id}/status", w.project);
    let (s, body) = w.send("GET", &uri("QP-07-002"), agent.clone(), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code(&body), "FG-REPO-0005");
    let (s, body) = w.send("GET", &uri("QP-99"), agent.clone(), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    let (s, body) = w.send("GET", &uri("QP-07-001"), agent, None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = w.send("GET", &uri("QP-07-002"), person(w.dev), None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}
