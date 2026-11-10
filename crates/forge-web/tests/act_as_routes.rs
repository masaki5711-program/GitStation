#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Acting for a person through the product router (M3 S2, ADR-0025),
//! against the development database and a real bare repository, with the
//! real token authenticator (forge-db) for the integration's service
//! account and injected sessions for the people: the confirmation, the
//! press, the claim (once), the read and the approve acting for the person
//! (audited as both), single use, a refused action consuming its
//! assertion, and the refusals of the headers and the credential with
//! their codes. Synthetic data only.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal};
use forge_core::act_as::ActAsKeys;
use forge_core::{RequestId, Role, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{Review, VersionRefs};
use forge_web::SessionCredential;
use forge_web::{AppConfig, AppState, GitHttpLimits, Lookups, SmartHttp};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

const KEYS: &str = "1:BwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSY= active";
const STATE: &str = "kanflow-state-0123456789abcdefABCDEF";

/// Who calls: an injected principal (people), or a bearer token (the
/// service account, authenticated by forge-db).
#[derive(Clone)]
enum Who {
    Person(Principal, Option<i64>),
    Bearer(String),
}

fn plumb(repo: &std::path::Path, args: &[&str], input: Option<&[u8]>) -> String {
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

fn commit_on(repo: &std::path::Path, parent: Option<&str>, files: &[(&str, &str)]) -> String {
    let mut listing = String::new();
    for (name, content) in files {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let tree = plumb(repo, &["mktree"], Some(listing.as_bytes()));
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

/// The branch lookups of the review commands, counted (review CRITICAL:
/// an act-as request reaches git only after the decision).
struct Counting {
    inner: Browser,
    calls: Arc<AtomicUsize>,
}

impl forge_review::BranchProbe for Counting {
    fn branch_head<'a>(
        &'a self,
        repo: forge_core::RepoId,
        branch: &'a forge_core::RefName,
        caller: &'a str,
    ) -> forge_core::namespace_resolver::LookupFuture<'a, forge_core::ObjectId> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.branch_head(repo, branch, caller)
    }
}

struct World {
    git_calls: Arc<AtomicUsize>,
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    router: axum::Router,
    /// The project's owner: approves through kanflow.
    olga: UserId,
    olga_session: i64,
    /// The merge request's author (a Developer).
    dave: UserId,
    sa: UserId,
    integration: i64,
    /// kanflow's token: read_api, merge_approve, act_as.
    kanflow: String,
    project: i64,
    head: String,
    /// The target branch's tip (what the confirmation shows).
    base: String,
}

fn session_of(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

async fn send(
    router: axum::Router,
    method: &str,
    uri: &str,
    who: Who,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if let Who::Bearer(token) = &who {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let mut req = req
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .expect("request");
    if let Who::Person(principal, session) = who {
        req.extensions_mut().insert(principal);
        if let Some(id) = session {
            req.extensions_mut().insert(SessionCredential {
                session_id: id,
                public_id: "00000000000000a1".to_owned(),
                csrf_token: "c5rf".repeat(16),
                idle_expires_at: "2099-01-01T00:00:00Z".to_owned(),
                absolute_expires_at: "2099-01-01T00:00:00Z".to_owned(),
            });
        }
    }
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let raw = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

fn code(body: &Value) -> &str {
    body["error_code"].as_str().unwrap_or("")
}

async fn issue_sa_token(t: &FreshDb, sa: UserId, by: UserId, scopes: &[Scope]) -> String {
    forge_db::tokens::issue_for_service_account(
        &t.db,
        &ctx("req-token"),
        &forge_db::tokens::NewToken {
            user: sa,
            name: "kanflow".to_owned(),
            scopes: scopes.to_vec(),
            expires_in: None,
            is_agent: false,
        },
        by,
    )
    .await
    .expect("token")
    .token
    .reveal()
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let mut olga_new = NewUser::human("olga", "Olga", "olga@example.test");
    olga_new.is_admin = true;
    olga_new.employee_number = Some("E-1001".to_owned());
    let olga = users::create(&t.db, &ctx("req-owner"), &olga_new)
        .await
        .expect("owner");
    let dave = users::create(
        &t.db,
        &ctx("req-dev"),
        &NewUser::human("dave", "dave", "dave@example.test"),
    )
    .await
    .expect("dev");
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", olga.namespace_id, "Web", olga.user_id),
    )
    .await
    .expect("web");
    projects::add_member(
        &t.db,
        &ctx("req-member"),
        web.project_id,
        dave.user_id,
        Role::Developer,
    )
    .await
    .expect("member");
    let _ready = repositories::mark_ready(&t.db, &ctx("req-ready"), web.repository_id)
        .await
        .expect("ready");
    let sa = forge_db::service_accounts::create(
        &t.db,
        &ctx("req-sa"),
        &forge_db::service_accounts::NewServiceAccount {
            username: "kanflow-sync".to_owned(),
            display_name: "kanflow".to_owned(),
            owner: olga.user_id,
            purpose: "kanflow approves for the people who confirm in forge".to_owned(),
        },
        olga.user_id,
    )
    .await
    .expect("sa")
    .user_id;
    let integration = forge_db::integrations::create(
        &t.db,
        &ctx("req-int"),
        &forge_db::integrations::NewIntegration {
            kind: forge_core::IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        olga.user_id,
    )
    .await
    .expect("integration");
    let kanflow = issue_sa_token(
        &t,
        sa,
        olga.user_id,
        &[Scope::ReadApi, Scope::MergeApprove, Scope::ActAs],
    )
    .await;
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let base = commit_on(&path, None, &[("README", "base\n")]);
    let head = commit_on(
        &path,
        Some(&base),
        &[("README", "base\n"), ("feature.txt", "f\n")],
    );
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &head], None);
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let git_calls = Arc::new(AtomicUsize::new(0));
    let smart = SmartHttp::new(git.clone(), storage.clone(), GitHttpLimits::default());
    let refs = VersionRefs::new(git.clone(), storage.clone());
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(t.db.clone()),
        Lookups {
            namespaces: Arc::new(t.db.clone()),
            projects: Arc::new(t.db.clone()),
        },
    )
    .with_smart_http(smart)
    .with_hosting(forge_hosting::Hosting::new(t.db.clone()))
    .with_review(
        Review::new(
            t.db.clone(),
            Arc::new(Counting {
                inner: browser.clone(),
                calls: Arc::clone(&git_calls),
            }),
        )
        .with_version_refs(refs.clone())
        .with_review_git(forge_review::approvals::ReviewGit::new(
            git.clone(),
            storage.clone(),
        )),
    )
    .with_act_as(forge_integ::ActAs::new(
        t.db.clone(),
        Some(ActAsKeys::parse(KEYS).expect("keys")),
    ));
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    let olga_session = forge_db::sessions::issue(
        &t.db,
        &ctx("req-session"),
        &forge_db::sessions::NewSession {
            user: olga.user_id,
            idle: Duration::from_secs(3600),
            absolute: Duration::from_secs(8 * 3600),
            oidc: None,
        },
    )
    .await
    .expect("session")
    .session_id;
    let p = web.project_id.get();
    // Bound through the route (the decided command is forge-hosting's).
    let (status, bound) = send(
        router.clone(),
        "PUT",
        &format!(
            "/api/v1/projects/{p}/integration-bindings/{}",
            integration.get()
        ),
        Who::Person(session_of(olga.user_id), None),
        Some(json!({"external_project_id": "KF-1"})),
        &[],
    )
    .await;
    assert!(status.is_success(), "{bound}");
    let (status, opened) = send(
        router.clone(),
        "POST",
        &format!("/api/v1/projects/{p}/merge-requests"),
        Who::Person(session_of(dave.user_id), None),
        Some(json!({"title": "Feature", "source_branch": "feature", "target_branch": "main"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    let mr: i64 = t
        .json_rows("SELECT id FROM forge.merge_requests")
        .await
        .first()
        .and_then(|r| r["id"].as_i64())
        .expect("merge request id");
    forge_review::refresh(
        &t.db,
        &refs,
        forge_review::RefreshRequest {
            merge_request: forge_core::MergeRequestId::new(mr),
            pushed_by: None,
        },
        &RequestId::parse("req-worker").expect("id"),
    )
    .await
    .expect("refresh");
    World {
        git_calls,
        t,
        _home: home,
        _root: root,
        router,
        olga: olga.user_id,
        olga_session,
        dave: dave.user_id,
        sa,
        integration: integration.get(),
        kanflow,
        project: p,
        head,
        base,
    }
}

impl World {
    fn mr(&self, tail: &str) -> String {
        format!("/api/v1/projects/{}/merge-requests/1{tail}", self.project)
    }

    fn olga_by_session(&self) -> Who {
        Who::Person(session_of(self.olga), Some(self.olga_session))
    }

    fn kanflow(&self) -> Who {
        Who::Bearer(self.kanflow.clone())
    }

    fn act_as(&self, user: UserId) -> String {
        format!("user:{}", user.get())
    }

    /// The person confirms `intent` in forge; kanflow claims it.
    async fn confirmed(&self, intent: &str, state: &str) -> String {
        let (status, body) = send(
            self.router.clone(),
            "POST",
            &format!(
                "/-/confirm/projects/{}/merge-requests/1/assertions",
                self.project
            ),
            self.olga_by_session(),
            Some(json!({
                "intent": intent,
                "sha": self.head,
                "target_sha": self.base,
                "integration": self.integration,
                "state": state,
            })),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, claimed) = send(
            self.router.clone(),
            "POST",
            &format!("/api/v1/integrations/{}/act-as/claim", self.integration),
            self.kanflow(),
            Some(json!({ "state": state })),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{claimed}");
        assert_eq!(claimed["sub"]["user_id"], self.olga.get());
        claimed["assertion"].as_str().expect("assertion").to_owned()
    }

    async fn approve_as(&self, person: UserId, assertion: &str, sha: &str) -> (StatusCode, Value) {
        let act_as = self.act_as(person);
        send(
            self.router.clone(),
            "POST",
            &self.mr("/approve"),
            self.kanflow(),
            Some(json!({ "sha": sha })),
            &[("forge-act-as", &act_as), ("forge-assertion", assertion)],
        )
        .await
    }

    async fn assertion_state(&self, public_id: &str) -> String {
        self.t
            .json_rows(&format!(
                "SELECT state, reject_reason FROM forge.act_as_assertions \
                 WHERE public_id = '{public_id}'"
            ))
            .await
            .first()
            .map(|r| r["state"].as_str().unwrap_or("").to_owned())
            .unwrap_or_default()
    }
}

fn public_id(assertion: &str) -> String {
    assertion[4..20].to_owned()
}

/// ADR-0025 §5: the confirmation shows forge's facts; the press writes a
/// pending assertion; kanflow claims it once and approves as the person;
/// the audit names both; the assertion cannot be used again.
#[tokio::test]
async fn an_approval_through_kanflow_is_confirmed_claimed_and_used_once() {
    let w = world().await;
    let (status, view) = send(
        w.router.clone(),
        "GET",
        &format!(
            "{}?intent=approve&sha={}&integration={}",
            w.mr("/act-as-confirmation"),
            w.head,
            w.integration
        ),
        w.olga_by_session(),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["sha_matches"], true);
    assert_eq!(view["head_sha"], w.head);
    assert_eq!(view["intent"], "approve");
    assert_eq!(view["integration_kind"], "kanflow");
    assert_eq!(view["integration_name"], "kanflow-sync");
    let assertion = w.confirmed("approve", STATE).await;
    assert!(assertion.starts_with("fga_"));
    // A second claim of the same state returns nothing.
    let (status, again) = send(
        w.router.clone(),
        "POST",
        &format!("/api/v1/integrations/{}/act-as/claim", w.integration),
        w.kanflow(),
        Some(json!({ "state": STATE })),
        &[],
    )
    .await;
    assert_eq!(
        (status, code(&again)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0020")
    );
    assert!(again.get("assertion").is_none());
    // Reading acting for the person, without an assertion.
    let act_as = w.act_as(w.olga);
    let (status, seen) = send(
        w.router.clone(),
        "GET",
        &w.mr("/status"),
        w.kanflow(),
        None,
        &[("forge-act-as", &act_as)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{seen}");
    assert_eq!(
        seen["viewer"]["act_as"]["can_approve"]["allowed"], true,
        "{seen}"
    );
    assert_eq!(seen["viewer"]["act_as"]["requires_assertion"], true);
    assert_eq!(seen["viewer"]["can_approve"], false);
    let (status, approved) = w.approve_as(w.olga, &assertion, &w.head).await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["outcome"], "approved");
    assert_eq!(w.assertion_state(&public_id(&assertion)).await, "consumed");
    let audit =
        w.t.json_rows(
            "SELECT action, actor_kind, actor_user_id, actor_via_id, via_assertion_id, \
             act_as_claimed FROM forge.audit_log WHERE action = 'merge_request.approved'",
        )
        .await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["actor_kind"], "user_via_service_account");
    assert_eq!(audit[0]["actor_user_id"], w.olga.get());
    assert_eq!(audit[0]["actor_via_id"], w.sa.get());
    assert_eq!(audit[0]["via_assertion_id"], public_id(&assertion));
    // The claimed person is a keyed HMAC, never the header's value.
    let claimed = audit[0]["act_as_claimed"].as_str().unwrap_or_default();
    assert_eq!(claimed.len(), 64, "{claimed}");
    assert!(!claimed.contains("user:"), "{claimed}");
    let origin =
        w.t.json_rows("SELECT credential_origin FROM forge.merge_request_approvals")
            .await;
    assert_eq!(origin[0]["credential_origin"], "act_as_assertion");
    // Single use.
    let (status, reused) = w.approve_as(w.olga, &assertion, &w.head).await;
    assert_eq!(
        (status, code(&reused)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0020"),
        "{reused}"
    );
    w.t.drop_now().await;
}

/// ADR-0025 §5.3: a verified assertion that the action did not consume
/// (here: the body names another sha) is rejected, never usable again.
#[tokio::test]
async fn assertion_is_consumed_when_the_action_fails() {
    let w = world().await;
    let assertion = w.confirmed("approve", STATE).await;
    let (status, refused) = w.approve_as(w.olga, &assertion, &"a".repeat(40)).await;
    assert_eq!(
        (status, code(&refused)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0020"),
        "{refused}"
    );
    assert_eq!(w.assertion_state(&public_id(&assertion)).await, "rejected");
    let (status, later) = w.approve_as(w.olga, &assertion, &w.head).await;
    assert_eq!(
        (status, code(&later)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0020"),
        "{later}"
    );
    let approvals =
        w.t.json_rows("SELECT count(*) AS n FROM forge.merge_request_approvals")
            .await;
    assert_eq!(approvals[0]["n"], 0);
    w.t.drop_now().await;
}

/// Review of ADR-0025 (2026-11-09, security LOW b): a request cut off after
/// its assertion was verified (the client went away while the body was
/// still being read) does not leave the assertion `claimed` for 5 minutes:
/// the drop guard rejects it (`request_cancelled`), and it cannot be used.
#[tokio::test]
async fn a_request_cut_off_while_its_assertion_is_being_rejected_still_rejects_it() {
    // Review of the follow-ups (2026-11-10, MEDIUM): the route answered
    // (a malformed body, 400: the assertion is not used), `finish` is
    // rejecting it and is cut off while waiting on the database — the
    // guard must still be armed and reject it.
    let w = world().await;
    let assertion = w
        .confirmed("approve", "kanflow-state-fin-0123456789abcdef")
        .await;
    let id = public_id(&assertion);
    // Hold the assertion's row: finish's UPDATE waits on it.
    let held =
        w.t.hold_audited(
            ("project", w.project),
            &[format!(
                "SELECT 1 FROM forge.act_as_assertions WHERE public_id = '{id}' FOR UPDATE"
            )],
        )
        .await;
    let act_as = w.act_as(w.olga);
    let req = Request::builder()
        .method("POST")
        .uri(w.mr("/approve"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", w.kanflow))
        .header("forge-act-as", &act_as)
        .header("forge-assertion", &assertion)
        .body(Body::from("{\"sha\": 5}"))
        .expect("request");
    let request = tokio::spawn(w.router.clone().oneshot(req));
    w.t.until_waiting("SET state = 'rejected'").await;
    request.abort();
    assert!(request.await.is_err(), "cut off inside finish");
    drop(held); // rollback: the waiting statements go on
    let mut state = String::new();
    for _ in 0..200 {
        state = w.assertion_state(&id).await;
        if state != "claimed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state, "rejected");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_request_cut_off_after_the_verification_rejects_its_assertion() {
    let w = world().await;
    let assertion = w
        .confirmed("approve", "kanflow-state-cut-0123456789abcdef")
        .await;
    let act_as = w.act_as(w.olga);
    // A body that never ends: the route waits for it after the
    // authentication (and the act-as verification) admitted the request.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(1);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    tx.send(Ok(axum::body::Bytes::from_static(b"{\"sha\":")))
        .await
        .expect("first chunk");
    let req = Request::builder()
        .method("POST")
        .uri(w.mr("/approve"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", w.kanflow))
        .header("forge-act-as", &act_as)
        .header("forge-assertion", &assertion)
        .body(Body::from_stream(stream))
        .expect("request");
    let cut = tokio::time::timeout(Duration::from_millis(500), w.router.clone().oneshot(req)).await;
    assert!(
        cut.is_err(),
        "the request must still be waiting for its body"
    );
    drop(tx);
    let id = public_id(&assertion);
    let mut state = String::new();
    for _ in 0..100 {
        state = w.assertion_state(&id).await;
        if state != "claimed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state, "rejected");
    let rows =
        w.t.json_rows(&format!(
            "SELECT reject_reason FROM forge.act_as_assertions WHERE public_id = '{id}'"
        ))
        .await;
    assert_eq!(rows[0]["reject_reason"], "request_cancelled");
    let (status, later) = w.approve_as(w.olga, &assertion, &w.head).await;
    assert_eq!(
        (status, code(&later)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0020"),
        "{later}"
    );
    w.t.drop_now().await;
}

/// ADR-0025 §4: the merge request's author approving through kanflow is
/// FG-REVIEW-0013 (the person's normal arm).
#[tokio::test]
async fn act_as_self_approval_is_refused_over_http() {
    let w = world().await;
    let assertion = w
        .confirmed("approve", "kanflow-state-self-0123456789abcdef")
        .await;
    // The assertion is olga's: naming dave is wrong_user, never dave.
    let (status, body) = w.approve_as(w.dave, &assertion, &w.head).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0020"),
        "{body}"
    );
    w.t.drop_now().await;
}

/// ADR-0025 §2: only a service account's `act_as` token acts for a person
/// — a session, a person's token, an agent's token are refused, and the
/// header is never ignored.
#[tokio::test]
async fn act_as_header_needs_a_service_account_token() {
    let w = world().await;
    let act_as = w.act_as(w.olga);
    let headers = [("forge-act-as", act_as.as_str())];
    let uri = format!("/api/v1/projects/{}", w.project);
    for (who, reason) in [
        (Who::Person(session_of(w.dave), None), "session"),
        (
            Who::Person(
                Principal::authenticated(
                    w.dave,
                    false,
                    true,
                    Credential::Token(vec![Scope::ReadApi, Scope::ActAs]),
                    AccountKind::Human,
                ),
                None,
            ),
            "person token",
        ),
    ] {
        let (status, body) = send(w.router.clone(), "GET", &uri, who, None, &headers).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
            "{reason}: {body}"
        );
    }
    let agent = Principal::authenticated(
        w.sa,
        false,
        true,
        Credential::Token(vec![Scope::ReadApi, Scope::ActAs]),
        AccountKind::ServiceAccount,
    )
    .as_agent(true);
    let (status, body) = send(
        w.router.clone(),
        "GET",
        &uri,
        Who::Person(agent, None),
        None,
        &headers,
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{body}"
    );
    // kanflow's own token reads acting for olga.
    let (status, body) = send(w.router.clone(), "GET", &uri, w.kanflow(), None, &headers).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn act_as_needs_the_act_as_scope() {
    let w = world().await;
    let reader = issue_sa_token(&w.t, w.sa, w.olga, &[Scope::ReadApi]).await;
    let act_as = w.act_as(w.olga);
    let (status, body) = send(
        w.router.clone(),
        "GET",
        &format!("/api/v1/projects/{}", w.project),
        Who::Bearer(reader),
        None,
        &[("forge-act-as", &act_as)],
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
        "{body}"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn two_act_as_headers_are_refused() {
    let w = world().await;
    let a = w.act_as(w.olga);
    let b = w.act_as(w.dave);
    let (status, body) = send(
        w.router.clone(),
        "GET",
        &format!("/api/v1/projects/{}", w.project),
        w.kanflow(),
        None,
        &[("forge-act-as", &a), ("forge-act-as", &b)],
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-INT-0006"),
        "{body}"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn two_assertion_headers_are_refused() {
    let w = world().await;
    let a = w.act_as(w.olga);
    let fake = format!("fga_00000000000000aa_{}", "ab".repeat(32));
    let (status, body) = send(
        w.router.clone(),
        "POST",
        &w.mr("/approve"),
        w.kanflow(),
        Some(json!({ "sha": w.head })),
        &[
            ("forge-act-as", &a),
            ("forge-assertion", &fake),
            ("forge-assertion", &fake),
        ],
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0020"),
        "{body}"
    );
    // An assertion without Forge-Act-As: unexpected.
    let (status, body) = send(
        w.router.clone(),
        "POST",
        &w.mr("/approve"),
        w.kanflow(),
        Some(json!({ "sha": w.head })),
        &[("forge-assertion", &fake)],
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0020"),
        "{body}"
    );
    w.t.drop_now().await;
}

/// ADR-0025 §1: a disabled person, an unknown one, an e-mail address and
/// the service account itself get the same answer (FG-INT-0006); only the
/// log says why.
#[tokio::test]
async fn act_as_disabled_target_is_int_0006() {
    let w = world().await;
    let disabled = users::disable(&w.t.db, &ctx("req-disable"), w.dave)
        .await
        .expect("disable");
    assert!(matches!(disabled, users::DisableOutcome::Disabled));
    let uri = format!("/api/v1/projects/{}", w.project);
    let mut bodies = Vec::new();
    for value in [
        w.act_as(w.dave),
        "user:999999".to_owned(),
        "user:dave@example.test".to_owned(),
        w.act_as(w.sa),
        "employee:E-9999".to_owned(),
    ] {
        let (status, mut body) = send(
            w.router.clone(),
            "GET",
            &uri,
            w.kanflow(),
            None,
            &[("forge-act-as", &value)],
        )
        .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "FG-INT-0006"),
            "{value}: {body}"
        );
        body["request_id"] = Value::Null;
        bodies.push(body);
    }
    assert!(bodies.windows(2).all(|w| w[0] == w[1]), "{bodies:?}");
    // The employee number of an active person resolves.
    let (status, body) = send(
        w.router.clone(),
        "GET",
        &uri,
        w.kanflow(),
        None,
        &[("forge-act-as", "employee:E-1001")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    w.t.drop_now().await;
}

/// ADR-0025 §4: merging (and every other action outside the allowlist) is
/// refused acting for a person, before anything is looked up.
#[tokio::test]
async fn act_as_merge_is_not_allowed_over_http() {
    let w = world().await;
    let act_as = w.act_as(w.olga);
    let (status, body) = send(
        w.router.clone(),
        "POST",
        &w.mr("/merge"),
        w.kanflow(),
        Some(json!({ "sha": w.head })),
        &[("forge-act-as", &act_as)],
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{body}"
    );
    w.t.drop_now().await;
}

impl World {
    fn git_calls(&self) -> usize {
        self.git_calls.load(Ordering::SeqCst)
    }
}

/// Review (authz CRITICAL): an act-as read never reads git for the
/// assertion's tip, and a change reaches git only after everything but the
/// tip was decided — not for an unbound project, not for another person.
#[tokio::test]
async fn act_as_never_reaches_git_before_the_decision() {
    let w = world().await;
    let assertion = w.confirmed("approve", STATE).await;
    let act_as = w.act_as(w.olga);
    let before = w.git_calls();
    for uri in [
        format!("/api/v1/projects/{}", w.project),
        format!("/api/v1/projects/{}/merge-requests", w.project),
        w.mr(""),
    ] {
        let (status, body) = send(
            w.router.clone(),
            "GET",
            &uri,
            w.kanflow(),
            None,
            &[("forge-act-as", &act_as)],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
    assert_eq!(w.git_calls(), before, "an act-as read reads no tip");
    // Another person named with olga's assertion: refused before git.
    let (status, body) = w.approve_as(w.dave, &assertion, &w.head).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0020"),
        "{body}"
    );
    // Unbound since the confirmation: 404 before git.
    let assertion = w
        .confirmed("approve", "kanflow-state-unbound-0123456789")
        .await;
    let (status, unbound) = send(
        w.router.clone(),
        "DELETE",
        &format!(
            "/api/v1/projects/{}/integration-bindings/{}",
            w.project, w.integration
        ),
        Who::Person(session_of(w.olga), None),
        None,
        &[],
    )
    .await;
    assert!(status.is_success(), "{unbound}");
    let before = w.git_calls();
    let (status, body) = w.approve_as(w.olga, &assertion, &w.head).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        "{body}"
    );
    assert_eq!(w.git_calls(), before, "an unbound project reaches no git");
    w.t.drop_now().await;
}

/// Review (authz HIGH): the read routes whose loaders add no act-as facts
/// are refused acting for a person (ADR-0025 §4 does not list them), never
/// served as the service account or as the person.
#[tokio::test]
async fn act_as_on_routes_without_the_facts_is_refused() {
    let w = world().await;
    let act_as = w.act_as(w.olga);
    for tail in ["approval-rules", "merge-settings", "integration-bindings"] {
        let uri = format!("/api/v1/projects/{}/{tail}", w.project);
        let (status, body) = send(
            w.router.clone(),
            "GET",
            &uri,
            w.kanflow(),
            None,
            &[("forge-act-as", &act_as)],
        )
        .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
            "{uri}: {body}"
        );
    }
    w.t.drop_now().await;
}

/// Review (authz MEDIUM): git Smart HTTP never ignores Forge-Act-As.
#[tokio::test]
async fn act_as_on_smart_http_is_refused() {
    let w = world().await;
    let act_as = w.act_as(w.olga);
    let (status, body) = send(
        w.router.clone(),
        "GET",
        "/olga/web.git/info/refs?service=git-upload-pack",
        w.kanflow(),
        None,
        &[("forge-act-as", &act_as)],
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{body}"
    );
    w.t.drop_now().await;
}

/// Re-review HIGH: the route's action decides, not the assertion's
/// intent. An unapprove assertion sent to the approve route (with the head
/// as `sha`) and an approve assertion sent to the unapprove route are both
/// refused `wrong_action` before any git read, and stay unusable.
#[tokio::test]
async fn a_mismatched_route_reaches_no_git_and_is_wrong_action() {
    let w = world().await;
    let act_as = w.act_as(w.olga);
    for (intent, tail, body, state) in [
        (
            "unapprove",
            "/approve",
            Some(json!({ "sha": w.head })),
            "kanflow-state-mismatch-approve-0123",
        ),
        (
            "approve",
            "/unapprove",
            None,
            "kanflow-state-mismatch-unapprove-01",
        ),
    ] {
        let assertion = w.confirmed(intent, state).await;
        let before = w.git_calls();
        let (status, answer) = send(
            w.router.clone(),
            "POST",
            &w.mr(tail),
            w.kanflow(),
            body,
            &[("forge-act-as", &act_as), ("forge-assertion", &assertion)],
        )
        .await;
        assert_eq!(
            (status, code(&answer)),
            (StatusCode::FORBIDDEN, "FG-AUTH-0020"),
            "{tail}: {answer}"
        );
        assert_eq!(
            w.git_calls(),
            before,
            "{tail}: a mismatched route reaches no git"
        );
        assert_eq!(w.assertion_state(&public_id(&assertion)).await, "rejected");
    }
    w.t.drop_now().await;
}

/// U3b review: the press carries the target tip the screen showed; a tip that
/// moved meanwhile (or a missing / malformed one) writes nothing.
#[tokio::test]
async fn a_press_with_another_target_tip_is_refused_as_stale() {
    let w = world().await;
    let uri = format!(
        "/-/confirm/projects/{}/merge-requests/1/assertions",
        w.project
    );
    for (what, tip) in [
        ("moved", json!("0".repeat(40))),
        ("malformed", json!("zz")),
        ("missing", Value::Null),
    ] {
        let mut body = json!({
            "intent": "approve", "sha": w.head,
            "integration": w.integration, "state": STATE,
        });
        if !tip.is_null() {
            body["target_sha"] = tip;
        }
        let (status, answer) = send(
            w.router.clone(),
            "POST",
            &uri,
            w.olga_by_session(),
            Some(body),
            &[],
        )
        .await;
        assert_ne!(status, StatusCode::CREATED, "{what}: {answer}");
        if what == "moved" {
            assert_eq!(status, StatusCode::CONFLICT, "{what}: {answer}");
        }
    }
    // Nothing was written: the same state is still free for the right press.
    let assertion = w.confirmed("approve", STATE).await;
    assert!(assertion.starts_with("fga_"));
}
