use std::time::Duration;

use forge_core::RefName;
use forge_errors::{ErrorCode, Lang};

use super::*;
use crate::redact::redact;

/// A git handle with a private HOME (never the shared temp dir).
fn git_with(config: impl FnOnce(&mut GitConfig)) -> (tempfile::TempDir, Git) {
    let home = tempfile::tempdir().expect("home");
    let mut cfg = GitConfig::new("git", home.path());
    config(&mut cfg);
    (home, Git::new(cfg))
}

fn git() -> (tempfile::TempDir, Git) {
    git_with(|_| {})
}

/// A non-bare repository with one empty commit on `main`; returns its sha.
async fn repo_with_commit(git: &Git, dir: &Path) -> String {
    git.command("init")
        .arg("--quiet")
        .arg("--initial-branch=main")
        .path(dir)
        .output()
        .await
        .expect("init");
    git.command("commit")
        .config_override("user.name", "forge-test")
        .config_override("user.email", "forge-test@example.invalid")
        .repo(dir.join(".git"))
        .current_dir(dir)
        .arg("--allow-empty")
        .arg("--quiet")
        .arg("-m")
        .arg("first")
        .output()
        .await
        .expect("commit");
    git.command("rev-parse")
        .repo(dir.join(".git"))
        .arg("--verify")
        .revision(&RefName::parse("refs/heads/main").expect("ref"))
        .output()
        .await
        .expect("rev-parse")
        .stdout_str()
        .trim()
        .to_string()
}

#[test]
fn argv_puts_config_before_subcommand_revisions_after_end_of_options_paths_after_dashdash() {
    let (_h, g) = git();
    let main = RefName::parse("refs/heads/main").expect("ref");
    let cmd = g
        .command("log")
        .config_override("core.hooksPath", "/srv/forge/hooks")
        .arg("--format=%H")
        .revision(&main)
        .path("src/lib.rs");
    let args: Vec<String> = cmd
        .argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "-c",
            "core.hooksPath=/srv/forge/hooks",
            "log",
            "--format=%H",
            "--end-of-options",
            "refs/heads/main",
            "--",
            "src/lib.rs"
        ]
    );
}

#[test]
fn option_lookalike_cannot_become_a_revision() {
    assert!(Revision::object_id("--upload-pack=evil").is_err());
    assert!(RefName::parse("refs/heads/--exec=x").is_err());
    let oid = "A".repeat(40);
    assert_eq!(
        Revision::object_id(&oid).expect("oid").as_str(),
        "a".repeat(40)
    );
}

#[tokio::test]
async fn revision_operand_selects_the_ref_in_real_git_log() {
    let (_h, g) = git();
    let dir = tempfile::tempdir().expect("repo");
    let sha = repo_with_commit(&g, dir.path()).await;
    let out = g
        .command("log")
        .repo(dir.path().join(".git"))
        .arg("--format=%H")
        .revision(&RefName::parse("refs/heads/main").expect("ref"))
        .output()
        .await
        .expect("log");
    assert_eq!(out.stdout_str().trim(), sha);
}

#[tokio::test]
async fn nonzero_exit_is_git_process_failed() {
    let (_h, g) = git();
    let dir = tempfile::tempdir().expect("tempdir");
    let err = g
        .command("rev-parse")
        .current_dir(dir.path())
        .arg("--git-dir")
        .output()
        .await
        .expect_err("not a repository");
    assert_eq!(err.code(), ErrorCode::GitProcessFailed);
    let internal = err.to_string();
    assert!(internal.contains("exit_code=128"), "{internal}");
    assert!(
        internal.contains("not a git repository"),
        "stderr is kept for the log: {internal}"
    );
    assert!(
        !err.user_message(Lang::En).contains("not a git repository"),
        "stderr never reaches the user"
    );
}

#[tokio::test]
async fn hung_git_is_killed_after_timeout() {
    let (_h, g) = git();
    let dir = tempfile::tempdir().expect("tempdir");
    g.command("init")
        .arg("--bare")
        .arg("--quiet")
        .path(dir.path())
        .output()
        .await
        .expect("init");
    // `cat-file --batch` reads stdin until EOF; a stalled client never sends it.
    let err = g
        .command("cat-file")
        .repo(dir.path())
        .arg("--batch")
        .hold_stdin_open()
        .timeout(Duration::from_millis(300))
        .output()
        .await
        .expect_err("must time out");
    assert_eq!(err.code(), ErrorCode::GitTimeout);
    let internal = err.to_string();
    assert!(internal.contains("kind=total"), "{internal}");
    assert!(internal.contains("timeout_ms=300"), "{internal}");
}

#[tokio::test]
async fn slot_is_released_after_timeout_and_failure() {
    let (_h, g) = git_with(|c| {
        c.max_concurrency = 1;
        c.acquire_timeout = Duration::from_secs(2);
    });
    let dir = tempfile::tempdir().expect("tempdir");
    g.command("init")
        .arg("--bare")
        .arg("--quiet")
        .path(dir.path())
        .output()
        .await
        .expect("init");
    let timed_out = g
        .command("cat-file")
        .repo(dir.path())
        .arg("--batch")
        .hold_stdin_open()
        .timeout(Duration::from_millis(200))
        .output()
        .await;
    assert!(timed_out.is_err());
    let failed = g
        .command("rev-parse")
        .current_dir(tempfile::tempdir().expect("t").path())
        .arg("--git-dir")
        .output()
        .await;
    assert!(failed.is_err());
    g.command("version")
        .output()
        .await
        .expect("the single slot must be free again");
}

#[tokio::test]
async fn concurrency_limit_reports_git_busy() {
    let (_h, g) = git_with(|c| {
        c.max_concurrency = 1;
        c.acquire_timeout = Duration::from_millis(100);
    });
    let _held = Arc::clone(&g.slots).acquire_owned().await.expect("permit");
    let err = g
        .command("version")
        .output()
        .await
        .expect_err("no free slot");
    assert_eq!(err.code(), ErrorCode::GitBusy);
}

#[tokio::test]
async fn stdin_input_is_delivered_to_child() {
    let (_h, g) = git();
    let dir = tempfile::tempdir().expect("tempdir");
    let out = g
        .command("hash-object")
        .current_dir(dir.path())
        .arg("--stdin")
        .stdin(b"hello\n".to_vec())
        .output()
        .await
        .expect("hash-object");
    assert_eq!(
        out.stdout_str().trim(),
        "ce013625030ba8dba906f756967f9e9ca394464a"
    );
}

#[tokio::test]
async fn large_stdin_and_stdout_do_not_deadlock() {
    let (_h, g) = git();
    let dir = tempfile::tempdir().expect("repo");
    let head = repo_with_commit(&g, dir.path()).await;
    // ~820 KB in, ~1 MB out: far above pipe buffers on every OS. Full object
    // ids, not ref names: resolving a ref per line takes seconds on Windows
    // and made this test time out under a parallel run.
    let input = format!("{}\n", head.trim()).repeat(20_000).into_bytes();
    let out = g
        .command("cat-file")
        .repo(dir.path().join(".git"))
        .arg("--batch-check")
        .stdin(input)
        .timeout(Duration::from_secs(60))
        .output()
        .await
        .expect("no deadlock");
    assert_eq!(out.stdout_str().lines().count(), 20_000);
}

#[tokio::test]
async fn output_over_cap_is_git_output_too_large() {
    let (_h, g) = git_with(|c| c.max_output_bytes = 4);
    let err = g.command("version").output().await.expect_err("over cap");
    assert_eq!(err.code(), ErrorCode::GitOutputTooLarge);
}

/// Regression (git-protocol-reviewer, 2026-10-26): a producer that keeps
/// writing past the cap must be stopped at once — not left blocked on the
/// undrained pipe until the total timeout, holding a slot.
#[tokio::test]
async fn output_over_cap_stops_a_producer_that_keeps_writing() {
    let (_h, g) = git_with(|c| c.max_output_bytes = 1024);
    let dir = tempfile::tempdir().expect("repo");
    repo_with_commit(&g, dir.path()).await;
    let input = "refs/heads/main\n".repeat(20_000).into_bytes(); // ~1 MB of output
    let started = std::time::Instant::now();
    let err = g
        .command("cat-file")
        .repo(dir.path().join(".git"))
        .arg("--batch-check")
        .stdin(input)
        .timeout(Duration::from_secs(60))
        .output()
        .await
        .expect_err("over cap");
    assert_eq!(err.code(), ErrorCode::GitOutputTooLarge);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "stopped promptly, not at the timeout: {:?}",
        started.elapsed()
    );
}

/// M2 MR7: `output_truncated` stops a producer at the cap without an
/// error (the `stopped_early` branch: git killed, reaped, `truncated`);
/// output under the cap is whole and not truncated. (`reap_failed` needs
/// a child SIGKILL cannot end; it is not reproducible in a test.)
#[tokio::test]
async fn output_truncated_stops_at_the_cap_and_is_not_an_error() {
    let (_h, g) = git();
    let dir = tempfile::tempdir().expect("repo");
    repo_with_commit(&g, dir.path()).await;
    let input = "refs/heads/main\n".repeat(20_000).into_bytes(); // ~1 MB of output
    let started = std::time::Instant::now();
    let (out, truncated) = g
        .command("cat-file")
        .repo(dir.path().join(".git"))
        .arg("--batch-check")
        .stdin(input)
        .timeout(Duration::from_secs(60))
        .output_truncated(100)
        .await
        .expect("truncated, not an error");
    assert!(truncated);
    assert_eq!(out.stdout.len(), 100);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "stopped promptly"
    );
    let (out, truncated) = g
        .command("version")
        .output_truncated(4096)
        .await
        .expect("small");
    assert!(!truncated);
    assert!(out.stdout_str().starts_with("git version"));
    // The cap never exceeds `max_output_bytes`.
    let (_h2, small) = git_with(|c| c.max_output_bytes = 4);
    let (out, truncated) = small
        .command("version")
        .output_truncated(4096)
        .await
        .expect("capped by the config");
    assert!(truncated);
    assert_eq!(out.stdout.len(), 4);
}

#[test]
fn git_protocol_accepts_only_protocol_pairs() {
    let (_h, g) = git();
    let ok = g.command("upload-pack").git_protocol("version=2");
    assert_eq!(ok.envs, [("GIT_PROTOCOL", OsString::from("version=2"))]);
    for bad in ["", "version=2\nGIT_DIR=/x", "version=2 GIT_DIR=/x", "a;b"] {
        let cmd = g.command("upload-pack").git_protocol(bad);
        assert!(cmd.envs.is_empty(), "{bad:?} must be dropped");
    }
}

#[tokio::test]
async fn option_like_generated_value_is_refused() {
    let (_h, g) = git();
    let err = g
        .command("config")
        .arg_owned("--add")
        .output()
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::Internal);
}

#[tokio::test]
async fn missing_git_binary_is_git_unavailable() {
    let home = tempfile::tempdir().expect("home");
    let g = Git::new(GitConfig::new("/nonexistent/forge-test/git", home.path()));
    let err = g.command("version").output().await.expect_err("no binary");
    assert_eq!(err.code(), ErrorCode::GitUnavailable);
    assert!(err.to_string().contains("reason=spawn_failed"), "{err}");
}

#[test]
fn scrubbed_environment_is_explicit() {
    let (home, g) = git();
    let cmd = g
        .command("status")
        .repo("/srv/forge/repos/00/1.git")
        .build(false);
    let envs: std::collections::HashMap<String, Option<String>> = cmd
        .as_std()
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let get = |k: &str| envs.get(k).cloned().flatten();
    assert_eq!(get("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
    assert_eq!(get("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
    assert_eq!(get("LC_ALL").as_deref(), Some("C"));
    assert_eq!(
        get("HOME"),
        Some(home.path().to_string_lossy().into_owned())
    );
    assert_eq!(
        get("GIT_CONFIG_GLOBAL"),
        Some(
            home.path()
                .join(".gitconfig")
                .to_string_lossy()
                .into_owned()
        )
    );
    assert_eq!(get("GIT_DIR").as_deref(), Some("/srv/forge/repos/00/1.git"));
    assert_eq!(get("GIT_NO_REPLACE_OBJECTS").as_deref(), Some("1"));
    assert_eq!(get("GIT_LITERAL_PATHSPECS").as_deref(), Some("1"));
    for k in envs.keys() {
        assert!(
            !k.starts_with("GIT_")
                || [
                    "GIT_CONFIG_NOSYSTEM",
                    "GIT_TERMINAL_PROMPT",
                    "GIT_CONFIG_GLOBAL",
                    "GIT_NO_REPLACE_OBJECTS",
                    "GIT_LITERAL_PATHSPECS",
                    "GIT_DIR"
                ]
                .contains(&k.as_str()),
            "unexpected git variable {k}"
        );
    }
}

#[test]
fn pinned_variables_cannot_be_overridden_and_cwd_defaults_to_home() {
    let (home, g) = git();
    let mut cmd = g.command("version");
    // Simulate a future bug that pushes a pinned key through the caller list.
    cmd.envs.push(("GIT_CONFIG_NOSYSTEM", OsString::from("0")));
    let built = cmd.build(false);
    let std_cmd = built.as_std();
    let nosystem = std_cmd
        .get_envs()
        .filter(|(k, _)| *k == "GIT_CONFIG_NOSYSTEM")
        .last()
        .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()));
    assert_eq!(nosystem.as_deref(), Some("1"), "pinned value wins");
    assert_eq!(std_cmd.get_current_dir(), Some(home.path()));
}

#[tokio::test]
async fn pathspec_magic_is_treated_literally() {
    let (_h, g) = git();
    let dir = tempfile::tempdir().expect("repo");
    repo_with_commit(&g, dir.path()).await;
    // With magic, ":/" would mean "the whole tree from the top" and match the
    // commit; literally it is a path that does not exist, so nothing matches.
    let out = g
        .command("log")
        .repo(dir.path().join(".git"))
        .arg("--format=%H")
        .revision(&RefName::parse("refs/heads/main").expect("ref"))
        .path(":/")
        .output()
        .await
        .expect("log");
    assert_eq!(out.stdout_str().trim(), "");
}

#[tokio::test]
async fn discovery_does_not_walk_up_into_a_parent_repository() {
    let (_h, g) = git();
    let outer = tempfile::tempdir().expect("outer");
    repo_with_commit(&g, outer.path()).await;
    let inner = outer.path().join("inner");
    std::fs::create_dir(&inner).expect("inner");
    let err = g
        .command("rev-parse")
        .current_dir(&inner)
        .arg("--git-dir")
        .output()
        .await
        .expect_err("must not find the parent repository");
    assert_eq!(err.code(), ErrorCode::GitProcessFailed);
}

#[test]
fn redact_removes_url_credentials() {
    assert_eq!(
        redact("fatal: unable to access 'https://user:s3cret@git.example/x.git/': 403"),
        "fatal: unable to access 'https://***@git.example/x.git/': 403"
    );
    assert_eq!(redact("no url here"), "no url here");
    assert_eq!(
        redact("ssh://git.example/x and https://a@b/c"),
        "ssh://git.example/x and https://***@b/c"
    );
    assert_eq!(
        redact("GET https://h/api?private_token=abc123&page=2 failed"),
        "GET https://h/api?private_token=***&page=2 failed"
    );
    assert_eq!(
        redact("http.extraheader: Authorization: Bearer abc.def"),
        "http.extraheader: Authorization: ***"
    );
}
