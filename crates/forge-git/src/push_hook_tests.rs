#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use std::path::{Path, PathBuf};

use forge_core::RepoId;

use super::*;
use crate::push_scan::tests::{commit, key_file};
use crate::smart_http::Phase;
use crate::GitConfig;

struct Fixture {
    _home: tempfile::TempDir,
    _root_dir: tempfile::TempDir,
    git: Git,
    root: StorageRoot,
    id: RepoId,
    path: PathBuf,
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root_dir = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let root = StorageRoot::open(root_dir.path()).expect("root");
    let id = RepoId::new(11);
    let path = root
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    git.install_push_hook().expect("hook installed");
    Fixture {
        _home: home,
        _root_dir: root_dir,
        git,
        root,
        id,
        path,
    }
}

impl Fixture {
    fn run(&self, max_blob_bytes: u64) -> PushCheckRun {
        let exe = std::env::current_exe().expect("exe");
        self.git
            .push_check_run(
                &self.root,
                &exe,
                &PushCheckPolicy {
                    request_id: "req-x8-test".to_owned(),
                    lang: Lang::En,
                    repo_id: self.id,
                    limits: crate::push_scan::ScanLimits::new(max_blob_bytes),
                },
            )
            .expect("run")
    }
}

fn env_of(run: &PushCheckRun) -> HookEnv {
    HookEnv {
        dir: Some(run.dir().as_os_str().to_os_string()),
        ..HookEnv::default()
    }
}

const ZERO: &str = "0000000000000000000000000000000000000000";

#[tokio::test]
async fn a_hook_without_its_policy_fails_closed() {
    let exit = run_pre_receive(HookEnv::default(), b"").await;
    assert!(!exit.accept);
    assert!(exit.stderr.starts_with("FG-GIT-0023 "), "{}", exit.stderr);
    assert!(
        exit.stderr.contains("reason: env_missing"),
        "{}",
        exit.stderr
    );

    let empty = tempfile::tempdir().expect("dir");
    let exit = run_pre_receive(
        HookEnv {
            dir: Some(empty.path().as_os_str().to_os_string()),
            ..HookEnv::default()
        },
        b"",
    )
    .await;
    assert!(!exit.accept);
    assert!(
        exit.stderr.contains("reason: policy_unreadable"),
        "{}",
        exit.stderr
    );
}

#[tokio::test]
async fn the_hook_refuses_a_secret_and_never_prints_it() {
    let f = fixture().await;
    let tip = commit(&f.path, &[("deploy/id_rsa", &key_file())], None);
    let run = f.run(1 << 20);
    let stdin = format!("{ZERO} {tip} refs/heads/main\n");
    let exit = run_pre_receive(env_of(&run), stdin.as_bytes()).await;
    assert!(!exit.accept, "{}", exit.stderr);
    assert!(exit.stderr.contains("FG-GIT-0022"), "{}", exit.stderr);
    assert!(exit.stderr.contains("deploy/id_rsa"), "{}", exit.stderr);
    assert!(exit.stderr.contains(&tip[..12]), "{}", exit.stderr);
    assert!(exit.stderr.contains("req-x8-test"), "{}", exit.stderr);
    assert!(!exit.stderr.contains("MIIE"), "{}", exit.stderr);

    let outcome = run.outcome().expect("readable").expect("written");
    assert_eq!(outcome.verdict, Verdict::Refused);
    assert_eq!(outcome.report.total_findings, 1);
    let finding = outcome.report.findings.first().expect("finding");
    assert_eq!(finding.kind.rule(), "private_key");
    assert_eq!(finding.commit.as_deref(), Some(tip.as_str()));
    let dir = run.dir().to_path_buf();
    run.remove().expect("removed");
    assert!(!dir.exists());
}

#[tokio::test]
async fn a_clean_or_delete_only_push_is_accepted_silently() {
    let f = fixture().await;
    let tip = commit(&f.path, &[("README", b"hi\n")], None);
    let run = f.run(1 << 20);
    let stdin = format!("{ZERO} {tip} refs/heads/a\n{tip} {ZERO} refs/heads/b\n");
    let exit = run_pre_receive(env_of(&run), stdin.as_bytes()).await;
    assert!(exit.accept);
    assert_eq!(exit.stderr, "");
    let outcome = run.outcome().expect("readable").expect("written");
    assert_eq!(outcome.verdict, Verdict::Accepted);
    assert_eq!(outcome.report.objects, 3);
}

#[tokio::test]
async fn a_hook_that_cannot_scan_writes_a_failed_result() {
    let f = fixture().await;
    let run = f.run(1 << 20);
    let exit = run_pre_receive(env_of(&run), b"not an update line\n").await;
    assert!(!exit.accept);
    assert!(
        exit.stderr.contains("reason: stdin_invalid"),
        "{}",
        exit.stderr
    );
    let outcome = run.outcome().expect("readable").expect("written");
    assert_eq!(outcome.verdict, Verdict::Failed);
    assert_eq!(outcome.reason.as_deref(), Some("stdin_invalid"));
}

#[tokio::test]
async fn no_result_is_none_and_another_runs_result_is_invalid() {
    let f = fixture().await;
    let run = f.run(1);
    assert_eq!(run.outcome().expect("readable"), None);
    let other = f.run(1);
    let exit = run_pre_receive(env_of(&other), b"").await;
    assert!(exit.accept);
    std::fs::copy(other.dir().join("result"), run.dir().join("result")).expect("copy");
    let err = run.outcome().expect_err("nonce");
    assert_eq!(err.code(), ErrorCode::PushCheckFailed);
}

fn policy(f: &Fixture) -> PushCheckPolicy {
    PushCheckPolicy {
        request_id: "req".to_owned(),
        lang: Lang::En,
        repo_id: f.id,
        limits: crate::push_scan::ScanLimits::new(1),
    }
}

fn arm(f: &Fixture) -> Result<PushCheckRun, ForgeError> {
    f.git
        .push_check_run(&f.root, &std::env::current_exe().expect("exe"), &policy(f))
}

/// Written once at start; a push only verifies it. Missing, changed or
/// (Unix) not executable refuses the push (X8 review: no writes per push,
/// the execute bit checked).
#[tokio::test]
async fn the_hook_script_is_forges_own() {
    let f = fixture().await;
    let script = f.git.push_hooks_dir().join("pre-receive");
    assert_eq!(
        std::fs::read_to_string(&script).expect("script"),
        HOOK_SCRIPT
    );
    let _run = arm(&f).expect("verified");
    // A second start keeps it.
    f.git.install_push_hook().expect("idempotent");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let err = arm(&f).expect_err("not executable");
        assert_eq!(err.code(), ErrorCode::GitUnavailable);
        assert!(format!("{:?}", err.context()).contains("push_hook_not_executable"));
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    }

    std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("tamper");
    let err = arm(&f).expect_err("changed");
    assert_eq!(err.code(), ErrorCode::GitUnavailable);
    let err = f
        .git
        .install_push_hook()
        .expect_err("start refuses a changed one");
    assert_eq!(err.code(), ErrorCode::GitUnavailable);

    std::fs::remove_file(&script).expect("remove");
    let err = arm(&f).expect_err("missing");
    assert!(format!("{:?}", err.context()).contains("push_hook_missing"));
}

#[test]
fn the_hook_exe_loses_the_verbatim_prefix() {
    #[cfg(windows)]
    {
        assert_eq!(
            hook_exe(Path::new(r"\\?\D:\APP\forge\forge.exe")).expect("exe"),
            "D:/APP/forge/forge.exe"
        );
        assert!(hook_exe(Path::new(r"\\?\UNC\host\share\forge.exe")).is_err());
    }
    #[cfg(unix)]
    assert_eq!(
        hook_exe(Path::new("/opt/forge/bin/forge")).expect("exe"),
        "/opt/forge/bin/forge"
    );
    assert!(hook_exe(Path::new("forge")).is_err());
}

#[tokio::test]
async fn receive_pack_with_checks_uses_the_push_hooks() {
    let f = fixture().await;
    let run = f.run(1);
    let cmd = f
        .git
        .receive_pack(
            &f.root,
            f.id,
            Phase::Rpc,
            None,
            std::num::NonZeroU64::new(1 << 20).expect("nz"),
        )
        .expect("cmd")
        .with_push_checks(&run);
    let argv: Vec<String> = cmd
        .argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let hooks: Vec<&String> = argv
        .iter()
        .filter(|a| a.starts_with("core.hooksPath="))
        .collect();
    assert_eq!(
        hooks,
        [&format!(
            "core.hooksPath={}",
            f.git.push_hooks_dir().to_string_lossy()
        )]
    );
    assert_eq!(cmd.env_value(DIR_ENV), Some(run.dir().as_os_str()));
    assert!(cmd.env_value(EXE_ENV).is_some());
}
