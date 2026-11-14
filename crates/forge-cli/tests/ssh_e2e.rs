#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly (rust/testing.md)
//! The SSH git transport with the real `git` and OpenSSH `ssh` clients
//! (M4 C13, ADR-0032): the product router served by `forge_web::ssh` on
//! 127.0.0.1:0 the way `forge serve` wires it, against the development
//! database. Every key, user and repository is synthetic; the client keys
//! are made by `ssh-keygen` in a temporary directory and the server's host
//! key is pinned in a `known_hosts` file (`StrictHostKeyChecking=yes`).
//!
//! Skipped with a message when `ssh` or `ssh-keygen` is not installed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::ssh_key::parse_public_key;
use forge_core::{RefPattern, RequestId, Role, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::ssh_keys::{self, NewSshKey};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::{protected_refs, repositories};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_web::ssh::{generate_host_key, AdmissionLimits, SshLimits, SshPublicInfo, SshServer};
use forge_web::{AppConfig, AppState, BoundedAuthenticator, GitHttpLimits, Lookups, SmartHttp};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

/// `ssh` and `ssh-keygen` answer `-V` / exist on `PATH`.
fn ssh_available() -> bool {
    let ok = |cmd: &str, arg: &str| {
        Command::new(cmd)
            .arg(arg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    };
    ok("ssh", "-V") && ok("ssh-keygen", "-?")
}

/// A path as git's shell (and OpenSSH on Windows) reads it.
fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Run `cmd` with a deadline; a hang is killed and fails the test.
fn run(mut cmd: Command, what: &str) -> Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap_or_else(|e| panic!("{what}: spawn: {e}"));
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if child.try_wait().expect("wait").is_some() {
            return child.wait_with_output().expect("output");
        }
        if Instant::now() > deadline {
            let _ = child.kill(); // ignore: it may have exited meanwhile
            panic!("{what}: no answer within 90 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn text(out: &Output) -> String {
    format!(
        "status={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

struct Fixture {
    rt: tokio::runtime::Runtime,
    t: Option<FreshDb>,
    dir: tempfile::TempDir,
    port: u16,
    known_hosts: PathBuf,
    bare: PathBuf,
    alice_key: PathBuf,
    carol_key: PathBuf,
    bob_key: PathBuf,
    stranger_key: PathBuf,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    _web: CreatedProject,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(()); // ignore: the server may have stopped already
        }
        if let Some(t) = self.t.take() {
            self.rt.block_on(t.drop_now());
        }
    }
}

impl Fixture {
    /// git with an isolated HOME, no prompts, and `ssh` with `key`, the
    /// pinned host key and nothing from the user's own setup.
    fn git(&self, key: &Path) -> Command {
        let home = self.dir.path().join("home");
        let ssh = format!(
            "ssh -i '{}' -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes \
             -o StrictHostKeyChecking=yes -o UserKnownHostsFile='{}' -o ConnectTimeout=20 -p {}",
            slash(key),
            slash(&self.known_hosts),
            self.port
        );
        let mut cmd = Command::new("git");
        cmd.env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_SSH_COMMAND", ssh)
            .env("GIT_SSH_VARIANT", "ssh")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .env_remove("SSH_AUTH_SOCK")
            .env("GIT_AUTHOR_NAME", "forge-test")
            .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
            .env("GIT_COMMITTER_NAME", "forge-test")
            .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid");
        cmd
    }

    fn url(&self, path: &str) -> String {
        format!("ssh://git@127.0.0.1:{}/{path}", self.port)
    }

    /// `ssh` itself (not through git) with `key`.
    fn ssh(&self, key: &Path, remote: &str) -> Output {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-i",
            &slash(key),
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "IdentityAgent=none",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            &format!("UserKnownHostsFile={}", slash(&self.known_hosts)),
            "-p",
            &self.port.to_string(),
            "git@127.0.0.1",
            remote,
        ])
        .env_remove("SSH_AUTH_SOCK");
        run(cmd, &format!("ssh {remote}"))
    }
}

fn keygen(dir: &Path, name: &str) -> (PathBuf, String) {
    let path = dir.join(name);
    let mut cmd = Command::new("ssh-keygen");
    cmd.args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&path);
    let out = run(cmd, "ssh-keygen");
    assert!(out.status.success(), "ssh-keygen: {}", text(&out));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    let public = std::fs::read_to_string(path.with_extension("pub")).expect("public key");
    (path, public)
}

/// alice owns the private project `alice/web` (one commit on `main`,
/// protected for Maintainers); carol is a Developer member; bob is signed
/// up but not a member; the stranger's key is registered to nobody.
fn fixture() -> Fixture {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = dir.path().join("storage");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&storage).expect("storage");
    std::fs::create_dir_all(&home).expect("home");
    let keys = dir.path().join("keys");
    std::fs::create_dir_all(&keys).expect("keys");
    let (alice_key, alice_pub) = keygen(&keys, "alice");
    let (carol_key, carol_pub) = keygen(&keys, "carol");
    let (bob_key, bob_pub) = keygen(&keys, "bob");
    let (stranger_key, _) = keygen(&keys, "stranger");
    let root = StorageRoot::open(&storage).expect("root");
    let git = Git::new(GitConfig::new("git", root.path()));

    let (t, web, bare) = rt.block_on(async {
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
        let web = projects::create(
            &t.db,
            &ctx("req-project-web"),
            &NewProject {
                visibility: Visibility::Private,
                ..NewProject::new("web", alice.namespace_id, "Synthetic", alice.user_id)
            },
        )
        .await
        .expect("project");
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
        projects::add_member(
            &t.db,
            &ctx("req-member-carol"),
            web.project_id,
            carol.user_id,
            Role::Developer,
        )
        .await
        .expect("member");
        protected_refs::create(
            &t.db,
            &ctx("req-rule-main"),
            web.project_id,
            &RefPattern::parse("refs/heads/main").expect("pattern"),
            Role::Maintainer,
        )
        .await
        .expect("rule");
        for (user, public, name) in [
            (alice.user_id, &alice_pub, "alice"),
            (carol.user_id, &carol_pub, "carol"),
            (bob.user_id, &bob_pub, "bob"),
        ] {
            ssh_keys::add(
                &t.db,
                &ctx(&format!("req-key-{name}")),
                &NewSshKey {
                    user,
                    title: String::new(),
                    key: parse_public_key(public).expect("key"),
                },
            )
            .await
            .expect("add key");
        }
        (t, web, bare)
    });

    // Content, pushed straight into the bare repository (not through forge).
    let seed = dir.path().join("seed");
    let local = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "forge-test")
            .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
            .env("GIT_COMMITTER_NAME", "forge-test")
            .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
            .args(args);
        let out = run(cmd, &format!("{args:?}"));
        assert!(out.status.success(), "{args:?}: {}", text(&out));
    };
    let seed_s = slash(&seed);
    local(&["init", "-q", "-b", "main", &seed_s]);
    std::fs::write(seed.join("README"), "synthetic over ssh\n").expect("write");
    local(&["-C", &seed_s, "add", "README"]);
    local(&["-C", &seed_s, "commit", "-q", "-m", "one"]);
    local(&["-C", &seed_s, "push", "-q", &slash(&bare), "main"]);

    let host_key = generate_host_key(&dir.path().join("ssh_host_ed25519_key")).expect("host key");
    let (port, shutdown) = rt.block_on(async {
        let db = t.db.clone(); // clone: pool handle
        let authenticator = Arc::new(BoundedAuthenticator::new(Arc::new(db.clone()), 4));
        let smart = SmartHttp::new(
            git.clone(), // clone: shared pools
            StorageRoot::open(&storage).expect("root"),
            GitHttpLimits::default(),
        )
        .with_push_checks(PathBuf::from(env!("CARGO_BIN_EXE_forge")))
        .expect("push checks armed")
        .with_push(Arc::new(db.clone()), Arc::new(db.clone()))
        .with_history_imports(Arc::new(db.clone()));
        let public = SshPublicInfo {
            clone_host: "127.0.0.1:0".to_owned(),
            host_key_fingerprints: vec![host_key.fingerprint().to_owned()],
        };
        let state = AppState::new(
            "test",
            vec![Arc::new(db.clone())],
            authenticator.clone(),
            Lookups {
                namespaces: Arc::new(db.clone()),
                projects: Arc::new(db.clone()),
            },
        )
        .with_smart_http(smart)
        .with_ssh_info(public.clone());
        let router = forge_web::app(state, AppConfig::default()).expect("router");
        let listener = forge_web::ssh::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = SshServer::new(
            router,
            authenticator,
            &host_key,
            public,
            SshLimits {
                admission: AdmissionLimits {
                    max_connections: 16,
                    max_connections_per_peer: 16,
                    connections_per_minute_per_peer: 1000,
                    auth_failures_per_minute_per_peer: 1000,
                },
                auth_timeout: Duration::from_secs(30),
                max_auth_attempts: 6,
                inactivity_timeout: Duration::from_secs(120),
            },
        );
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let stop = async move {
                let _ = rx.await; // ignore: a dropped sender also stops
            };
            forge_web::ssh::serve(listener, server, stop)
                .await
                .expect("ssh serve");
        });
        (port, tx)
    });
    let known_hosts = dir.path().join("known_hosts");
    std::fs::write(
        &known_hosts,
        format!(
            "[127.0.0.1]:{port} {}\n",
            host_key.public_openssh().expect("public")
        ),
    )
    .expect("known_hosts");
    Fixture {
        rt,
        t: Some(t),
        dir,
        port,
        known_hosts,
        bare,
        alice_key,
        carol_key,
        bob_key,
        stranger_key,
        shutdown: Some(shutdown),
        _web: web,
    }
}

fn bare_ref(f: &Fixture, name: &str) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(&f.bare)
        .args(["rev-parse", "--verify", "-q", name]);
    let out = run(cmd, "rev-parse");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The whole journey: clone, push, the protections and the refusals, over
/// a real SSH connection with the pinned host key.
#[test]
fn git_clone_and_push_over_ssh_go_through_the_same_checks_as_smart_http() {
    if !ssh_available() {
        eprintln!(
            "SKIPPED git_clone_and_push_over_ssh_go_through_the_same_checks_as_smart_http: \
             `ssh` / `ssh-keygen` (OpenSSH client) is not installed on this host"
        );
        return;
    }
    let f = fixture();
    let work = f.dir.path().join("work");

    // alice (Owner) clones over SSH.
    let mut clone = f.git(&f.alice_key);
    clone
        .args(["clone", "-q", &f.url("alice/web.git")])
        .arg(&work);
    let out = run(clone, "clone");
    assert!(out.status.success(), "clone: {}", text(&out));
    assert_eq!(
        std::fs::read_to_string(work.join("README")).expect("README"),
        "synthetic over ssh\n"
    );

    // carol (Developer) pushes a new branch: allowed.
    let w = slash(&work);
    let in_work = |key: &Path, args: &[&str]| {
        let mut cmd = f.git(key);
        cmd.args(["-C", &w]).args(args);
        run(cmd, &format!("{args:?}"))
    };
    std::fs::write(work.join("feature.txt"), "a synthetic feature\n").expect("write");
    assert!(in_work(&f.carol_key, &["add", "feature.txt"])
        .status
        .success());
    assert!(in_work(&f.carol_key, &["commit", "-q", "-m", "feature"])
        .status
        .success());
    let out = in_work(
        &f.carol_key,
        &["push", "-q", "origin", "HEAD:refs/heads/feature"],
    );
    assert!(out.status.success(), "push feature: {}", text(&out));
    let head = String::from_utf8_lossy(&in_work(&f.carol_key, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_owned();
    assert_eq!(
        bare_ref(&f, "refs/heads/feature").as_deref(),
        Some(head.as_str())
    );

    // carol to the protected main (Maintainers only): refused by the same
    // per-ref decision as Smart HTTP (FG-GIT-0008), main unchanged.
    let main_before = bare_ref(&f, "refs/heads/main");
    let out = in_work(&f.carol_key, &["push", "origin", "HEAD:refs/heads/main"]);
    assert!(!out.status.success(), "protected main: {}", text(&out));
    assert!(text(&out).contains("FG-GIT-0008"), "{}", text(&out));
    assert_eq!(bare_ref(&f, "refs/heads/main"), main_before);

    // A delete-only push (git keeps its side open: no pack, no EOF).
    let out = in_work(
        &f.carol_key,
        &["push", "-q", "origin", ":refs/heads/feature"],
    );
    assert!(out.status.success(), "delete: {}", text(&out));
    assert_eq!(bare_ref(&f, "refs/heads/feature"), None);

    // alice (Owner) may push main.
    let out = in_work(
        &f.alice_key,
        &["push", "-q", "origin", "HEAD:refs/heads/main"],
    );
    assert!(out.status.success(), "owner push: {}", text(&out));
    assert_eq!(
        bare_ref(&f, "refs/heads/main").as_deref(),
        Some(head.as_str())
    );

    // bob (not a member) is told the repository does not exist.
    let mut cmd = f.git(&f.bob_key);
    cmd.args(["ls-remote", &f.url("alice/web.git")]);
    let out = run(cmd, "bob ls-remote");
    assert!(!out.status.success());
    assert!(text(&out).contains("FG-REPO-0005"), "{}", text(&out));

    // A key registered to nobody is not let in at all.
    let mut cmd = f.git(&f.stranger_key);
    cmd.args(["ls-remote", &f.url("alice/web.git")]);
    let out = run(cmd, "stranger ls-remote");
    assert!(!out.status.success());
    assert!(text(&out).contains("Permission denied"), "{}", text(&out));

    // Protocol v0 fetch is refused in-band (FG-GIT-0034).
    let mut cmd = f.git(&f.alice_key);
    cmd.args([
        "-c",
        "protocol.version=0",
        "ls-remote",
        &f.url("alice/web.git"),
    ]);
    let out = run(cmd, "v0 ls-remote");
    assert!(!out.status.success());
    assert!(text(&out).contains("FG-GIT-0034"), "{}", text(&out));

    // Anything but git is refused in-band (FG-GIT-0033); a shell only says
    // who the key belongs to.
    let out = f.ssh(&f.alice_key, "git-upload-archive 'alice/web.git'");
    assert!(!out.status.success());
    assert!(text(&out).contains("FG-GIT-0033"), "{}", text(&out));
    let out = f.ssh(&f.alice_key, "cat /etc/passwd");
    assert!(text(&out).contains("FG-GIT-0033"), "{}", text(&out));
}
