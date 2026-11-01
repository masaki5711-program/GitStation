#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly (rust/testing.md)
//! Smart HTTP fetch with the real `git` client (M1 phase 9 MR3, ADR-0007,
//! ADR-0015 "git", ADR-0019 §7): the product router and middleware, served
//! on 127.0.0.1:0 the way `forge serve` wires them (the database as token
//! store and resolver, forge-git for the repositories), against the
//! development database (ADR-0011). Credentials reach git through an inline
//! `credential.helper` (the token is the Basic password); every repository
//! and token is synthetic.
//!
//! Also the ADR-0007 option (a) rows: an object no ref reaches (a deleted
//! branch's commit and blob) cannot be fetched by id, over v0 and v2.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::{RequestId, Scope, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::tokens::{self, NewToken};
use forge_db::users::{self, NewUser};
use forge_db::{repositories, Db};
use forge_errors::ErrorCode;
use forge_git::{Git, GitConfig, GitStream, StorageRoot};
use forge_web::{
    AppConfig, AppState, BoundedAuthenticator, GitHttpLimits, Lookups, ServerLimits, SmartHttp,
};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

/// How a fixture's server runs.
#[derive(Clone, Copy)]
struct Options {
    max_streams: usize,
    acquire_timeout: Duration,
    /// Add a 32 MiB incompressible file (a response larger than the socket
    /// buffers).
    big: bool,
    limits: GitHttpLimits,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_streams: 16,
            acquire_timeout: Duration::from_secs(10),
            big: false,
            limits: GitHttpLimits::default(),
        }
    }
}

struct Server {
    rt: tokio::runtime::Runtime,
    t: Option<FreshDb>,
    addr: SocketAddr,
    storage: tempfile::TempDir,
    home: tempfile::TempDir,
    git: Git,
    bare: PathBuf,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    alice: String,
    alice_api: String,
    bob: String,
    web: CreatedProject,
    tip: String,
    secret_commit: String,
    secret_blob: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(()); // ignore: the server may have stopped already
        }
        if let Some(t) = self.t.take() {
            self.rt.block_on(t.drop_now());
        }
    }
}

async fn token(db: &Db, user: forge_core::UserId, name: &str, scopes: Vec<Scope>) -> String {
    let new = NewToken {
        user,
        name: name.to_owned(),
        scopes,
        expires_in: None,
        is_agent: false,
    };
    tokens::issue(db, &ctx(&format!("req-tok-{name}")), &new)
        .await
        .expect("token")
        .token
        .reveal()
}

/// A deterministic, incompressible byte stream (xorshift; synthetic data).
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// A local git (setup and client) with an isolated HOME and no prompts.
fn local_git(home: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid");
    cmd
}

fn ok(out: &Output, what: &str) -> String {
    assert!(
        out.status.success(),
        "{what}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// alice owns the private project `alice/web` (ready, two commits on
/// `main`; a deleted branch `secret` left a commit and a blob no ref
/// reaches) and `alice/pending` (still provisioning). bob is signed in and
/// not a member. Tokens: alice `read_repository`, alice `api`, bob
/// `read_repository`.
fn server(opts: Options) -> Server {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let storage = tempfile::tempdir().expect("storage");
    let home = tempfile::tempdir().expect("client home");
    let root = StorageRoot::open(storage.path()).expect("storage root");
    let mut cfg = GitConfig::new("git", root.path());
    cfg.max_streams = opts.max_streams;
    cfg.max_streams_per_principal = opts.max_streams.min(4);
    cfg.acquire_timeout = opts.acquire_timeout;
    let git = Git::new(cfg);
    let (t, alice, alice_api, bob, web, bare) = rt.block_on(async {
        let t = FreshDb::migrated().await;
        let new_user = |name: &'static str| {
            let db = t.db.clone(); // clone: pool handle
            async move {
                users::create(
                    &db,
                    &ctx(&format!("req-user-{name}")),
                    &NewUser::human(name, name, &format!("{name}@example.test")),
                )
                .await
                .expect("user")
            }
        };
        let alice = new_user("alice").await;
        let bob = new_user("bob").await;
        let project = |segment: &'static str| {
            let db = t.db.clone(); // clone: pool handle
            async move {
                projects::create(
                    &db,
                    &ctx(&format!("req-project-{segment}")),
                    &NewProject {
                        visibility: Visibility::Private,
                        ..NewProject::new(segment, alice.namespace_id, "Synthetic", alice.user_id)
                    },
                )
                .await
                .expect("project")
            }
        };
        let web = project("web").await;
        let _pending = project("pending").await;
        let target = repositories::provision_target(&t.db, web.repository_id)
            .await
            .expect("target");
        let bare = root
            .init_bare_or_adopt(&git, web.repository_id, target.token, None)
            .await
            .expect("init")
            .into_path();
        let marked = repositories::mark_ready(&t.db, &ctx("req-ready-web"), web.repository_id)
            .await
            .expect("ready");
        assert!(
            matches!(marked, repositories::MarkReady::Marked { .. }),
            "{marked:?}"
        );
        let alice_tok = token(
            &t.db,
            alice.user_id,
            "alice-read",
            vec![Scope::ReadRepository],
        )
        .await;
        let alice_api = token(&t.db, alice.user_id, "alice-api", vec![Scope::Api]).await;
        let bob_tok = token(&t.db, bob.user_id, "bob-read", vec![Scope::ReadRepository]).await;
        (t, alice_tok, alice_api, bob_tok, web, bare)
    });
    // Content, pushed straight into the bare repository (not through forge).
    let work = home.path().join("seed");
    ok(
        &local_git(home.path())
            .args(["init", "-q", "-b", "main"])
            .arg(&work)
            .output()
            .expect("init"),
        "init",
    );
    let in_work = |args: &[&str]| {
        ok(
            &local_git(home.path())
                .arg("-C")
                .arg(&work)
                .args(args)
                .output()
                .expect("git"),
            &format!("{args:?}"),
        )
    };
    std::fs::write(work.join("README"), "first synthetic line\n").expect("write");
    in_work(&["add", "README"]);
    in_work(&["commit", "-q", "-m", "one"]);
    std::fs::write(work.join("README"), "hello, synthetic world\n").expect("write");
    if opts.big {
        std::fs::write(work.join("big.bin"), noise(32 * 1024 * 1024)).expect("write");
        in_work(&["add", "big.bin"]);
    }
    in_work(&["add", "README"]);
    in_work(&["commit", "-q", "-m", "two"]);
    let tip = in_work(&["rev-parse", "HEAD"]);
    in_work(&["checkout", "-q", "-b", "secret"]);
    std::fs::write(work.join("secret.txt"), "synthetic secret\n").expect("write");
    in_work(&["add", "secret.txt"]);
    in_work(&["commit", "-q", "-m", "oops"]);
    let secret_commit = in_work(&["rev-parse", "HEAD"]);
    let secret_blob = in_work(&["rev-parse", "HEAD:secret.txt"]);
    let bare_arg = bare.to_string_lossy().into_owned();
    in_work(&["push", "-q", &bare_arg, "main", "secret"]);
    // The branch is deleted (as by a force push); its objects stay.
    ok(
        &local_git(home.path())
            .arg("--git-dir")
            .arg(&bare)
            .args(["update-ref", "-d", "refs/heads/secret"])
            .output()
            .expect("update-ref"),
        "delete branch",
    );

    let (addr, shutdown) = rt.block_on(async {
        let db = t.db.clone(); // clone: pool handle
        let state = AppState::new(
            "test",
            vec![Arc::new(db.clone())],
            Arc::new(BoundedAuthenticator::new(Arc::new(db.clone()), 2)),
            Lookups {
                namespaces: Arc::new(db.clone()),
                projects: Arc::new(db),
            },
        )
        .with_smart_http(SmartHttp {
            git: git.clone(), // clone: shared pools with the test
            storage: StorageRoot::open(storage.path()).expect("root"),
            limits: opts.limits,
        });
        let router = forge_web::app(state, AppConfig::default()).expect("router");
        let listener = forge_web::server::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let stop = async move {
                let _ = rx.await; // ignore: a dropped sender also stops
            };
            forge_web::server::serve(listener, router, ServerLimits::default(), stop)
                .await
                .expect("serve");
        });
        (addr, tx)
    });
    Server {
        rt,
        t: Some(t),
        addr,
        storage,
        home,
        git,
        bare,
        shutdown: Some(shutdown),
        alice,
        alice_api,
        bob,
        web,
        tip,
        secret_commit,
        secret_blob,
    }
}

/// An HTTP answer read off the socket.
struct Answer {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Vec<String> {
        self.headers
            .lines()
            .filter_map(|l| l.split_once(':'))
            .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_owned())
            .collect()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("json body")
    }

    fn code(&self) -> String {
        self.json()["error_code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// The body without its request id (identical denials).
    fn without_request_id(&self) -> (u16, String) {
        let mut v = self.json();
        v["request_id"] = serde_json::Value::Null;
        (self.status, v.to_string())
    }
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("http://{}/{path}", self.addr)
    }

    /// git with `token` as the Basic password (inline credential helper).
    fn git(&self, token: Option<&str>) -> Command {
        let mut cmd = local_git(self.home.path());
        cmd.args(["-c", "credential.helper="]);
        if let Some(token) = token {
            cmd.arg("-c").arg(format!(
                "credential.helper=!f() {{ echo username=git; echo password={token}; }}; f"
            ));
        }
        cmd
    }

    fn clone_into(&self, token: Option<&str>, path: &str, extra: &[&str]) -> (Output, PathBuf) {
        let dest = self.home.path().join(format!("clone-{}", rand_suffix()));
        let out = self
            .git(token)
            .arg("clone")
            .args(extra)
            .arg(self.url(path))
            .arg(&dest)
            .output()
            .expect("git clone");
        (out, dest)
    }

    /// One raw HTTP/1.1 request (`Connection: close`), the whole answer.
    fn http(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> Answer {
        let mut s = TcpStream::connect(self.addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(30)))
            .expect("timeout");
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.addr,
            body.len()
        );
        if let Some(token) = token {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        for (k, v) in extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).expect("write head");
        s.write_all(body).expect("write body");
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).expect("read");
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("end of headers");
        let headers = String::from_utf8_lossy(&raw[..split]).into_owned();
        let status = headers
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("status");
        let mut body = raw[split + 4..].to_vec();
        if headers
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            body = dechunk(&body);
        }
        Answer {
            status,
            headers,
            body,
        }
    }
}

fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = raw.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size = usize::from_str_radix(std::str::from_utf8(&raw[..eol]).expect("size"), 16)
            .expect("hex size");
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&raw[eol + 2..eol + 2 + size]);
        raw = &raw[eol + 2 + size + 2..];
    }
}

fn rand_suffix() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

/// A pkt-line.
fn pkt(s: &str) -> String {
    format!("{:04x}{s}", s.len() + 4)
}

fn v0_request(want: &str) -> Vec<u8> {
    format!(
        "{}0000{}",
        pkt(&format!("want {want} side-band-64k ofs-delta\n")),
        pkt("done\n")
    )
    .into_bytes()
}

fn v2_request(want: &str) -> Vec<u8> {
    format!(
        "{}{}0001{}{}{}0000",
        pkt("command=fetch\n"),
        pkt("object-format=sha1\n"),
        pkt("no-progress\n"),
        pkt(&format!("want {want}\n")),
        pkt("done\n")
    )
    .into_bytes()
}

const INFO_REFS: &str = "/alice/web.git/info/refs?service=git-upload-pack";
const UPLOAD: &str = "/alice/web.git/git-upload-pack";
const REQUEST_TYPE: (&str, &str) = ("Content-Type", "application/x-git-upload-pack-request");

#[test]
fn git_clone_with_a_token_in_basic_auth_succeeds() {
    let s = server(Options::default());
    let (out, dest) = s.clone_into(Some(&s.alice), "alice/web.git", &[]);
    ok(&out, "clone");
    let readme = std::fs::read_to_string(dest.join("README")).expect("README");
    assert_eq!(readme, "hello, synthetic world\n");
    let head = ok(
        &s.git(None)
            .arg("-C")
            .arg(&dest)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git"),
        "rev-parse",
    );
    assert_eq!(head, s.tip);
    // The deleted branch is not advertised.
    let refs = ok(
        &s.git(None)
            .arg("-C")
            .arg(&dest)
            .args(["branch", "-r"])
            .output()
            .expect("git"),
        "branch",
    );
    assert!(!refs.contains("secret"), "{refs}");
    // A fetch afterwards (nothing new) works too.
    ok(
        &s.git(Some(&s.alice))
            .arg("-C")
            .arg(&dest)
            .arg("fetch")
            .output()
            .expect("git"),
        "fetch",
    );
    // The advertisement headers (HEAD answers them without starting git).
    let head = s.http("HEAD", INFO_REFS, Some(&s.alice), &[], b"");
    assert_eq!(head.status, 200);
    assert_eq!(
        head.header("content-type"),
        ["application/x-git-upload-pack-advertisement"]
    );
    assert_eq!(head.header("cache-control"), ["private, no-store"]);
    assert_eq!(head.header("vary"), ["Authorization"]);
    assert!(head.body.is_empty());
}

#[test]
fn git_without_credentials_gets_www_authenticate_basic() {
    let s = server(Options::default());
    let answer = s.http("GET", INFO_REFS, None, &[], b"");
    assert_eq!(answer.status, 401);
    assert_eq!(answer.code(), ErrorCode::Unauthenticated.id());
    let challenges = answer.header("www-authenticate");
    assert!(
        challenges
            .iter()
            .any(|c| c.starts_with("Basic realm=\"GitStation\"")),
        "{challenges:?}"
    );
    // The same 401 for a path that does not exist (nothing is looked up).
    let absent = s.http(
        "GET",
        "/alice/nothing.git/info/refs?service=git-upload-pack",
        None,
        &[],
        b"",
    );
    assert_eq!(absent.without_request_id(), answer.without_request_id());
    let (out, _) = s.clone_into(None, "alice/web.git", &[]);
    assert!(!out.status.success());
}

#[test]
fn fetch_uses_protocol_v2() {
    let s = server(Options::default());
    let dest = s.home.path().join("v2");
    let out = s
        .git(Some(&s.alice))
        .args(["-c", "protocol.version=2", "clone"])
        .arg(s.url("alice/web.git"))
        .arg(&dest)
        .env("GIT_TRACE_PACKET", "1")
        .output()
        .expect("clone v2");
    ok(&out, "clone v2");
    let trace = String::from_utf8_lossy(&out.stderr);
    assert!(trace.contains("git< version 2"), "{trace}");
    assert!(trace.contains("> command=fetch"), "{trace}");
    // A v0 client works too.
    let dest = s.home.path().join("v0");
    let out = s
        .git(Some(&s.alice))
        .args(["-c", "protocol.version=0", "clone"])
        .arg(s.url("alice/web.git"))
        .arg(&dest)
        .env("GIT_TRACE_PACKET", "1")
        .output()
        .expect("clone v0");
    ok(&out, "clone v0");
    let trace = String::from_utf8_lossy(&out.stderr);
    assert!(!trace.contains("git< version 2"), "{trace}");
    assert!(dest.join("README").exists());
}

#[test]
fn a_shallow_clone_has_one_commit() {
    let s = server(Options::default());
    let (out, dest) = s.clone_into(Some(&s.alice), "alice/web.git", &["--depth", "1"]);
    ok(&out, "shallow clone");
    let count = ok(
        &s.git(None)
            .arg("-C")
            .arg(&dest)
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .expect("git"),
        "count",
    );
    assert_eq!(count, "1");
}

/// `--filter=blob:none`: the checkout and an old blob are fetched lazily,
/// by id (wants of blobs, which the reachability check walks).
#[test]
fn a_partial_clone_fetches_blobs_on_demand() {
    let s = server(Options::default());
    let (out, dest) = s.clone_into(Some(&s.alice), "alice/web.git", &["--filter=blob:none"]);
    ok(&out, "partial clone");
    assert_eq!(
        std::fs::read_to_string(dest.join("README")).expect("README"),
        "hello, synthetic world\n"
    );
    let old = ok(
        &s.git(Some(&s.alice))
            .arg("-C")
            .arg(&dest)
            .args(["cat-file", "-p", "HEAD~1:README"])
            .output()
            .expect("git"),
        "lazy fetch of an old blob",
    );
    assert_eq!(old, "first synthetic line");
}

#[test]
fn an_api_scope_token_is_refused() {
    let s = server(Options::default());
    let answer = s.http("GET", INFO_REFS, Some(&s.alice_api), &[], b"");
    assert_eq!(answer.status, 403);
    assert_eq!(answer.code(), ErrorCode::InsufficientScope.id());
    let (out, _) = s.clone_into(Some(&s.alice_api), "alice/web.git", &[]);
    assert!(!out.status.success());
}

#[test]
fn a_private_repository_is_the_same_404_for_a_non_member_as_an_absent_one() {
    let s = server(Options::default());
    let hidden = s.http("GET", INFO_REFS, Some(&s.bob), &[], b"");
    let absent = s.http(
        "GET",
        "/alice/nothing.git/info/refs?service=git-upload-pack",
        Some(&s.bob),
        &[],
        b"",
    );
    assert_eq!(hidden.status, 404);
    assert_eq!(hidden.code(), ErrorCode::NotFound.id());
    assert_eq!(hidden.without_request_id(), absent.without_request_id());
    let post_hidden = s.http(
        "POST",
        UPLOAD,
        Some(&s.bob),
        &[REQUEST_TYPE],
        &v0_request(&s.tip),
    );
    let post_absent = s.http(
        "POST",
        "/alice/nothing.git/git-upload-pack",
        Some(&s.bob),
        &[REQUEST_TYPE],
        &v0_request(&s.tip),
    );
    assert_eq!(
        post_hidden.without_request_id(),
        post_absent.without_request_id()
    );
    assert_eq!(post_hidden.status, 404);
}

/// A project still provisioning answers FG-REPO-0016 (503) — only to a
/// caller who may read it; anyone else gets the absent 404.
#[test]
fn a_provisioning_repository_is_repository_not_ready() {
    let s = server(Options::default());
    let path = "/alice/pending.git/info/refs?service=git-upload-pack";
    let answer = s.http("GET", path, Some(&s.alice), &[], b"");
    assert_eq!(answer.status, 503);
    assert_eq!(answer.code(), ErrorCode::RepositoryNotReady.id());
    let outsider = s.http("GET", path, Some(&s.bob), &[], b"");
    assert_eq!(outsider.status, 404);
}

/// Rename `alice/web` → `alice/site` with a `moved` tombstone (D5 has no
/// command yet: written with its audit row, as the route tests do).
fn rename_web(s: &Server) {
    let t = s.t.as_ref().expect("db");
    let (pid, ns) = (s.web.project_id.get(), s.web.namespace_id.get());
    s.rt.block_on(t.commit_audited(
        ("project", pid),
        &[
            format!(
                "UPDATE forge.namespaces SET segment = 'site', full_path = 'alice/site' WHERE id = {ns}"
            ),
            format!(
                "INSERT INTO forge.namespace_tombstones (old_path, kind, namespace_id, cause_type, \
                 cause_id) VALUES ('alice/web', 'moved', {ns}, 'project', {pid})"
            ),
        ],
    ));
}

/// A renamed project's old URL: the advertisement answers `302` to the
/// current path (only after `authorize()` allowed), git follows it and
/// warns — when its first request carries the credentials (token in the
/// URL). With a credential helper git sees the `302` only on its retry
/// after the `401` and keeps POSTing to the old path, which forge serves
/// in place for an allowed reader (ADR-0015 amendment, MR3): the clone
/// works either way. Never for an outsider, never for a push.
#[test]
fn clone_after_rename_follows_302_with_warning() {
    let s = server(Options::default());
    rename_web(&s);
    let moved = s.http("GET", INFO_REFS, Some(&s.alice), &[], b"");
    assert_eq!(moved.status, 302);
    assert_eq!(
        moved.header("location"),
        ["/alice/site.git/info/refs?service=git-upload-pack"]
    );
    assert_eq!(moved.header("cache-control"), ["no-store"]);
    // An outsider gets the absent 404, for the GET and the POST.
    assert_eq!(s.http("GET", INFO_REFS, Some(&s.bob), &[], b"").status, 404);
    let outsider = s.http(
        "POST",
        UPLOAD,
        Some(&s.bob),
        &[REQUEST_TYPE],
        &v0_request(&s.tip),
    );
    assert_eq!(outsider.status, 404);
    // Credentials on the first request: a real redirect, with git's warning.
    let dest = s.home.path().join("renamed-url");
    let url = format!("http://git:{}@{}/alice/web.git", s.alice, s.addr);
    let out = s
        .git(None)
        .arg("clone")
        .arg(&url)
        .arg(&dest)
        .output()
        .expect("clone");
    ok(&out, "clone through the old path, token in the URL");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("warning: redirecting to"), "{stderr}");
    assert!(stderr.contains("alice/site.git"), "{stderr}");
    assert!(dest.join("README").exists());
    // A credential helper: the POST at the old path is served.
    let (out, dest) = s.clone_into(Some(&s.alice), "alice/web.git", &[]);
    ok(&out, "clone through the old path, credential helper");
    assert!(dest.join("README").exists());
}

#[test]
fn a_deleted_path_is_the_absent_404() {
    let s = server(Options::default());
    let t = s.t.as_ref().expect("db");
    s.rt.block_on(
        t.commit_audited(
            ("project", s.web.project_id.get()),
            &[
                "INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, cause_id) \
           VALUES ('alice/old', 'deleted', 'project', 1)"
                    .to_owned(),
            ],
        ),
    );
    let deleted = s.http(
        "GET",
        "/alice/old.git/info/refs?service=git-upload-pack",
        Some(&s.alice),
        &[],
        b"",
    );
    let absent = s.http(
        "GET",
        "/alice/nothing.git/info/refs?service=git-upload-pack",
        Some(&s.alice),
        &[],
        b"",
    );
    assert_eq!(deleted.status, 404);
    assert_eq!(deleted.without_request_id(), absent.without_request_id());
}

#[test]
fn dumb_http_is_not_served() {
    let s = server(Options::default());
    for path in [
        "/alice/web.git/HEAD",
        "/alice/web.git/objects/info/packs",
        "/alice/web.git/info/refs/heads/main",
    ] {
        let answer = s.http("GET", path, Some(&s.alice), &[], b"");
        assert_eq!(answer.status, 404, "{path}");
        assert_eq!(answer.code(), ErrorCode::RouteNotFound.id(), "{path}");
    }
    // `info/refs` without `service` (a dumb client's first request).
    let answer = s.http("GET", "/alice/web.git/info/refs", Some(&s.alice), &[], b"");
    assert_eq!(answer.status, 400);
    assert_eq!(answer.code(), ErrorCode::SmartHttpServiceUnsupported.id());
    let dest = s.home.path().join("dumb");
    let out = s
        .git(Some(&s.alice))
        .arg("clone")
        .arg(s.url("alice/web.git"))
        .arg(&dest)
        .env("GIT_SMART_HTTP", "0")
        .output()
        .expect("dumb clone");
    assert!(!out.status.success());
}

/// ADR-0007 option (a): the deleted branch's commit and blob cannot be
/// fetched by id — not by git over v2 or v0, not by hand-written v0 / v2
/// requests; a reachable want still gets its pack.
#[test]
fn unreferenced_object_cannot_be_fetched_by_id() {
    let s = server(Options::default());
    let (out, dest) = s.clone_into(Some(&s.alice), "alice/web.git", &[]);
    ok(&out, "clone");
    for version in ["2", "0"] {
        let out = s
            .git(Some(&s.alice))
            .args(["-c", &format!("protocol.version={version}")])
            .arg("-C")
            .arg(&dest)
            .args(["fetch", "origin", &s.secret_commit])
            .output()
            .expect("fetch by id");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "v{version}: {stderr}");
        let has = s
            .git(None)
            .arg("-C")
            .arg(&dest)
            .args(["cat-file", "-e", &s.secret_commit])
            .output()
            .expect("cat-file");
        assert!(!has.status.success(), "v{version}: the object arrived");
        if version == "2" {
            assert!(stderr.contains("remote error"), "{stderr}");
        }
    }
    for (label, body, protocol) in [
        ("v0 commit", v0_request(&s.secret_commit), None),
        ("v0 blob", v0_request(&s.secret_blob), None),
        ("v2 commit", v2_request(&s.secret_commit), Some("version=2")),
        ("v2 blob", v2_request(&s.secret_blob), Some("version=2")),
    ] {
        let mut headers = vec![REQUEST_TYPE];
        if let Some(p) = protocol {
            headers.push(("Git-Protocol", p));
        }
        let answer = s.http("POST", UPLOAD, Some(&s.alice), &headers, &body);
        assert_eq!(answer.status, 200, "{label}");
        let text = String::from_utf8_lossy(&answer.body);
        assert!(
            text.get(4..).is_some_and(|t| t.starts_with("ERR ")),
            "{label}: {text}"
        );
        assert!(text.contains("request_id"), "{label}: {text}");
        assert!(!text.contains("PACK"), "{label}");
    }
    let reachable = s.http(
        "POST",
        UPLOAD,
        Some(&s.alice),
        &[REQUEST_TYPE, ("Git-Protocol", "version=2")],
        &v2_request(&s.tip),
    );
    assert_eq!(reachable.status, 200);
    assert!(
        reachable.body.windows(4).any(|w| w == b"PACK"),
        "a reachable want gets its pack"
    );
    // A wrong Content-Type is refused before anything runs.
    let wrong = s.http(
        "POST",
        UPLOAD,
        Some(&s.alice),
        &[("Content-Type", "text/plain")],
        &v0_request(&s.tip),
    );
    assert_eq!(wrong.status, 400);
    assert_eq!(wrong.code(), ErrorCode::GitProtocolInvalid.id());
}

/// The processes whose command line or working directory is `repo`
/// (upload-pack and its pack-objects).
#[cfg(target_os = "linux")]
fn git_processes_in(repo: &Path) -> Vec<String> {
    let repo = std::fs::canonicalize(repo).expect("canonical");
    let needle = repo.to_string_lossy().into_owned();
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").expect("/proc") {
        let Ok(entry) = entry else { continue };
        let dir = entry.path();
        let cmdline = std::fs::read(dir.join("cmdline")).unwrap_or_default();
        let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        let cwd = std::fs::read_link(dir.join("cwd")).ok();
        let in_repo = cwd.is_some_and(|c| c == repo) || cmdline.contains(&needle);
        if in_repo && cmdline.contains("git") {
            found.push(cmdline);
        }
    }
    found
}

/// The client goes away in the middle of a large clone: git and
/// pack-objects are killed and reaped, and the only stream slot is free
/// again (the next clone succeeds).
#[cfg(target_os = "linux")]
#[test]
fn client_disconnect_mid_clone_reaps_git() {
    let s = server(Options {
        max_streams: 1,
        big: true,
        ..Options::default()
    });
    let mut sock = TcpStream::connect(s.addr).expect("connect");
    let body = v0_request(&s.tip);
    let head = format!(
        "POST {UPLOAD} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n",
        s.addr,
        s.alice,
        REQUEST_TYPE.1,
        body.len()
    );
    sock.write_all(head.as_bytes()).expect("head");
    sock.write_all(&body).expect("body");
    let mut first = vec![0; 4096];
    sock.read_exact(&mut first).expect("the response started");
    assert!(
        first.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&first)
    );
    assert!(
        !git_processes_in(&s.bare).is_empty(),
        "git is still sending the pack"
    );
    drop(sock);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = git_processes_in(&s.bare);
        if left.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "still running: {left:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let (out, _) = s.clone_into(Some(&s.alice), "alice/web.git", &["--depth", "1"]);
    ok(&out, "the slot is free again");
}

/// Every stream slot is taken: the advertisement waits `acquire_timeout`,
/// then answers 503 FG-GIT-0004; once the slot is free it is served.
#[test]
fn stream_slots_exhausted_is_503() {
    let s = server(Options {
        max_streams: 1,
        acquire_timeout: Duration::from_millis(300),
        ..Options::default()
    });
    let holder: GitStream = s.rt.block_on(async {
        s.git
            .upload_pack(
                &StorageRoot::open(s.storage.path()).expect("root"),
                s.web.repository_id,
                forge_git::smart_http::Phase::Rpc,
                None,
            )
            .expect("command")
            .spawn_stream("user:holder")
            .await
            .expect("hold the only slot")
    });
    let busy = s.http("GET", INFO_REFS, Some(&s.alice), &[], b"");
    assert_eq!(busy.status, 503);
    assert_eq!(busy.code(), ErrorCode::GitBusy.id());
    assert!(
        busy.header("www-authenticate").is_empty(),
        "never a challenge on 503"
    );
    let err = s.rt.block_on(holder.abort_client_disconnected());
    assert_eq!(err.code(), ErrorCode::GitClientDisconnected);
    let served = s.http("GET", INFO_REFS, Some(&s.alice), &[], b"");
    assert_eq!(served.status, 200);
}

/// The request body limits of `POST git-upload-pack` (`git.http.*`): gzip
/// is decoded (and capped after decoding), other encodings are refused,
/// and a body over the limit is FG-GIT-0013 before anything runs.
#[test]
fn upload_pack_request_body_limits() {
    let s = server(Options {
        limits: GitHttpLimits {
            fetch_request_max_bytes: 4096,
            ..GitHttpLimits::default()
        },
        ..Options::default()
    });
    let v2 = [REQUEST_TYPE, ("Git-Protocol", "version=2")];
    let gzip = |raw: &[u8]| {
        use flate2::write::GzEncoder;
        let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(raw).expect("gzip");
        enc.finish().expect("gzip")
    };
    let mut headers = v2.to_vec();
    headers.push(("Content-Encoding", "gzip"));
    let packed = s.http(
        "POST",
        UPLOAD,
        Some(&s.alice),
        &headers,
        &gzip(&v2_request(&s.tip)),
    );
    assert_eq!(packed.status, 200);
    assert!(
        packed.body.windows(4).any(|w| w == b"PACK"),
        "gzip body decoded"
    );
    // Over the limit raw, and over it only once decompressed.
    let mut huge = v2_request(&s.tip);
    huge.extend(std::iter::repeat_n(b'0', 8192));
    let raw = s.http("POST", UPLOAD, Some(&s.alice), &v2, &huge);
    assert_eq!(raw.status, 413);
    assert_eq!(raw.code(), ErrorCode::GitRequestTooLarge.id());
    let bomb = s.http("POST", UPLOAD, Some(&s.alice), &headers, &gzip(&huge));
    assert_eq!(bomb.status, 413);
    assert_eq!(bomb.code(), ErrorCode::GitRequestTooLarge.id());
    let mut br = v2.to_vec();
    br.push(("Content-Encoding", "br"));
    let refused = s.http("POST", UPLOAD, Some(&s.alice), &br, &v2_request(&s.tip));
    assert_eq!(refused.status, 400);
    assert_eq!(refused.code(), ErrorCode::GitProtocolInvalid.id());
    // A v2 command forge does not serve (object-info answers by id).
    let info = format!(
        "{}0001{}0000",
        pkt("command=object-info\n"),
        pkt(&format!("oid {}\n", s.secret_blob))
    );
    let answer = s.http("POST", UPLOAD, Some(&s.alice), &v2, info.as_bytes());
    assert_eq!(answer.status, 400);
    assert_eq!(answer.code(), ErrorCode::GitProtocolInvalid.id());
}
