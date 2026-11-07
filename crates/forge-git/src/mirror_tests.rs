#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

//! The push mirror against local bare repositories (plan 11 §7: the nextest
//! destination) and a fake git that records what it was given.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use forge_core::mirror::{MirrorCredential, MirrorPolicy};
use forge_core::RepoId;
use forge_errors::ErrorCode;

use super::*;
use crate::GitConfig;

const T: Duration = Duration::from_secs(60);

struct Fixture {
    home: tempfile::TempDir,
    _root_dir: tempfile::TempDir,
    dest_dir: tempfile::TempDir,
    git: Git,
    root: StorageRoot,
    id: RepoId,
    path: PathBuf,
}

fn run(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn refs(dir: &Path) -> Vec<String> {
    let out = run(dir, &["for-each-ref", "--format=%(refname)"]);
    out.lines().map(str::to_owned).collect()
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root_dir = tempfile::tempdir().expect("root");
    let dest_dir = tempfile::tempdir().expect("dest");
    let git = Git::new(GitConfig::new("git", home.path()));
    let root = StorageRoot::open(root_dir.path()).expect("root");
    let id = RepoId::new(7);
    let path = root
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    // One commit on main, a tag, a branch and a hidden merge request ref.
    let tree = run(&path, &["hash-object", "-t", "tree", "-w", "/dev/null"]);
    let commit = {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&path)
            .args(["commit-tree", &tree, "-m", "first"])
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .output()
            .expect("commit-tree");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    for r in [
        "refs/heads/main",
        "refs/heads/feature",
        "refs/tags/v1",
        "refs/merge-requests/1/head",
    ] {
        run(&path, &["update-ref", r, &commit]);
    }
    let dest = dest_dir.path().join("mirror.git");
    let out = Command::new("git")
        .args(["init", "--quiet", "--bare"])
        .arg(&dest)
        .output()
        .expect("init dest");
    assert!(out.status.success());
    Fixture {
        home,
        _root_dir: root_dir,
        dest_dir,
        git,
        root,
        id,
        path,
    }
}

impl Fixture {
    fn dest(&self) -> PathBuf {
        self.dest_dir.path().join("mirror.git")
    }

    async fn push(&self) -> Result<MirrorPushReport, ForgeError> {
        let dest = self.dest();
        self.git
            .push_mirror(&self.root, self.id, &MirrorDestination::Local(&dest), T)
            .await
    }
}

#[tokio::test]
async fn the_mirror_gets_branches_and_tags_and_never_merge_request_refs() {
    let f = fixture().await;
    let report = f.push().await.expect("push");
    assert_eq!(report.updated, 3, "{report:?}");
    assert_eq!(
        refs(&f.dest()),
        ["refs/heads/feature", "refs/heads/main", "refs/tags/v1"]
    );
    // Again: nothing to do.
    let again = f.push().await.expect("again");
    assert_eq!((again.updated, again.up_to_date), (0, 3), "{again:?}");
}

#[tokio::test]
async fn a_deleted_branch_and_a_force_push_reach_the_mirror() {
    let f = fixture().await;
    f.push().await.expect("push");
    run(&f.path, &["update-ref", "-d", "refs/heads/feature"]);
    // A rewritten main (another root commit).
    let tree = run(&f.path, &["hash-object", "-t", "tree", "-w", "/dev/null"]);
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(&f.path)
        .args(["commit-tree", &tree, "-m", "rewritten"])
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .output()
        .expect("commit-tree");
    let rewritten = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    run(&f.path, &["update-ref", "refs/heads/main", &rewritten]);
    let report = f.push().await.expect("push");
    assert_eq!((report.deleted, report.updated), (1, 1), "{report:?}");
    assert_eq!(refs(&f.dest()), ["refs/heads/main", "refs/tags/v1"]);
    assert_eq!(run(&f.dest(), &["rev-parse", "refs/heads/main"]), rewritten);
}

#[tokio::test]
async fn a_refused_push_is_int_0013_with_the_ref() {
    let f = fixture().await;
    // The destination refuses every update (like a GitLab pre-receive hook).
    let hook = f.dest().join("hooks").join("pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'GL-HOOK-ERR: protected' >&2\nexit 1\n",
    )
    .expect("hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let e = f.push().await.expect_err("refused");
    assert_eq!(e.code(), ErrorCode::PushMirrorFailed);
    let ctx = |k: &str| {
        e.context()
            .iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(ctx("reason").as_deref(), Some("rejected"));
    assert!(ctx("ref").is_some_and(|r| r.starts_with("refs/")), "{e:?}");
    assert_eq!(ctx("repo_id").as_deref(), Some("7"));
}

/// A fake git: records its arguments and the header variable's presence,
/// then answers like an HTTPS remote that refused the credentials.
fn recording_git(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("argv.log");
    let script = dir.join("fake-git");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = push ] || [ \"$4\" = push ] || echo \"$*\" | grep -q ' push '; then\n\
             printf '%s\\n' \"$@\" > '{log}'\n\
             printf 'key=%s\\nvalue=%s\\ncount=%s\\n' \"$GIT_CONFIG_KEY_0\" \"$GIT_CONFIG_VALUE_0\" \"$GIT_CONFIG_COUNT\" >> '{log}.env'\n\
             echo 'fatal: Authentication failed for https://gitlab.example.invalid/ymma/app.git/' >&2\n\
             exit 128\nfi\nexec git \"$@\"\n",
            log = log.display()
        ),
    )
    .expect("script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    (script, log)
}

#[cfg(unix)]
#[tokio::test]
async fn the_token_goes_in_the_environment_never_in_the_arguments_or_the_error() {
    let f = fixture().await;
    let bin_dir = tempfile::tempdir().expect("bin");
    let (bin, log) = recording_git(bin_dir.path());
    let mut cfg = GitConfig::new(bin, f.home.path());
    cfg.timeout_total = T;
    let git = Git::new(cfg);
    let policy = MirrorPolicy::parse("gitlab.example.invalid", false).expect("policy");
    let url = policy
        .check("https://gitlab.example.invalid/ymma/app.git")
        .expect("url");
    let header = MirrorCredential::parse("synthetic:tok-synthetic-123\n")
        .expect("credential")
        .authorization();
    let e = git
        .push_mirror(
            &f.root,
            f.id,
            &MirrorDestination::Remote {
                url: &url,
                authorization: &header,
                ca_file: None,
            },
            T,
        )
        .await
        .expect_err("auth");
    assert_eq!(e.code(), ErrorCode::PushMirrorFailed);
    assert!(
        e.context()
            .iter()
            .any(|(k, v)| *k == "reason" && v == "auth"),
        "{e:?}"
    );
    let argv = std::fs::read_to_string(&log).expect("argv");
    let env = std::fs::read_to_string(format!("{}.env", log.display())).expect("env");
    assert!(!argv.contains("c3ludGhldGlj"), "token in argv: {argv}");
    assert!(argv.contains("--end-of-options"), "{argv}");
    assert!(argv.contains("+refs/heads/*:refs/heads/*"), "{argv}");
    assert!(argv.contains("+refs/tags/*:refs/tags/*"), "{argv}");
    assert!(
        !argv.contains("--mirror") && !argv.contains("refs/*"),
        "{argv}"
    );
    assert!(argv.contains("protocol.allow=never"), "{argv}");
    assert!(argv.contains("protocol.https.allow=always"), "{argv}");
    assert!(argv.contains("credential.helper="), "{argv}");
    assert!(env.contains("key=http.extraHeader"), "{env}");
    assert!(
        env.contains("value=Authorization: Basic c3ludGhldGlj"),
        "{env}"
    );
    assert!(env.contains("count=1"), "{env}");
    assert!(
        !format!("{e:?}").contains("c3ludGhldGlj"),
        "token in error: {e:?}"
    );
}

#[test]
fn failure_reasons_are_read_from_stderr() {
    assert_eq!(
        classify("fatal: Authentication failed for 'https://x/'"),
        "auth"
    );
    assert_eq!(classify("remote: HTTP Basic: Access denied"), "auth");
    assert_eq!(
        classify("fatal: unable to access 'https://x/': Could not resolve host: x"),
        "unreachable"
    );
    assert_eq!(
        classify("fatal: unable to access 'https://x/': SSL certificate problem"),
        "unreachable"
    );
    assert_eq!(
        classify("error: The requested URL returned error: 502"),
        "unreachable"
    );
    assert_eq!(classify("fatal: something else"), "git");
    let (report, rejected) = read_porcelain(
        b"To https://x/\n*\trefs/heads/a:refs/heads/a\t[new branch]\n\
          !\trefs/heads/main:refs/heads/main\t[remote rejected] (protected)\n\
          -\t:refs/heads/old\t[deleted]\n=\trefs/tags/v1:refs/tags/v1\t[up to date]\nDone\n",
    );
    assert_eq!(
        report,
        MirrorPushReport {
            updated: 1,
            deleted: 1,
            up_to_date: 1
        }
    );
    assert_eq!(rejected.as_deref(), Some("refs/heads/main"));
}
