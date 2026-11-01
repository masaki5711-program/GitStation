#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use forge_core::RepoId;
use forge_errors::ErrorCode;

use crate::{Git, GitConfig, StorageRoot};

struct Fixture {
    _home: tempfile::TempDir,
    _root_dir: tempfile::TempDir,
    git: Git,
    root: StorageRoot,
    id: RepoId,
    path: PathBuf,
}

/// Plumbing on the bare repository (test setup only; synthetic content).
fn plumb(repo: &Path, args: &[&str], input: Option<&str>) -> String {
    use std::io::Write as _;
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("git");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write");
    } else {
        drop(child.stdin.take());
    }
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn blob(repo: &Path, content: &str) -> String {
    plumb(repo, &["hash-object", "-w", "--stdin"], Some(content))
}

fn tree(repo: &Path, blob: &str) -> String {
    plumb(
        repo,
        &["mktree"],
        Some(&format!("100644 blob {blob}\tREADME\n")),
    )
}

fn commit(repo: &Path, tree: &str, parent: Option<&str>) -> String {
    let mut args = vec!["commit-tree", tree, "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root_dir = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let root = StorageRoot::open(root_dir.path()).expect("root");
    let id = RepoId::new(7);
    let path = root
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    Fixture {
        _home: home,
        _root_dir: root_dir,
        git,
        root,
        id,
        path,
    }
}

/// Synthetic history: `c1 ← c2` on main (c1's blob only in history), an
/// annotated tag on c1, and objects no ref reaches: a commit `s1` on top of
/// c2 (a branch that was deleted / force-pushed away), its blob, and a
/// loose blob.
struct History {
    c1: String,
    c2: String,
    b1: String,
    b2: String,
    t1: String,
    tag: String,
    s1: String,
    secret: String,
    loose: String,
}

fn history(repo: &Path) -> History {
    let b1 = blob(repo, "one\n");
    let t1 = tree(repo, &b1);
    let c1 = commit(repo, &t1, None);
    let b2 = blob(repo, "two\n");
    let c2 = commit(repo, &tree(repo, &b2), Some(&c1));
    plumb(repo, &["update-ref", "refs/heads/main", &c2], None);
    let tag = plumb(
        repo,
        &["mktag"],
        Some(&format!(
            "object {c1}\ntype commit\ntag v1\ntagger forge-test <forge-test@example.invalid> 0 +0000\n\nsynthetic\n"
        )),
    );
    plumb(repo, &["update-ref", "refs/tags/v1", &tag], None);
    let secret = blob(repo, "synthetic secret\n");
    let s1 = commit(repo, &tree(repo, &secret), Some(&c2));
    let loose = blob(repo, "loose\n");
    History {
        c1,
        c2,
        b1,
        b2,
        t1,
        tag,
        s1,
        secret,
        loose,
    }
}

impl Fixture {
    async fn check(&self, wants: &[&str]) -> crate::WantCheck {
        let wants: Vec<String> = wants.iter().map(|w| (*w).to_owned()).collect();
        self.git
            .check_wants(
                &self.root,
                self.id,
                &wants,
                "user:1",
                Duration::from_secs(30),
            )
            .await
            .expect("check")
    }
}

#[tokio::test]
async fn reachable_commits_trees_blobs_and_tags_pass() {
    let f = fixture().await;
    let h = history(&f.path);
    for wants in [
        vec![h.c2.as_str()],
        vec![h.c1.as_str()],
        vec![h.c2.as_str(), h.c1.as_str(), h.c2.as_str()],
        vec![h.b1.as_str()],
        vec![h.b2.as_str(), h.t1.as_str()],
        vec![h.tag.as_str()],
        vec![],
    ] {
        let check = f.check(&wants).await;
        assert_eq!(check.unreachable, 0, "{wants:?}: {check:?}");
        assert!(check.clone().into_result().is_ok());
    }
    let mixed = f.check(&[&h.c2, &h.b1, &h.c2]).await;
    assert_eq!((mixed.wants, mixed.commits, mixed.objects), (2, 1, 1));
}

/// The ADR-0007 hole: an object no ref reaches is refused whatever its
/// type, alone or next to reachable wants. Also pins that `--stdin` before
/// `--not` keeps the wants on the positive side (a git that read stdin
/// after `--not` would make every commit look reachable).
#[tokio::test]
async fn unreachable_or_missing_wants_are_refused() {
    let f = fixture().await;
    let h = history(&f.path);
    let missing = "0123456789012345678901234567890123456789";
    for (wants, first) in [
        (vec![h.s1.as_str()], h.s1.as_str()),
        (vec![h.c2.as_str(), h.s1.as_str()], h.s1.as_str()),
        (vec![h.secret.as_str()], h.secret.as_str()),
        (vec![h.b2.as_str(), h.loose.as_str()], h.loose.as_str()),
        (vec![missing], missing),
    ] {
        let check = f.check(&wants).await;
        assert!(check.unreachable >= 1, "{wants:?}: {check:?}");
        assert_eq!(check.first_unreachable.as_deref(), Some(first), "{wants:?}");
        let err = check.into_result().expect_err("refused");
        assert_eq!(err.code(), ErrorCode::GitProtocolInvalid);
        let ctx: Vec<_> = err
            .context()
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        assert!(
            ctx.contains(&"reason=want_unreachable".to_owned()),
            "{ctx:?}"
        );
        assert!(ctx.iter().any(|c| c.starts_with("check_ms=")), "{ctx:?}");
    }
}

#[tokio::test]
async fn ids_that_are_not_hex_never_reach_git() {
    let f = fixture().await;
    for bad in ["--all", "HEAD", "main", &"A".repeat(40)] {
        let err = f
            .git
            .check_wants(
                &f.root,
                f.id,
                &[bad.to_owned()],
                "user:1",
                Duration::from_secs(5),
            )
            .await
            .expect_err(bad);
        assert_eq!(err.code(), ErrorCode::GitProtocolInvalid, "{bad}");
    }
}

#[tokio::test]
async fn a_missing_repository_is_storage_missing() {
    let f = fixture().await;
    let err = f
        .git
        .check_wants(
            &f.root,
            RepoId::new(99),
            &["1".repeat(40)],
            "user:1",
            Duration::from_secs(5),
        )
        .await
        .expect_err("missing");
    assert_eq!(err.code(), ErrorCode::StorageMissing);
}

/// The cost of the check on a repository with many refs (ADR-0007
/// amendment, MR3). Run by hand:
/// `FORGE_WANT_CHECK_REFS=20000 [FORGE_WANT_CHECK_PACK_REFS=1] cargo nextest run -p forge-git
/// --run-ignored only --no-capture -E 'test(want_check_cost)'`
#[tokio::test]
#[ignore = "measurement, run by hand"]
async fn want_check_cost_with_many_refs() {
    let refs: usize = std::env::var("FORGE_WANT_CHECK_REFS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let f = fixture().await;
    let h = history(&f.path);
    // `refs` branches, each on its own commit on top of c2 (a wide history).
    let mut tips = Vec::with_capacity(refs);
    let mut fast_import = String::new();
    for i in 0..refs {
        fast_import.push_str(&format!(
            "commit refs/heads/b{i}\ncommitter forge-test <forge-test@example.invalid> 0 +0000\ndata 9\nsynthetic\nfrom {}\nM 100644 inline f{i}\ndata 2\n{}\n\n",
            h.c2,
            i % 10
        ));
    }
    plumb(&f.path, &["fast-import", "--quiet"], Some(&fast_import));
    if std::env::var_os("FORGE_WANT_CHECK_PACK_REFS").is_some() {
        plumb(&f.path, &["pack-refs", "--all"], None);
    }
    let listed = plumb(&f.path, &["for-each-ref", "--format=%(objectname)"], None);
    tips.extend(listed.lines().map(str::to_owned));
    assert!(tips.len() > refs, "{}", tips.len());
    let timed = |label: &'static str, wants: Vec<String>| {
        let f = &f;
        async move {
            let started = Instant::now();
            let check = f
                .git
                .check_wants(&f.root, f.id, &wants, "user:1", Duration::from_secs(600))
                .await
                .expect("check");
            println!(
                "{label}: wants={} commits={} objects={} unreachable={} {:?}",
                check.wants,
                check.commits,
                check.objects,
                check.unreachable,
                started.elapsed()
            );
        }
    };
    timed("one tip", vec![tips[0].clone()]).await;
    timed("every tip (clone)", tips.clone()).await;
    timed("an old commit", vec![h.c1.clone()]).await;
    timed("an unreachable commit", vec![h.s1.clone()]).await;
    timed("a blob (object walk)", vec![h.b1.clone()]).await;
    timed("an unreachable blob (object walk)", vec![h.secret.clone()]).await;
}
