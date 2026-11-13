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

use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    PushRecord, PushRecorded, PushRecorder, Pusher, RefPattern, RequestId, Role, Scope, Visibility,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::tokens::{self, NewToken};
use forge_db::users::{self, NewUser};
use forge_db::{protected_refs, repositories, Db};
use forge_errors::ErrorCode;
use forge_git::push_scan::ScanLimits;
use forge_git::{Git, GitConfig, GitStream, StorageRoot};
use forge_web::{
    AppConfig, AppState, BoundedAuthenticator, GitHttpLimits, Lookups, ServerLimits, SmartHttp,
};

/// The database is "down" once the refs are updated: every record fails
/// (`FG-DB-0001`), as the push path would see it.
struct DownRecorder;

impl PushRecorder for DownRecorder {
    fn record_push<'a>(
        &'a self,
        _: &'a Pusher,
        _: &'a PushRecord,
    ) -> LookupFuture<'a, PushRecorded> {
        Box::pin(async {
            Err(forge_errors::ForgeError::new(ErrorCode::DbUnavailable)
                .with_context("reason", "synthetic_outage"))
        })
    }
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

/// How a fixture's server runs.
#[derive(Clone)]
struct Options {
    max_streams: usize,
    acquire_timeout: Duration,
    /// Add a 32 MiB incompressible file (a response larger than the socket
    /// buffers).
    big: bool,
    limits: GitHttpLimits,
    /// Record pushes with a recorder that always fails (the database is
    /// "down" after the ref update; MR4).
    failing_recorder: bool,
    /// Run the push checks (M3 X8) with this test's `forge` binary, as
    /// `forge serve` does.
    push_checks: bool,
    /// Another binary for the hook script (X8 review: `hook_not_run`, the
    /// Windows verbatim path).
    push_check_exe: Option<PathBuf>,
    /// Other scan limits (X8 review: tiny caps).
    push_check_limits: Option<ScanLimits>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_streams: 16,
            acquire_timeout: Duration::from_secs(10),
            big: false,
            limits: GitHttpLimits::default(),
            failing_recorder: false,
            push_checks: true,
            push_check_exe: None,
            push_check_limits: None,
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
    /// Push tokens (`write_repository`): alice (Owner), carol (Developer
    /// member), dave (administrator, not a member; token with `admin`).
    alice_write: String,
    carol_write: String,
    dave_admin: String,
    web: CreatedProject,
    /// An empty, ready repository (alice/fresh; the history import rows).
    fresh: CreatedProject,
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
/// reaches), `alice/pending` (still provisioning) and `alice/fresh` (ready,
/// empty: the history import rows). bob is signed in and
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
    let (t, alice, alice_api, bob, web, bare, pushers, fresh) = rt.block_on(async {
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
        let carol = new_user("carol").await;
        let dave = users::create(
            &t.db,
            &ctx("req-user-dave"),
            &NewUser {
                is_admin: true,
                ..NewUser::human("dave", "dave", "dave@example.test")
            },
        )
        .await
        .expect("admin");
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
        // 2026-11-12: an empty, ready repository (the history import rows).
        let fresh = project("fresh").await;
        let fresh_target = repositories::provision_target(&t.db, fresh.repository_id)
            .await
            .expect("target");
        root.init_bare_or_adopt(&git, fresh.repository_id, fresh_target.token, None)
            .await
            .expect("init fresh");
        let marked = repositories::mark_ready(&t.db, &ctx("req-ready-fresh"), fresh.repository_id)
            .await
            .expect("ready");
        assert!(
            matches!(marked, repositories::MarkReady::Marked { .. }),
            "{marked:?}"
        );
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
        // Push fixtures (MR4): carol is a Developer of alice/web; main and
        // tags v* need a Maintainer.
        projects::add_member(
            &t.db,
            &ctx("req-member-carol"),
            web.project_id,
            carol.user_id,
            Role::Developer,
        )
        .await
        .expect("member");
        for (i, pattern) in ["refs/heads/main", "refs/tags/v*"].into_iter().enumerate() {
            protected_refs::create(
                &t.db,
                &ctx(&format!("req-rule-{i}")),
                web.project_id,
                &RefPattern::parse(pattern).expect("pattern"),
                Role::Maintainer,
            )
            .await
            .expect("rule");
        }
        let write = |user, name: &'static str, scopes: Vec<Scope>| {
            let db = t.db.clone(); // clone: pool handle
            async move { token(&db, user, name, scopes).await }
        };
        let pushers = (
            write(alice.user_id, "alice-write", vec![Scope::WriteRepository]).await,
            write(carol.user_id, "carol-write", vec![Scope::WriteRepository]).await,
            write(
                dave.user_id,
                "dave-admin",
                vec![Scope::WriteRepository, Scope::Admin],
            )
            .await,
        );
        (t, alice_tok, alice_api, bob_tok, web, bare, pushers, fresh)
    });
    let (alice_write, carol_write, dave_admin) = pushers;
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
                projects: Arc::new(db.clone()), // clone: also the push services below
            },
        )
        .with_smart_http({
            let smart = SmartHttp::new(
                git.clone(), // clone: shared pools with the test
                StorageRoot::open(storage.path()).expect("root"),
                opts.limits,
            );
            let smart = if opts.push_checks {
                let exe = opts
                    .push_check_exe
                    .clone() // clone: the options stay readable
                    .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_forge")));
                let smart = smart.with_push_checks(exe).expect("push checks armed");
                match opts.push_check_limits {
                    Some(limits) => smart.with_push_check_limits(limits),
                    None => smart,
                }
            } else {
                smart
            };
            smart
                .with_push(
                    Arc::new(db.clone()),
                    if opts.failing_recorder {
                        Arc::new(DownRecorder) as Arc<dyn PushRecorder>
                    } else {
                        Arc::new(db.clone())
                    },
                )
                .with_history_imports(Arc::new(db.clone())) // clone: pool handle
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
        alice_write,
        carol_write,
        dave_admin,
        web,
        fresh,
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
    // `sparse:oid` would read a blob by id (MR3 review): refused, and a git
    // client asking for it fails.
    let sparse = format!(
        "{}0001{}{}{}0000",
        pkt("command=fetch\n"),
        pkt(&format!("want {}\n", s.tip)),
        pkt(&format!("filter sparse:oid={}\n", s.secret_blob)),
        pkt("done\n")
    );
    let answer = s.http("POST", UPLOAD, Some(&s.alice), &v2, sparse.as_bytes());
    assert_eq!(answer.status, 400);
    assert_eq!(answer.code(), ErrorCode::GitProtocolInvalid.id());
}

#[test]
fn a_sparse_oid_filter_clone_is_refused() {
    let s = server(Options::default());
    let filter = format!("--filter=sparse:oid={}", s.secret_blob);
    let (out, _) = s.clone_into(Some(&s.alice), "alice/web.git", &[&filter]);
    assert!(
        !out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── M1 phase 9 MR4: push (ADR-0008 option B) ─────────────────────────

impl Server {
    /// A fresh clone of `alice/web` (alice's read token) to push from.
    fn work(&self) -> PathBuf {
        let (out, dest) = self.clone_into(Some(&self.alice), "alice/web.git", &[]);
        ok(&out, "clone");
        dest
    }

    /// git in `dir` (no credentials needed: local operations).
    fn local(&self, dir: &Path, args: &[&str]) -> String {
        ok(
            &self
                .git(None)
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git"),
            &format!("{args:?}"),
        )
    }

    /// `git push <url of alice/web> <refspecs>` with `token`.
    fn push(&self, token: &str, dir: &Path, refspecs: &[&str]) -> Output {
        self.push_to(token, dir, "alice/web.git", refspecs)
    }

    fn push_to(&self, token: &str, dir: &Path, path: &str, refspecs: &[&str]) -> Output {
        self.git(Some(token))
            .arg("-C")
            .arg(dir)
            .arg("push")
            .arg(self.url(path))
            .args(refspecs)
            .output()
            .expect("git push")
    }

    /// The bare repository's refs: `name → object id`.
    fn refs(&self) -> std::collections::BTreeMap<String, String> {
        let out = local_git(self.home.path())
            .arg("--git-dir")
            .arg(&self.bare)
            .args(["for-each-ref", "--format=%(refname) %(objectname)"])
            .output()
            .expect("for-each-ref");
        ok(&out, "for-each-ref")
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(r, o)| (r.to_owned(), o.to_owned()))
            .collect()
    }

    /// A new commit on the clone's current branch; its id.
    fn commit(&self, dir: &Path, file: &str, content: &[u8]) -> String {
        std::fs::write(dir.join(file), content).expect("write");
        self.local(dir, &["add", file]);
        self.local(dir, &["commit", "-q", "-m", file]);
        self.local(dir, &["rev-parse", "HEAD"])
    }

    /// `repository.pushed` audit rows and events (as JSON rows).
    fn pushed_rows(&self) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
        let t = self.t.as_ref().expect("db");
        self.rt.block_on(async {
            (
                t.json_rows(
                    "SELECT request_id, details FROM forge.audit_log \
                     WHERE action = 'repository.pushed' ORDER BY id",
                )
                .await,
                t.json_rows(
                    "SELECT payload FROM forge.events \
                     WHERE event_type = 'repository.pushed' ORDER BY id",
                )
                .await,
            )
        })
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn refused(out: &Output, code: ErrorCode) -> String {
    let text = stderr(out);
    assert!(!out.status.success(), "the push must fail: {text}");
    assert!(text.contains(code.id()), "{}: {text}", code.id());
    text
}

#[test]
fn git_push_with_a_read_repository_token_is_refused() {
    let s = server(Options::default());
    let work = s.work();
    s.commit(&work, "new.txt", b"synthetic\n");
    let before = s.refs();
    let out = s.push(&s.alice, &work, &["HEAD:refs/heads/feature"]);
    let text = stderr(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("403"), "{text}");
    // An `api` token is refused the same way, before any body is read.
    let out = s.push(&s.alice_api, &work, &["HEAD:refs/heads/feature"]);
    assert!(!out.status.success(), "{}", stderr(&out));
    // The advertisement itself answers 403 (FG-AUTH-0002).
    let answer = s.http(
        "GET",
        "/alice/web.git/info/refs?service=git-receive-pack",
        Some(&s.alice),
        &[],
        b"",
    );
    assert_eq!(answer.status, 403);
    assert_eq!(answer.code(), ErrorCode::InsufficientScope.id());
    assert_eq!(s.refs(), before);
    assert!(s.pushed_rows().0.is_empty());
}

#[test]
fn push_creates_a_branch_and_a_tag_and_records_it_once() {
    let s = server(Options::default());
    let work = s.work();
    let first = s.commit(&work, "a.txt", b"synthetic a\n");
    let second = s.commit(&work, "b.txt", b"synthetic b\n");
    s.local(&work, &["tag", "release-1"]);
    let out = s.push(
        &s.alice_write,
        &work,
        &["HEAD:refs/heads/feature", "refs/tags/release-1"],
    );
    ok(&out, "push");
    let refs = s.refs();
    assert_eq!(refs.get("refs/heads/feature"), Some(&second));
    assert_eq!(refs.get("refs/tags/release-1"), Some(&second));
    // A fast-forward of main by its Owner (Maintainer protection).
    let out = s.push(&s.alice_write, &work, &["HEAD:main"]);
    ok(&out, "fast-forward main");
    assert_eq!(s.refs().get("refs/heads/main"), Some(&second));

    // Recorded asynchronously, after the answer: wait for both pushes.
    let deadline = Instant::now() + Duration::from_secs(20);
    let (audits, events) = loop {
        let (audits, events) = s.pushed_rows();
        if audits.len() >= 2 && events.len() >= 2 || Instant::now() > deadline {
            break (audits, events);
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(audits.len(), 2, "{audits:?}");
    assert_eq!(events.len(), 2, "{events:?}");
    let created = &events[0]["payload"];
    assert_eq!(created["total_refs"], 2);
    assert_eq!(created["created"], 2);
    let feature = created["refs"]
        .as_array()
        .expect("refs")
        .iter()
        .find(|r| r["ref"] == "refs/heads/feature")
        .expect("feature listed");
    assert_eq!(feature["kind"], "create");
    // The commits the branch brings (newest first), not the history main had.
    assert_eq!(feature["commits"], serde_json::json!([second, first]));
    assert_eq!(feature["total_commits"], 2);
    let ff = &events[1]["payload"]["refs"][0];
    assert_eq!(ff["ref"], "refs/heads/main");
    assert_eq!(ff["kind"], "fast_forward");
    assert_eq!(ff["old"], s.tip);
    assert_eq!(ff["total_commits"], 2);
    assert_eq!(audits[0]["details"]["created"], 2);
}

#[test]
fn a_developer_below_the_protection_level_gets_fg_git_0008() {
    let s = server(Options::default());
    let work = s.work();
    s.commit(&work, "c.txt", b"synthetic c\n");
    let before = s.refs();
    let out = s.push(&s.carol_write, &work, &["HEAD:main"]);
    let text = refused(&out, ErrorCode::ProtectedRefPushNotAllowed);
    assert!(text.contains("request_id"), "{text}");
    assert_eq!(s.refs(), before);
    // An unprotected branch is fine for the Developer.
    ok(
        &s.push(&s.carol_write, &work, &["HEAD:refs/heads/carol"]),
        "push",
    );
}

/// A force push to a protected branch is FG-GIT-0007 for an administrator
/// too: decided by forge when the new commit is already in the repository
/// (a rewind), by git (`receive.denyNonFastForwards`, rewritten to
/// FG-GIT-0007) when it arrives in the pack.
#[test]
fn force_push_to_protected_is_fg_git_0007_even_for_an_admin() {
    let s = server(Options::default());
    let work = s.work();
    let before = s.refs();
    // Rewind main to its parent (the commit exists in the repository).
    let out = s.push(&s.dave_admin, &work, &["+HEAD~1:main"]);
    refused(&out, ErrorCode::ProtectedRefForcePush);
    assert_eq!(s.refs(), before);
    // A rewritten commit that only the pack brings.
    s.local(&work, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let out = s.push(&s.dave_admin, &work, &["+HEAD:main"]);
    let text = refused(&out, ErrorCode::ProtectedRefForcePush);
    assert_eq!(s.refs(), before, "{text}");
    // The administrator passes the minimum role for a fast-forward.
    let fresh = s.work();
    let tip = s.commit(&fresh, "d.txt", b"synthetic d\n");
    ok(
        &s.push(&s.dave_admin, &fresh, &["HEAD:main"]),
        "admin fast-forward",
    );
    assert_eq!(s.refs().get("refs/heads/main"), Some(&tip));
}

/// M3 R6 (ADR-0024 §4.3, §12): a merge rule that forbids direct pushes
/// refuses a fast-forward of `main` for its Owner and for an administrator
/// (FG-GIT-0017, nothing changes); another branch still pushes.
#[test]
fn a_direct_push_to_a_branch_with_the_ban_is_fg_git_0017() {
    let s = server(Options::default());
    let t = s.t.as_ref().expect("db");
    let pid = s.web.project_id.get();
    s.rt.block_on(t.commit_audited(
        ("project", pid),
        &[format!(
            "INSERT INTO forge.protected_ref_merge_rules (protected_ref_id, project_id, \
             direct_push_forbidden) SELECT id, project_id, true FROM forge.protected_refs \
             WHERE project_id = {pid} AND pattern = 'refs/heads/main'"
        )],
    ));
    let work = s.work();
    s.commit(
        &work,
        "ban.txt",
        b"synthetic ban
",
    );
    let before = s.refs();
    for token in [&s.alice_write, &s.dave_admin] {
        let out = s.push(token, &work, &["HEAD:main"]);
        refused(&out, ErrorCode::ProtectedRefDirectPushForbidden);
        assert_eq!(s.refs(), before);
    }
    ok(
        &s.push(&s.alice_write, &work, &["HEAD:refs/heads/side"]),
        "an unprotected branch",
    );
}

#[test]
fn deleting_a_protected_branch_is_fg_git_0009() {
    let s = server(Options::default());
    let work = s.work();
    let before = s.refs();
    let out = s.push(&s.alice_write, &work, &[":main"]);
    refused(&out, ErrorCode::ProtectedRefDeletion);
    assert_eq!(s.refs(), before);
}

#[test]
fn a_reserved_ref_is_fg_git_0010() {
    let s = server(Options::default());
    let work = s.work();
    let before = s.refs();
    let out = s.push(&s.alice_write, &work, &["HEAD:refs/replace/x"]);
    refused(&out, ErrorCode::ReservedRefNamespace);
    let hex = "abcdef0123456789abcdef0123456789abcdef01";
    let out = s.push(&s.alice_write, &work, &[&format!("HEAD:refs/heads/{hex}")]);
    refused(&out, ErrorCode::AmbiguousRefName);
    assert_eq!(s.refs(), before);
}

/// One refused ref refuses the whole push: the allowed branch is not
/// created either, and git was not run.
#[test]
fn a_mixed_push_is_refused_as_a_whole() {
    let s = server(Options::default());
    let work = s.work();
    s.commit(&work, "e.txt", b"synthetic e\n");
    let before = s.refs();
    let out = s.push(&s.alice_write, &work, &["HEAD:refs/heads/ok", ":main"]);
    let text = refused(&out, ErrorCode::ProtectedRefDeletion);
    assert!(text.contains("atomic push failure"), "{text}");
    assert_eq!(s.refs(), before);
    assert!(s.pushed_rows().0.is_empty());
}

#[test]
fn moving_a_protected_tag_is_fg_git_0007() {
    let s = server(Options::default());
    let work = s.work();
    s.local(&work, &["tag", "v1"]);
    ok(
        &s.push(&s.alice_write, &work, &["refs/tags/v1"]),
        "create v1",
    );
    s.commit(&work, "f.txt", b"synthetic f\n");
    s.local(&work, &["tag", "-f", "v1"]);
    let before = s.refs();
    let out = s.push(&s.alice_write, &work, &["+refs/tags/v1"]);
    refused(&out, ErrorCode::ProtectedRefForcePush);
    assert_eq!(s.refs(), before);
}

#[test]
fn push_to_tombstone_is_404() {
    let s = server(Options::default());
    rename_web(&s);
    let (out, work) = s.clone_into(Some(&s.alice), "alice/site.git", &[]);
    ok(&out, "clone the new path");
    s.commit(&work, "g.txt", b"synthetic g\n");
    let before = s.refs();
    let out = s.push_to(
        &s.alice_write,
        &work,
        "alice/web.git",
        &["HEAD:refs/heads/x"],
    );
    let text = stderr(&out);
    assert!(!out.status.success(), "{text}");
    // git reports the 404 of the advertisement as a missing repository.
    assert!(text.contains("not found"), "{text}");
    for (method, path) in [
        ("GET", "/alice/web.git/info/refs?service=git-receive-pack"),
        ("POST", "/alice/web.git/git-receive-pack"),
    ] {
        let answer = s.http(method, path, Some(&s.alice_write), &[], b"0000");
        assert_eq!(answer.status, 404, "{method} {path}");
        assert_eq!(answer.code(), ErrorCode::NotFound.id());
    }
    assert_eq!(s.refs(), before);
    // The current path takes the push.
    ok(
        &s.push_to(
            &s.alice_write,
            &work,
            "alice/site.git",
            &["HEAD:refs/heads/x"],
        ),
        "push to the current path",
    );
}

/// Over `git.http.push_max_bytes` (index-pack's `receive.maxInputSize`):
/// refused, no ref changed. The pack is larger than git's `postBuffer`,
/// so git first sends its probe (`0000`), which forge answers without git.
#[test]
fn an_over_cap_push_is_refused_with_no_ref_changed() {
    let s = server(Options {
        limits: GitHttpLimits {
            push_max_bytes: std::num::NonZeroU64::new(256 * 1024).expect("non-zero"),
            ..GitHttpLimits::default()
        },
        ..Options::default()
    });
    let work = s.work();
    s.commit(&work, "big.bin", &noise(2 * 1024 * 1024));
    let before = s.refs();
    let out = s.push(&s.alice_write, &work, &["HEAD:refs/heads/big"]);
    let text = stderr(&out);
    assert!(!out.status.success(), "{text}");
    assert_eq!(s.refs(), before, "{text}");
    // A small push still works.
    let small = s.work();
    s.commit(&small, "small.txt", b"synthetic\n");
    ok(
        &s.push(&s.alice_write, &small, &["HEAD:refs/heads/small"]),
        "small push",
    );
}

/// `receive.fsckObjects`: a commit with a malformed author line is refused
/// by git, and no ref changes.
#[test]
fn a_corrupt_object_is_refused_by_fsck() {
    let s = server(Options::default());
    let work = s.work();
    let tree = s.local(&work, &["rev-parse", "HEAD^{tree}"]);
    let body = format!(
        "tree {tree}\nparent {}\nauthor no email 1700000000 +0000\ncommitter forge-test <forge-test@example.invalid> 1700000000 +0000\n\nsynthetic\n",
        s.tip
    );
    let path = work.join("bad-commit");
    std::fs::write(&path, body).expect("write");
    let path_arg = path.to_string_lossy().into_owned();
    let bad = s.local(
        &work,
        &[
            "hash-object",
            "-t",
            "commit",
            "--literally",
            "-w",
            &path_arg,
        ],
    );
    s.local(&work, &["update-ref", "refs/heads/bad", &bad]);
    let before = s.refs();
    let out = s.push(&s.alice_write, &work, &["refs/heads/bad"]);
    let text = stderr(&out);
    assert!(!out.status.success(), "{text}");
    assert_eq!(s.refs(), before, "{text}");
    // git's `unpack <error>` text is replaced by forge's code and the
    // request id (git's own words go to the log only).
    assert!(text.contains("FG-GIT-0014"), "{text}");
    assert!(text.contains("request_id"), "{text}");
    assert!(
        !text.contains(&s.storage.path().to_string_lossy().into_owned()),
        "{text}"
    );
}

/// A push that updates a protected branch and another ref: git refuses the
/// rewrite of the protected branch (the new commit only arrives in the
/// pack), and forge made the push atomic, so the other ref is not created
/// either (the review's MEDIUM: no partly applied push).
#[test]
fn a_rewrite_git_refuses_takes_the_whole_push_with_it() {
    let s = server(Options::default());
    let work = s.work();
    s.local(&work, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let before = s.refs();
    let out = s.push(
        &s.alice_write,
        &work,
        &["+HEAD:main", "HEAD:refs/heads/side"],
    );
    let text = refused(&out, ErrorCode::ProtectedRefForcePush);
    assert!(text.contains("atomic"), "{text}");
    assert_eq!(s.refs(), before, "{text}");
}

/// The database is down once git updated the refs: the push succeeds for
/// the client, `forge.git.push_applied` (with the full list) is logged
/// first, then FG-GIT-0012 after the retries — the replay input and the
/// alert, under the same request id.
#[test]
fn a_push_the_database_cannot_record_logs_fg_git_0012_with_its_list() {
    let logs = LogSink::install();
    let s = server(Options {
        failing_recorder: true,
        ..Options::default()
    });
    let work = s.work();
    let tip = s.commit(&work, "h.txt", b"synthetic h\n");
    ok(
        &s.push(&s.alice_write, &work, &["HEAD:refs/heads/down"]),
        "push",
    );
    assert_eq!(s.refs().get("refs/heads/down"), Some(&tip));
    // The sink is the whole process's (the tests of this binary may run as
    // threads) and a finished test's database is dropped while its push is
    // still being recorded, which logs FG-GIT-0012 for *that* push. So the
    // alert is ours only under the request id of the push that created
    // `refs/heads/down` at `tip`.
    let deadline = Instant::now() + Duration::from_secs(30);
    let failed = loop {
        let events = logs.events();
        let ours = events.iter().find(|e| {
            e["event"] == "forge.git.push_applied"
                && forge_core::PushApplied::from_log_line(&e.to_string())
                    .ok()
                    .flatten()
                    .is_some_and(|p| {
                        p.updates
                            .iter()
                            .any(|u| u.name == "refs/heads/down" && u.new == tip)
                    })
        });
        if let Some(id) = ours.map(|e| e["request_id"].clone()) {
            if let Some(i) = events.iter().position(|e| {
                e["error_code"] == ErrorCode::PushRecordFailed.id() && e["request_id"] == id
            }) {
                break (events, i);
            }
        }
        assert!(Instant::now() < deadline, "no FG-GIT-0012: {events:?}");
        std::thread::sleep(Duration::from_millis(200));
    };
    let (events, at) = failed;
    let alert = &events[at];
    let request_id = alert["request_id"].as_str().expect("request id").to_owned();
    let context = alert["context"].as_str().unwrap_or_default();
    assert!(context.contains("attempts=3"), "{context}");
    assert!(context.contains("cause_code=FG-DB-0001"), "{context}");
    let lines: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e["event"] == "forge.git.push_applied" && e["request_id"] == request_id.as_str()
        })
        .map(|(i, _)| i)
        .collect();
    // The minimal line as soon as git exited, then the final one; both
    // before the alert.
    assert_eq!(lines.len(), 2, "{events:?}");
    assert_eq!(events[lines[0]]["stage"], "minimal");
    assert_eq!(events[lines[1]]["stage"], "final");
    let applied = lines[1];
    assert!(applied < at, "push_applied is logged before the alert");
    let minimal = forge_core::PushApplied::from_log_line(&events[lines[0]].to_string())
        .expect("readable")
        .expect("a push line");
    assert_eq!(minimal.updates[0].new, tip);
    let line = events[applied].to_string();
    let parsed = forge_core::PushApplied::from_log_line(&line)
        .expect("readable")
        .expect("a push line");
    assert_eq!(parsed.updates.len(), 1);
    assert_eq!(parsed.updates[0].name, "refs/heads/down");
    assert_eq!(parsed.updates[0].new, tip);
    assert_eq!(parsed.updates[0].commits, [tip]);
    assert!(s.pushed_rows().0.is_empty());
}

/// Captured JSON log lines of the whole process (nextest runs each test in
/// its own process; the server's tasks run on other threads).
#[derive(Clone, Default)]
struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogSink {
    /// The process's sink, installed on first use (several tests read it;
    /// each filters for its own events).
    fn install() -> Self {
        static SINK: std::sync::OnceLock<LogSink> = std::sync::OnceLock::new();
        SINK.get_or_init(|| {
            let sink = Self::default();
            let writer = sink.clone(); // clone: the subscriber writes, the test reads
            let subscriber = tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("one subscriber per process");
            sink
        })
        .clone() // clone: a shared handle
    }

    fn events(&self) -> Vec<serde_json::Value> {
        String::from_utf8_lossy(&self.0.lock().expect("log lock"))
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

/// M3 R1b (ADR-0024 §2, ADR-0007 amendment): merge request version refs
/// are in no advertisement (`ls-remote` over v0 and v2, a clone, the
/// receive-pack advertisement), an object only they reach is refused by id
/// (commit and blob, both wire versions), and a push to them is refused
/// (FG-GIT-0010) with nothing changed. M4 C7: the merge result ref
/// `refs/merge-requests/<iid>/merge` alike.
#[test]
fn hidden_merge_request_refs_are_not_advertised_or_fetchable() {
    let s = server(Options::default());
    for name in [
        "refs/merge-requests/1/v1",
        "refs/merge-requests/1/head",
        "refs/merge-requests/1/merge",
        // M4 C8 (ADR-0030 §3): the merge queue's train ref.
        "refs/merge-queue/1/train",
    ] {
        let out = local_git(s.home.path())
            .arg("--git-dir")
            .arg(&s.bare)
            .args(["update-ref", name, &s.secret_commit])
            .output()
            .expect("update-ref");
        ok(&out, "update-ref");
    }
    for version in ["2", "0"] {
        let out = s
            .git(Some(&s.alice))
            .args(["-c", &format!("protocol.version={version}")])
            .args(["ls-remote"])
            .arg(s.url("alice/web.git"))
            .output()
            .expect("ls-remote");
        let listed = ok(&out, "ls-remote");
        assert!(listed.contains(&s.tip), "v{version}: {listed}");
        assert!(
            !listed.contains("refs/merge-requests"),
            "v{version}: {listed}"
        );
        assert!(!listed.contains("refs/merge-queue"), "v{version}: {listed}");
        assert!(!listed.contains(&s.secret_commit), "v{version}: {listed}");
    }
    let (out, dest) = s.clone_into(Some(&s.alice), "alice/web.git", &["--mirror"]);
    ok(&out, "mirror clone");
    let has = s
        .git(None)
        .arg("--git-dir")
        .arg(&dest)
        .args(["cat-file", "-e", &s.secret_commit])
        .output()
        .expect("cat-file");
    assert!(
        !has.status.success(),
        "the hidden commit arrived with --mirror"
    );
    let advertised = s.http(
        "GET",
        "/alice/web.git/info/refs?service=git-receive-pack",
        Some(&s.alice_write),
        &[],
        &[],
    );
    assert_eq!(advertised.status, 200);
    let text = String::from_utf8_lossy(&advertised.body);
    assert!(!text.contains("refs/merge-requests"), "{text}");
    assert!(!text.contains("refs/merge-queue"), "{text}");
    assert!(!text.contains(&s.secret_commit), "{text}");
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
        assert!(!text.contains("PACK"), "{label}");
    }
    let work = s.work();
    let before = s.refs();
    let out = s.push(&s.alice_write, &work, &["HEAD:refs/merge-requests/1/v2"]);
    refused(&out, ErrorCode::ReservedRefNamespace);
    let out = s.push(&s.dave_admin, &work, &["HEAD:refs/merge-requests/1/head"]);
    refused(&out, ErrorCode::ReservedRefNamespace);
    // M4 C7: nobody writes the merge result ref over a push.
    let out = s.push(&s.dave_admin, &work, &["HEAD:refs/merge-requests/1/merge"]);
    refused(&out, ErrorCode::ReservedRefNamespace);
    assert_eq!(s.refs(), before);
}

// ── M3 X8: push checks in the pre-receive hook ─────────────────────────
//
// The sample key is assembled at run time, so this file never trips the
// checks of the repository that hosts it.

fn synthetic_key() -> Vec<u8> {
    [
        "-----BEGIN OPENSSH PRIV",
        "ATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAAsynthetic\n-----END OPENSSH PRIV",
        "ATE KEY-----\n",
    ]
    .concat()
    .into_bytes()
}

impl Server {
    /// Whether the bare repository has `object`.
    fn has_object(&self, object: &str) -> bool {
        local_git(self.home.path())
            .arg("--git-dir")
            .arg(&self.bare)
            .args(["cat-file", "-e", object])
            .output()
            .expect("cat-file")
            .status
            .success()
    }

    /// The push check runs left in `<git home>/push-checks` once every push
    /// task ended (they are removed after git exits).
    fn wait_no_check_runs(&self) {
        let dir = self.storage.path().join("push-checks");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = std::fs::read_dir(&dir).map_or(0, Iterator::count);
            if left == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "{left} run(s) left in {dir:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn a_push_with_a_private_key_is_refused_with_fg_git_0022() {
    let s = server(Options::default());
    let work = s.work();
    std::fs::create_dir_all(work.join("deploy")).expect("dir");
    let commit = s.commit(&work, "deploy/id_ed25519", &synthetic_key());
    s.commit(&work, "later.txt", b"synthetic later\n");
    let before = s.refs();
    let out = s.push(&s.alice_write, &work, &["HEAD:refs/heads/feature"]);
    let text = refused(&out, ErrorCode::PushSecretDetected);
    assert!(text.contains("deploy/id_ed25519"), "{text}");
    assert!(text.contains(&commit[..12]), "{text}");
    assert!(text.contains("private_key"), "{text}");
    assert!(text.contains("request_id"), "{text}");
    assert!(text.contains("pre-receive hook declined"), "{text}");
    // Never the value.
    assert!(!text.contains("b3BlbnNzaC1rZXkt"), "{text}");
    // Nothing changed, and the quarantined objects are gone.
    assert_eq!(s.refs(), before);
    assert!(!s.has_object(&commit));
    s.wait_no_check_runs();
    assert!(s.pushed_rows().0.is_empty());
}

#[test]
fn a_push_with_a_large_file_is_refused_with_fg_git_0021() {
    let s = server(Options {
        limits: GitHttpLimits {
            push_max_blob_bytes: 64 * 1024,
            ..GitHttpLimits::default()
        },
        ..Options::default()
    });
    let work = s.work();
    let commit = s.commit(&work, "assets.bin", &noise(100 * 1024));
    let before = s.refs();
    let out = s.push(&s.alice_write, &work, &["HEAD:refs/heads/assets"]);
    let text = refused(&out, ErrorCode::PushBlobTooLarge);
    assert!(text.contains("assets.bin"), "{text}");
    assert!(text.contains(&commit[..12]), "{text}");
    assert!(text.contains("102400 bytes"), "{text}");
    assert_eq!(s.refs(), before);
    assert!(!s.has_object(&commit));
    // A file under the limit passes (a fresh clone, without the big one).
    let work = s.work();
    let small = s.commit(&work, "small.bin", &noise(16 * 1024));
    ok(
        &s.push(&s.alice_write, &work, &["HEAD:refs/heads/small"]),
        "push under the limit",
    );
    assert_eq!(s.refs().get("refs/heads/small"), Some(&small));
    s.wait_no_check_runs();
}

#[test]
fn only_new_objects_are_checked_and_a_marked_fixture_passes() {
    let s = server(Options::default());
    // A key already in the repository (pushed straight into the bare
    // repository, as the fixture's seed is): a push on top of it passes.
    let work = s.work();
    s.commit(&work, "legacy.pem", &synthetic_key());
    let bare = s.bare.to_string_lossy().into_owned();
    s.local(&work, &["push", "-q", &bare, "HEAD:refs/heads/legacy"]);
    let on_top = s.commit(&work, "next.txt", b"synthetic next\n");
    ok(
        &s.push(&s.alice_write, &work, &["HEAD:refs/heads/next"]),
        "push on top of an old key",
    );
    assert_eq!(s.refs().get("refs/heads/next"), Some(&on_top));
    // A reviewed synthetic fixture carries the marker.
    let mut fixture = b"# synthetic test key, forge:allow-secret\n".to_vec();
    fixture.extend_from_slice(&synthetic_key());
    let marked = s.commit(&work, "fixture.pem", &fixture);
    ok(
        &s.push(&s.alice_write, &work, &["HEAD:refs/heads/next"]),
        "push of a marked fixture",
    );
    assert_eq!(s.refs().get("refs/heads/next"), Some(&marked));
    s.wait_no_check_runs();
}

// ── X8 review: the size check is complete, the rest of the e2e rows ─────

fn tiny_caps(max_blob_bytes: u64) -> Options {
    let mut limits = ScanLimits::new(max_blob_bytes);
    limits.max_objects = 2;
    Options {
        limits: GitHttpLimits {
            push_max_blob_bytes: max_blob_bytes,
            ..GitHttpLimits::default()
        },
        push_check_limits: Some(limits),
        ..Options::default()
    }
}

/// X8 review (HIGH): padding a push past the secret scan's object cap
/// does not let a large file through — and without one, the incomplete
/// scan is accepted with the FG-GIT-0024 warning.
#[test]
fn a_padded_push_cannot_hide_a_large_file() {
    let s = server(tiny_caps(64 * 1024));
    let work = s.work();
    for i in 0..20 {
        std::fs::write(work.join(format!("pad{i:02}.txt")), format!("pad {i}\n")).expect("pad");
    }
    s.local(&work, &["add", "."]);
    s.local(&work, &["commit", "-q", "-m", "padding"]);
    let padded = s.local(&work, &["rev-parse", "HEAD"]);
    // Only padding: accepted, with the warning.
    let out = s.push(&s.alice_write, &work, &["HEAD:refs/heads/padded"]);
    let text = ok(&out, "padded push");
    let _ = text;
    assert!(
        stderr(&out).contains(ErrorCode::PushCheckIncomplete.id()),
        "{}",
        stderr(&out)
    );
    assert_eq!(s.refs().get("refs/heads/padded"), Some(&padded));
    // Padding and a large file: refused all the same.
    let big = s.commit(&work, "zz-big.bin", &noise(100 * 1024));
    let before = s.refs();
    let out = s.push(&s.alice_write, &work, &["HEAD:refs/heads/padded"]);
    let text = refused(&out, ErrorCode::PushBlobTooLarge);
    assert!(text.contains("zz-big.bin"), "{text}");
    assert!(text.contains(&big[..12]), "{text}");
    assert_eq!(s.refs(), before);
    s.wait_no_check_runs();
}

/// A large blob only an annotated tag names (no tree, no commit) is sized.
#[test]
fn a_tag_of_a_large_blob_is_refused() {
    let s = server(tiny_caps(64 * 1024));
    let work = s.work();
    std::fs::write(work.join("blob.bin"), noise(100 * 1024)).expect("write");
    let blob = s.local(&work, &["hash-object", "-w", "blob.bin"]);
    std::fs::remove_file(work.join("blob.bin")).expect("remove");
    s.local(&work, &["tag", "-a", "-m", "a blob", "blob-tag", &blob]);
    let before = s.refs();
    let out = s.push(&s.alice_write, &work, &["refs/tags/blob-tag"]);
    let text = refused(&out, ErrorCode::PushBlobTooLarge);
    assert!(text.contains(&blob[..12]), "{text}");
    assert_eq!(s.refs(), before);
    assert!(!s.has_object(&blob));
}

/// Pushes that bring nothing new pass: a branch at an existing commit,
/// and a delete.
#[test]
fn a_branch_at_an_existing_commit_and_a_delete_pass() {
    let s = server(Options::default());
    let work = s.work();
    ok(
        &s.push(
            &s.alice_write,
            &work,
            &[&format!("{}:refs/heads/again", s.tip)],
        ),
        "branch at an existing commit",
    );
    assert_eq!(s.refs().get("refs/heads/again"), Some(&s.tip));
    ok(
        &s.push(&s.alice_write, &work, &[":refs/heads/again"]),
        "delete-only push",
    );
    assert!(!s.refs().contains_key("refs/heads/again"));
    s.wait_no_check_runs();
}

/// One good and one bad ref: the hook refuses the whole push (plain and
/// `--atomic`), so the good ref is not created either.
#[test]
fn a_mixed_push_is_refused_whole() {
    let s = server(Options::default());
    let good_work = s.work();
    s.commit(&good_work, "fine.txt", b"synthetic fine\n");
    let good = s.local(&good_work, &["rev-parse", "HEAD"]);
    s.local(&good_work, &["branch", "good", &good]);
    s.commit(&good_work, "id_rsa", &synthetic_key());
    let before = s.refs();
    for atomic in [false, true] {
        let mut args: Vec<&str> = Vec::new();
        if atomic {
            args.push("--atomic");
        }
        args.extend(["refs/heads/good:refs/heads/good", "HEAD:refs/heads/bad"]);
        let out = s.push(&s.alice_write, &good_work, &args);
        let text = refused(&out, ErrorCode::PushSecretDetected);
        assert!(text.contains("id_rsa"), "atomic={atomic}: {text}");
        assert_eq!(s.refs(), before, "atomic={atomic}");
    }
    assert!(!s.has_object(&good));
    s.wait_no_check_runs();
}

/// The `true` binary of this machine's shell tools (git for Windows ships
/// one next to its sh).
fn true_binary() -> PathBuf {
    if cfg!(windows) {
        let out = Command::new("git")
            .arg("--exec-path")
            .output()
            .expect("git --exec-path");
        let exec = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        // <git root>/mingw64/libexec/git-core → <git root>/usr/bin/true.exe
        let root = exec.ancestors().nth(3).expect("git root").to_path_buf();
        let path = root.join("usr").join("bin").join("true.exe");
        assert!(path.is_file(), "{path:?}");
        path
    } else {
        ["/bin/true", "/usr/bin/true"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .expect("true")
    }
}

/// A hook that exits 0 without its result (git ran something else, or
/// skipped it): the refs change, and serve raises FG-GIT-0023
/// `hook_not_run` (an alert) instead of passing it silently.
#[test]
fn a_hook_that_left_no_result_is_hook_not_run() {
    let logs = LogSink::install();
    let s = server(Options {
        push_check_exe: Some(true_binary()),
        ..Options::default()
    });
    let work = s.work();
    let tip = s.commit(&work, "id_rsa", &synthetic_key());
    ok(
        &s.push(&s.alice_write, &work, &["HEAD:refs/heads/unchecked"]),
        "the do-nothing hook accepts",
    );
    assert_eq!(s.refs().get("refs/heads/unchecked"), Some(&tip));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let found = logs.events().into_iter().any(|e| {
            e["error_code"] == ErrorCode::PushCheckFailed.id()
                && e["context"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("reason=hook_not_run")
        });
        if found {
            break;
        }
        assert!(Instant::now() < deadline, "no hook_not_run alert");
        std::thread::sleep(Duration::from_millis(100));
    }
    s.wait_no_check_runs();
}

/// Windows: the binary path as canonicalize gives it (`\\?\D:\…`) still
/// runs the hook (the prefix is stripped for git for Windows' sh).
#[cfg(windows)]
#[test]
fn a_verbatim_windows_exe_path_runs_the_hook() {
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_forge")).expect("canonical");
    assert!(exe.to_string_lossy().starts_with(r"\\?\"), "{exe:?}");
    let s = server(Options {
        push_check_exe: Some(exe),
        ..Options::default()
    });
    let work = s.work();
    s.commit(&work, "id_rsa", &synthetic_key());
    let out = s.push(&s.alice_write, &work, &["HEAD:refs/heads/win"]);
    refused(&out, ErrorCode::PushSecretDetected);
    s.wait_no_check_runs();
}

// ── 2026-11-12: the one-time history import of an empty repository ─────

impl Server {
    /// A new local repository (no remote) with a key added by its first
    /// commit and removed by the second: the key is only in history.
    fn history_with_old_key(&self) -> (PathBuf, String) {
        let work = self.home.path().join(format!("import-{}", rand_suffix()));
        self.local(
            self.home.path(),
            &["init", "-q", "-b", "main", work.to_str().expect("utf-8")],
        );
        std::fs::create_dir_all(work.join("deploy")).expect("dir");
        let old = self.commit(&work, "deploy/id_ed25519", &synthetic_key());
        self.local(&work, &["rm", "-q", "deploy/id_ed25519"]);
        self.local(&work, &["commit", "-q", "-m", "drop the key"]);
        (work, old)
    }

    fn allow_import(&self, repo: forge_core::RepoId, request: &str) {
        let t = self.t.as_ref().expect("db");
        self.rt.block_on(async {
            forge_db::history_imports::allow(&t.db, &ctx(request), repo, Duration::from_secs(3600))
                .await
                .expect("allowed")
        });
    }

    fn import_open(&self, repo: forge_core::RepoId) -> bool {
        let t = self.t.as_ref().expect("db");
        self.rt.block_on(async {
            forge_db::history_imports::import_allowed(&t.db, repo)
                .await
                .expect("read")
        })
    }

    fn audit_rows(&self, action: &str) -> Vec<serde_json::Value> {
        let t = self.t.as_ref().expect("db");
        self.rt.block_on(async {
            t.json_rows(&format!(
                "SELECT request_id, details FROM forge.audit_log \
                 WHERE action = '{action}' ORDER BY id"
            ))
            .await
        })
    }

    /// Wait for the push task to write `action` (it records after the answer).
    fn wait_rows(&self, action: &str, n: usize) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let rows = self.audit_rows(action);
            if rows.len() >= n || Instant::now() > deadline {
                return rows;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[test]
fn a_history_import_accepts_an_old_key_once_and_audits_it() {
    let s = server(Options::default());
    let repo = s.fresh.repository_id;
    s.allow_import(repo, "req-allow-fresh");
    let (work, old) = s.history_with_old_key();
    let out = s.push_to(
        &s.alice_write,
        &work,
        "alice/fresh.git",
        &["HEAD:refs/heads/main"],
    );
    let text = ok(&out, "import push");
    let text = format!("{text}{}", stderr(&out));
    assert!(text.contains("FG-GIT-0022 warning"), "{text}");
    assert!(text.contains("deploy/id_ed25519"), "{text}");
    assert!(!text.contains("b3BlbnNzaC1rZXkt"), "{text}");
    let secrets = s.wait_rows("repository.history_secret_imported", 1);
    assert_eq!(secrets.len(), 1, "{secrets:?}");
    let d = &secrets[0]["details"];
    assert_eq!(d["path"], "deploy/id_ed25519");
    assert_eq!(d["rule"], "private_key");
    assert_eq!(d["commit"], old);
    assert!(!d.to_string().contains("b3BlbnNzaC1rZXkt"), "{d}");
    let imported = s.wait_rows("repository.history_imported", 1);
    assert_eq!(imported[0]["details"]["consumed"], true, "{imported:?}");
    assert!(!s.import_open(repo));
    s.wait_no_check_runs();

    // The second push is checked in full again: a key in a new commit,
    // removed by the next, is refused.
    std::fs::create_dir_all(work.join("keys")).expect("dir");
    s.commit(&work, "keys/id_rsa", &synthetic_key());
    s.local(&work, &["rm", "-q", "keys/id_rsa"]);
    s.local(&work, &["commit", "-q", "-m", "drop it again"]);
    let out = s.push_to(
        &s.alice_write,
        &work,
        "alice/fresh.git",
        &["HEAD:refs/heads/next"],
    );
    refused(&out, ErrorCode::PushSecretDetected);
    s.wait_no_check_runs();
}

#[test]
fn a_key_in_the_tip_is_refused_even_under_the_import_allowance() {
    let s = server(Options::default());
    let repo = s.fresh.repository_id;
    s.allow_import(repo, "req-allow-fresh-tip");
    let work = s.home.path().join("import-tip");
    s.local(
        s.home.path(),
        &["init", "-q", "-b", "main", work.to_str().expect("utf-8")],
    );
    s.commit(&work, "id_ed25519", &synthetic_key());
    let out = s.push_to(
        &s.alice_write,
        &work,
        "alice/fresh.git",
        &["HEAD:refs/heads/main"],
    );
    let text = refused(&out, ErrorCode::PushSecretDetected);
    assert!(text.contains("id_ed25519"), "{text}");
    s.wait_no_check_runs();
    // A refused push does not use the allowance up.
    assert!(s.import_open(repo));
    assert!(s.audit_rows("repository.history_imported").is_empty());
}

#[test]
fn without_an_allowance_or_into_a_repository_with_refs_history_is_checked_in_full() {
    let s = server(Options::default());
    // No allowance: an empty repository is checked like any other.
    let (work, _) = s.history_with_old_key();
    let out = s.push_to(
        &s.alice_write,
        &work,
        "alice/fresh.git",
        &["HEAD:refs/heads/main"],
    );
    refused(&out, ErrorCode::PushSecretDetected);
    s.wait_no_check_runs();
    // An allowance on a repository that has refs (the CLI refuses that;
    // written here straight to the database) changes nothing.
    s.allow_import(s.web.repository_id, "req-allow-web");
    let out = s.push(&s.alice_write, &work, &["HEAD:refs/heads/imported"]);
    refused(&out, ErrorCode::PushSecretDetected);
    s.wait_no_check_runs();
    assert!(s.import_open(s.web.repository_id));
    assert!(s.audit_rows("repository.history_imported").is_empty());
}
