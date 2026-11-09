#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The push mirror routes of M3 O2 through the product router against the
//! development database (ADR-0011), with the real loaders and commands: an
//! administrator sets, reads, pushes now and removes a project's mirror;
//! the project's Owner and Developer, a person without a role, an
//! administrator's token without the `admin` scope, an agent's token and a
//! service account are refused; a destination outside the policy or an
//! unusable credential file is `400 FG-INT-0021` with one sentence; the
//! full URL is never readable by the read pool. Principals are inserted as
//! request extensions. Synthetic data only.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::config::MirrorConfig;
use forge_core::mirror::MirrorPolicy;
use forge_core::{ProjectId, RequestId, Role, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::repositories::{mark_ready, MarkReady};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_integ::mirror::PushMirrors;
use forge_web::{AppConfig, AppState, Lookups};
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
        Box::pin(async {
            Err(forge_errors::ForgeError::new(
                forge_errors::ErrorCode::Unauthenticated,
            ))
        })
    }
}

const URL: &str = "https://gitlab.example.invalid/ymma/web.git";

/// A credentials directory: `gitlab` (0600), `loose` (0644), `broken`
/// (0600, no `:`).
fn credentials() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, text, mode) in [
        ("gitlab", "mirror-bot:glpat-synthetic-token\n", 0o600),
        ("loose", "mirror-bot:glpat-synthetic-token\n", 0o644),
        ("broken", "glpat-synthetic-token\n", 0o600),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, text).expect("write");
        // Windows has no mode bits: the product checks them on Unix only
        // (forge-core `config::secret`), so `loose` is refused there only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode");
        }
        #[cfg(not(unix))]
        let _ = mode; // ignore: no mode bits to set
    }
    dir
}

fn config(dir: Option<&Path>) -> MirrorConfig {
    MirrorConfig {
        credentials_dir: dir.map(Path::to_path_buf),
        policy: MirrorPolicy::parse("gitlab.example.invalid", false).expect("policy"),
        ..MirrorConfig::default()
    }
}

fn router(db: &Db, mirror: MirrorConfig) -> axum::Router {
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
    .with_push_mirrors(PushMirrors::new(db.clone(), mirror));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn person(t: &FreshDb, name: &str, admin: bool) -> users::Created {
    let mut new = NewUser::human(
        name,
        &format!("Synthetic {name}"),
        &format!("{name}@example.test"),
    );
    new.is_admin = admin;
    users::create(&t.db, &ctx(&format!("req-user-{name}")), &new)
        .await
        .expect("user")
}

struct Answer {
    status: StatusCode,
    json: Value,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }
}

struct World {
    t: FreshDb,
    creds: tempfile::TempDir,
    admin: UserId,
    owner: UserId,
    dev: UserId,
    outsider: UserId,
    web: ProjectId,
}

impl World {
    async fn call(&self, method: &str, uri: &str, who: Principal, body: Option<Value>) -> Answer {
        self.call_with(config(Some(self.creds.path())), method, uri, who, body)
            .await
    }

    async fn call_with(
        &self,
        mirror: MirrorConfig,
        method: &str,
        uri: &str,
        who: Principal,
        body: Option<Value>,
    ) -> Answer {
        let builder = Request::builder().method(method).uri(uri);
        let mut req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("request");
        req.extensions_mut().insert(who);
        let res = router(&self.t.db, mirror)
            .oneshot(req)
            .await
            .expect("infallible");
        let status = res.status();
        let raw = res
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        Answer {
            status,
            json: serde_json::from_slice(&raw).unwrap_or(Value::Null),
        }
    }

    fn uri(&self) -> String {
        format!("/api/v1/projects/{}/push-mirror", self.web.get())
    }
}

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Session, AccountKind::Human)
}

fn token(user: UserId, admin: bool, scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        user,
        admin,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::Human,
    )
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let admin = person(&t, "mira", true).await.user_id;
    let owner = person(&t, "alice", false).await;
    let dev = person(&t, "dev", false).await.user_id;
    let outsider = person(&t, "outsider", false).await.user_id;
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", owner.namespace_id, "Web", owner.user_id),
    )
    .await
    .expect("web");
    let marked = mark_ready(&t.db, &ctx("req-ready"), web.repository_id)
        .await
        .expect("ready");
    assert!(matches!(marked, MarkReady::Marked { .. }), "{marked:?}");
    projects::add_member(&t.db, &ctx("req-dev"), web.project_id, dev, Role::Developer)
        .await
        .expect("developer");
    World {
        t,
        creds: credentials(),
        admin,
        owner: owner.user_id,
        dev,
        outsider,
        web: web.project_id,
    }
}

fn setting(url: &str, credential: &str) -> Value {
    json!({"url": url, "credential_name": credential})
}

/// The refusal's body: the code, one sentence and the request id, nothing
/// else (CLAUDE.md §3-3).
fn assert_user_facing_only(a: &Answer) {
    let keys: Vec<&str> = a
        .json
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["error_code", "request_id", "user_message"],
        "{}",
        a.json
    );
}

async fn mirror_jobs(w: &World) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM forge.jobs WHERE kind = 'repository.push_mirror'")
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("jobs")
}

#[tokio::test]
async fn an_administrator_sets_reads_pushes_and_removes_a_mirror() {
    let w = world().await;
    let uri = w.uri();
    let admin = || session(w.admin, true);
    let missing = w.call("GET", &uri, admin(), None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.json);
    assert_eq!(missing.code(), "FG-REPO-0005");

    let created = w
        .call("PUT", &uri, admin(), Some(setting(URL, "gitlab")))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    assert_eq!(created.json["url"], URL);
    assert_eq!(created.json["origin"], "https://gitlab.example.invalid:443");
    assert_eq!(created.json["credential_name"], "gitlab");
    assert_eq!(created.json["enabled"], true);
    assert_eq!(created.json["created_by"], w.admin.get());
    assert!(created.json["status"]["pending_since"].is_string());
    assert_eq!(created.json["status"]["failures"], 0);
    assert!(
        !created.json.to_string().contains("glpat"),
        "{}",
        created.json
    );
    // The first push is queued at once.
    assert_eq!(mirror_jobs(&w).await, 1);

    // The same setting again: unchanged, 200; with an admin token too.
    let again = w
        .call(
            "PUT",
            &uri,
            token(w.admin, true, &[Scope::Api, Scope::Admin]),
            Some(setting(URL, "gitlab")),
        )
        .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.json);
    assert_eq!(mirror_jobs(&w).await, 1);
    let read = w
        .call(
            "GET",
            &uri,
            token(w.admin, true, &[Scope::ReadApi, Scope::Admin]),
            None,
        )
        .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.json);
    for key in ["repo_id", "url", "created_at", "updated_at", "status"] {
        assert!(!read.json[key].is_null(), "{key}: {}", read.json);
    }
    for key in [
        "pending_seq",
        "mirrored_seq",
        "failures",
        "pending_since",
        "last_attempt_at",
        "last_success_at",
        "last_error_code",
        "last_reason",
    ] {
        assert!(read.json["status"].get(key).is_some(), "{key}");
    }

    // Push now.
    let synced = w.call("POST", &format!("{uri}/sync"), admin(), None).await;
    assert_eq!(synced.status, StatusCode::ACCEPTED, "{}", synced.json);
    assert_eq!(synced.json, json!({"queued": true}));
    assert_eq!(mirror_jobs(&w).await, 2);

    // Disabled: kept, not pushed; a push now is not found.
    let off = w
        .call(
            "PUT",
            &uri,
            admin(),
            Some(json!({"url": URL, "credential_name": "gitlab", "enabled": false})),
        )
        .await;
    assert_eq!(off.status, StatusCode::OK, "{}", off.json);
    assert_eq!(off.json["enabled"], false);
    assert!(off.json["status"]["pending_since"].is_null());
    let refused = w.call("POST", &format!("{uri}/sync"), admin(), None).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.json);
    assert_eq!(refused.code(), "FG-REPO-0005");

    // Removed.
    let removed = w.call("DELETE", &uri, admin(), None).await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    let gone = w.call("GET", &uri, admin(), None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "{}", gone.json);
    let twice = w.call("DELETE", &uri, admin(), None).await;
    assert_eq!(twice.status, StatusCode::NOT_FOUND, "{}", twice.json);
    let no_sync = w.call("POST", &format!("{uri}/sync"), admin(), None).await;
    assert_eq!(no_sync.status, StatusCode::NOT_FOUND, "{}", no_sync.json);
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_mirror_is_an_administrators_and_a_persons() {
    let w = world().await;
    let uri = w.uri();
    let body = setting(URL, "gitlab");
    let sa_admin = Principal::authenticated(
        w.admin,
        true,
        true,
        Credential::Token(vec![Scope::Api, Scope::Admin]),
        AccountKind::ServiceAccount,
    );
    for (who, status, code) in [
        // The project's Owner and Developer: not theirs to set.
        (
            session(w.owner, false),
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
        (
            token(w.owner, false, &[Scope::Api]),
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
        (session(w.dev, false), StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        // No role on a private project: not found.
        (
            session(w.outsider, false),
            StatusCode::NOT_FOUND,
            "FG-REPO-0005",
        ),
        // An administrator's token without `admin`: no rights, no role.
        (
            token(w.admin, true, &[Scope::Api]),
            StatusCode::NOT_FOUND,
            "FG-REPO-0005",
        ),
        // Reading scope only.
        (
            token(w.admin, true, &[Scope::ReadApi, Scope::Admin]),
            StatusCode::FORBIDDEN,
            "FG-AUTH-0002",
        ),
        // An agent's token and a service account: people only.
        (
            token(w.admin, true, &[Scope::Api, Scope::Admin]).as_agent(true),
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
        (sa_admin, StatusCode::FORBIDDEN, "FG-AUTH-0003"),
    ] {
        let refused = w.call("PUT", &uri, who.clone(), Some(body.clone())).await;
        assert_eq!(refused.status, status, "{}", refused.json);
        assert_eq!(refused.code(), code, "{}", refused.json);
        assert_user_facing_only(&refused);
        let read = w.call("GET", &uri, who, None).await;
        assert!(read.status.is_client_error(), "{}", read.json);
    }
    // Nothing was stored.
    let none = w.call("GET", &uri, session(w.admin, true), None).await;
    assert_eq!(none.status, StatusCode::NOT_FOUND, "{}", none.json);
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_invalid_mirror_target_is_400_int_0021_with_one_sentence() {
    let w = world().await;
    let uri = w.uri();
    for (url, credential) in [
        ("http://gitlab.example.invalid/ymma/web.git", "gitlab"),
        (
            "https://user:pw@gitlab.example.invalid/ymma/web.git",
            "gitlab",
        ),
        (
            "https://gitlab.example.invalid/ymma/web.git?private_token=abc",
            "gitlab",
        ),
        ("https://elsewhere.example.invalid/ymma/web.git", "gitlab"),
        ("ssh://gitlab.example.invalid/ymma/web.git", "gitlab"),
        ("not a url", "gitlab"),
        (URL, "../gitlab"),
        (URL, "Gitlab"),
        (URL, "absent"),
        // Too open (0644): refused on Unix only — Windows has no mode bits.
        #[cfg(unix)]
        (URL, "loose"),
        (URL, "broken"),
    ] {
        let refused = w
            .call(
                "PUT",
                &uri,
                session(w.admin, true),
                Some(setting(url, credential)),
            )
            .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{url} {credential}: {}",
            refused.json
        );
        assert_eq!(refused.code(), "FG-INT-0021", "{url} {credential}");
        assert_user_facing_only(&refused);
        let text = refused.json.to_string();
        for leak in [
            "reason",
            "pw@",
            "private_token",
            "ymma",
            "glpat",
            "elsewhere",
        ] {
            assert!(!text.contains(leak), "{leak}: {text}");
        }
    }
    // Without mirror.credentials_dir nothing can be set.
    let unconfigured = w
        .call_with(
            config(None),
            "PUT",
            &uri,
            session(w.admin, true),
            Some(setting(URL, "gitlab")),
        )
        .await;
    assert_eq!(
        unconfigured.status,
        StatusCode::BAD_REQUEST,
        "{}",
        unconfigured.json
    );
    assert_eq!(unconfigured.code(), "FG-INT-0021");
    // An unknown field is refused, nothing was stored.
    let extra = w
        .call(
            "PUT",
            &uri,
            session(w.admin, true),
            Some(json!({"url": URL, "credential_name": "gitlab", "token": "x"})),
        )
        .await;
    assert_eq!(extra.status, StatusCode::BAD_REQUEST, "{}", extra.json);
    let none = w.call("GET", &uri, session(w.admin, true), None).await;
    assert_eq!(none.status, StatusCode::NOT_FOUND, "{}", none.json);
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_full_url_is_never_in_a_read_pool_read_or_the_audit() {
    let w = world().await;
    let set = w
        .call(
            "PUT",
            &w.uri(),
            session(w.admin, true),
            Some(setting(URL, "gitlab")),
        )
        .await;
    assert_eq!(set.status, StatusCode::CREATED, "{}", set.json);
    let mut su = w.t.superuser().await;
    let details: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM forge.audit_log WHERE action LIKE 'push_mirror.%' ORDER BY id",
    )
    .fetch_all(&mut su)
    .await
    .expect("audit");
    assert_eq!(details.len(), 1, "{details:?}");
    let audited = details[0].to_string();
    assert!(audited.contains("gitlab.example.invalid:443"), "{audited}");
    assert!(!audited.contains("ymma"), "{audited}");
    sqlx::raw_sql("SET ROLE forge_ro")
        .execute(&mut su)
        .await
        .expect("role");
    let state = match sqlx::raw_sql("SELECT url FROM forge.push_mirrors")
        .execute(&mut su)
        .await
    {
        Err(sqlx::Error::Database(e)) => e.code().map(|c| c.into_owned()),
        other => panic!("{other:?}"),
    };
    assert_eq!(state.as_deref(), Some("42501"));
    let origin: String = sqlx::query_scalar("SELECT origin FROM forge.push_mirrors")
        .fetch_one(&mut su)
        .await
        .expect("origin is readable");
    assert_eq!(origin, "https://gitlab.example.invalid:443");
    drop(su);
    w.t.drop_now().await;
}
