#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly
//! Browsing against real git repositories (synthetic content), plus fake
//! git binaries for the timeout, the per-repository cap and the stream
//! cleanup.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use tokio::io::AsyncReadExt;

use super::*;
use crate::{Git, GitConfig, StorageRoot};

/// Plumbing on the bare repository (test setup only; synthetic content).
fn plumb(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
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

fn blob(repo: &Path, content: &[u8]) -> String {
    plumb(repo, &["hash-object", "-w", "--stdin"], Some(content))
}

fn mktree(repo: &Path, entries: &[(&str, &str, &str, &str)]) -> String {
    let mut input = String::new();
    for (mode, kind, oid, name) in entries {
        input.push_str(&format!("{mode} {kind} {oid}\t{name}\n"));
    }
    plumb(repo, &["mktree"], Some(input.as_bytes()))
}

fn commit(repo: &Path, tree: &str, parent: Option<&str>) -> String {
    let mut args = vec!["commit-tree", tree, "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

fn update_ref(repo: &Path, name: &str, oid: &str) {
    plumb(repo, &["update-ref", name, oid], None);
}

struct Fixture {
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    home: PathBuf,
    git: Git,
    storage: StorageRoot,
    id: RepoId,
    path: PathBuf,
}

impl Fixture {
    fn browser(&self, limits: BrowseLimits) -> Browser {
        Browser::new(self.git.clone(), self.storage.clone(), limits)
    }

    /// A browser whose git is `bin` (the repository stays real).
    fn browser_with_bin(&self, bin: &Path, limits: BrowseLimits) -> Browser {
        let git = Git::new(GitConfig::new(bin, &self.home));
        Browser::new(git, self.storage.clone(), limits)
    }
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let id = RepoId::new(11);
    let path = storage
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    Fixture {
        home: home.path().to_path_buf(),
        _home: home,
        _root: root,
        git,
        storage,
        id,
        path,
    }
}

/// Synthetic history:
/// - `c1`: `README.md`, `src/lib.rs`, files literally named `-n` and
///   `--output=x`, a symlink `link` and a submodule `sub`;
/// - `c2` (child of c1): `README.md` changed;
/// - branches `main` (c2, HEAD), `feature/x` (c1), `a` (c1), and a branch
///   named like `c2`'s id but pointing at c1;
/// - tags `v1` (annotated, on c1), `a/b` (lightweight, on c2), `treetag`
///   (a tag of a tree).
struct History {
    c1: String,
    c2: String,
    tree1: String,
    readme2: String,
}

fn history(repo: &Path) -> History {
    let readme1 = blob(repo, b"# synthetic\n");
    let lib = blob(repo, b"pub fn f() {}\n");
    let dash = blob(repo, b"dash-n\n");
    let opt = blob(repo, b"option-like\n");
    let target = blob(repo, b"README.md");
    let src = mktree(repo, &[("100644", "blob", &lib, "lib.rs")]);
    let base = mktree(repo, &[("100644", "blob", &readme1, "README.md")]);
    let sub_commit = commit(repo, &base, None);
    let tree1 = mktree(
        repo,
        &[
            ("100644", "blob", &readme1, "README.md"),
            ("040000", "tree", &src, "src"),
            ("100644", "blob", &dash, "-n"),
            ("100644", "blob", &opt, "--output=x"),
            ("120000", "blob", &target, "link"),
            ("160000", "commit", &sub_commit, "sub"),
        ],
    );
    let c1 = commit(repo, &tree1, None);
    let readme2 = blob(repo, b"# synthetic, second\n");
    let tree2 = mktree(
        repo,
        &[
            ("100644", "blob", &readme2, "README.md"),
            ("040000", "tree", &src, "src"),
        ],
    );
    let c2 = commit(repo, &tree2, Some(&c1));
    update_ref(repo, "refs/heads/main", &c2);
    update_ref(repo, "refs/heads/feature/x", &c1);
    update_ref(repo, "refs/heads/a", &c1);
    update_ref(repo, &format!("refs/heads/{c2}"), &c1);
    let tag = plumb(
        repo,
        &["mktag"],
        Some(
            format!(
                "object {c1}\ntype commit\ntag v1\ntagger forge-test <forge-test@example.invalid> 0 +0000\n\nsynthetic\n"
            )
            .as_bytes(),
        ),
    );
    update_ref(repo, "refs/tags/v1", &tag);
    update_ref(repo, "refs/tags/a/b", &c2);
    update_ref(repo, "refs/tags/treetag", &tree1);
    plumb(repo, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
    History {
        c1,
        c2,
        tree1,
        readme2,
    }
}

fn reason(err: &ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn segs(spec: &str) -> Vec<String> {
    spec.split('/').map(str::to_owned).collect()
}

#[tokio::test]
async fn refs_list_branches_and_tags_peeled_with_the_default_branch() {
    let f = fixture().await;
    let h = history(&f.path);
    let b = f.browser(BrowseLimits::default());
    let list = b.refs(f.id).await.expect("refs");
    assert_eq!(list.default_branch.as_deref(), Some("main"));
    assert!(!list.truncated);
    let names: Vec<&str> = list.refs.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"refs/heads/feature/x"), "{names:?}");
    let v1 = list.find("refs/tags/v1").expect("v1");
    assert_eq!(v1.kind, RefKind::Tag);
    assert_eq!(v1.short(), "v1");
    assert_eq!(v1.commit.as_deref(), Some(h.c1.as_str()), "peeled");
    assert_ne!(v1.target, h.c1, "the tag object itself");
    assert_eq!(list.find("refs/tags/treetag").expect("tt").commit, None);
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
}

#[tokio::test]
async fn refs_over_the_cap_are_truncated_and_resolution_still_finds_the_rest() {
    let f = fixture().await;
    let h = history(&f.path);
    let b = f.browser(BrowseLimits {
        max_refs: 2,
        ..BrowseLimits::default()
    });
    let list = b.refs(f.id).await.expect("refs");
    assert!(list.truncated);
    assert_eq!(list.refs.len(), 2);
    // `main` sorts after the first two refs: found by asking git directly.
    let r = b.resolve_ref(f.id, "main").await.expect("main");
    assert_eq!(r.commit, h.c2);
    let s = b
        .resolve_spec(f.id, &segs("v1/README.md"))
        .await
        .expect("v1");
    assert_eq!(s.rev.commit, h.c1);
}

#[tokio::test]
async fn specs_resolve_by_id_then_longest_branch_then_longest_tag() {
    let f = fixture().await;
    let h = history(&f.path);
    let b = f.browser(BrowseLimits::default());

    let s = b
        .resolve_spec(f.id, &segs("main/src/lib.rs"))
        .await
        .expect("main");
    assert_eq!(
        s.rev.target,
        Target::Branch(RefName::parse("refs/heads/main").unwrap())
    );
    assert_eq!(s.rev.commit, h.c2);
    assert_eq!(s.path.as_str(), "src/lib.rs");

    // `feature/x` is a branch with a `/`: the longest match.
    let s = b
        .resolve_spec(f.id, &segs("feature/x/README.md"))
        .await
        .expect("feature");
    assert_eq!(s.rev.display_name(), "feature/x");
    assert_eq!(s.path.as_str(), "README.md");

    // Branch `a` and tag `a/b`: the kind decides before the length (D/F
    // keeps a branch `a/b` from existing next to `a`).
    let s = b
        .resolve_spec(f.id, &segs("a/b/README.md"))
        .await
        .expect("a");
    assert_eq!(s.rev.kind(), "branch");
    assert_eq!(s.rev.commit, h.c1);
    assert_eq!(s.path.as_str(), "b/README.md");

    // An annotated tag peels to its commit.
    let s = b.resolve_spec(f.id, &segs("v1")).await.expect("v1");
    assert_eq!(s.rev.kind(), "tag");
    assert_eq!(s.rev.commit, h.c1);
    assert!(s.path.is_root());

    // A full id first: the branch named like c2's id (pointing at c1) loses.
    let s = b
        .resolve_spec(f.id, &segs(&format!("{}/README.md", h.c2)))
        .await
        .expect("id");
    assert_eq!(s.rev.target, Target::Commit);
    assert_eq!(s.rev.commit, h.c2);
    let upper = h.c2.to_ascii_uppercase();
    let s = b
        .resolve_spec(f.id, &segs(&upper))
        .await
        .expect("upper-case id");
    assert_eq!(s.rev.commit, h.c2);
    let r = b.resolve_ref(f.id, &h.c2).await.expect("ref=id");
    assert_eq!(r.target, Target::Commit);
}

#[tokio::test]
async fn an_unknown_ref_or_path_is_repository_path_not_resolved() {
    let f = fixture().await;
    let h = history(&f.path);
    let b = f.browser(BrowseLimits::default());
    let not = |r: Result<ResolvedSpec, ForgeError>| {
        let e = r.expect_err("not resolved");
        assert_eq!(e.code(), ErrorCode::RepositoryPathNotResolved);
        reason(&e)
    };
    assert_eq!(
        not(b.resolve_spec(f.id, &segs("nope/x")).await),
        "ref_not_found"
    );
    // A 64-hex id (SHA-256 length) in this SHA-1 repository: an id, missing.
    assert_eq!(
        not(b.resolve_spec(f.id, &segs(&"a".repeat(64))).await),
        "ref_not_found"
    );
    // 39 / 41 hex digits are no id: names that match no ref.
    for n in [39, 41] {
        assert_eq!(
            not(b.resolve_spec(f.id, &segs(&"a".repeat(n))).await),
            "ref_not_found"
        );
    }
    let missing = "0".repeat(40);
    assert_eq!(
        not(b.resolve_spec(f.id, &segs(&missing)).await),
        "ref_not_found"
    );
    assert_eq!(
        not(b.resolve_spec(f.id, &segs(&h.tree1)).await),
        "not_a_commit"
    );
    assert_eq!(
        not(b.resolve_spec(f.id, &segs("treetag")).await),
        "not_a_commit"
    );
    let empty: [&str; 0] = [];
    assert_eq!(not(b.resolve_spec(f.id, &empty).await), "ref_not_found");

    let e = b
        .tree(f.id, &h.c2, &RepoPath::parse("nope").unwrap())
        .await
        .expect_err("tree");
    assert_eq!(
        (e.code(), reason(&e).as_str()),
        (ErrorCode::RepositoryPathNotResolved, "path_not_found")
    );
    let e = b
        .tree(f.id, &h.c2, &RepoPath::parse("README.md").unwrap())
        .await
        .expect_err("blob");
    assert_eq!(reason(&e), "not_a_tree");
    let e = b
        .blob(f.id, &h.c2, &RepoPath::parse("src").unwrap())
        .await
        .expect_err("tree");
    assert_eq!(reason(&e), "not_a_blob");
    let e = b
        .blob(f.id, &h.c1, &RepoPath::parse("sub").unwrap())
        .await
        .expect_err("sub");
    assert_eq!(reason(&e), "not_a_blob");
    let e = b
        .blob(f.id, &h.c1, &RepoPath::root())
        .await
        .expect_err("root");
    assert_eq!(reason(&e), "not_a_blob");
    let e = b
        .resolve_ref(f.id, "refs/heads/nope")
        .await
        .expect_err("full");
    assert_eq!(reason(&e), "ref_not_found");
}

/// Refs and paths that look like options are names: never options.
#[tokio::test]
async fn option_like_refs_and_paths_are_only_names() {
    let f = fixture().await;
    let h = history(&f.path);
    let b = f.browser(BrowseLimits::default());
    for spec in ["-n/README.md", "--output=x", "--all", "-"] {
        let e = b.resolve_spec(f.id, &segs(spec)).await.expect_err(spec);
        assert_eq!(e.code(), ErrorCode::RepositoryPathNotResolved, "{spec}");
    }
    for r in ["--output=x", "-n", "--all", "refs/heads/-x"] {
        let e = b.resolve_ref(f.id, r).await.expect_err(r);
        assert_eq!(e.code(), ErrorCode::RepositoryPathNotResolved, "{r}");
    }
    // The files named `-n` and `--output=x` are read as files.
    let blob_n = b
        .blob(f.id, &h.c1, &RepoPath::parse("-n").unwrap())
        .await
        .expect("-n");
    assert_eq!(blob_n.content, b"dash-n\n");
    let blob_o = b
        .blob(f.id, &h.c1, &RepoPath::parse("--output=x").unwrap())
        .await
        .expect("--output=x");
    assert_eq!(blob_o.content, b"option-like\n");
    // A path that would be an option writes nothing anywhere.
    let e = b
        .tree(f.id, &h.c1, &RepoPath::parse("--output=written").unwrap())
        .await
        .expect_err("no such path");
    assert_eq!(reason(&e), "path_not_found");
    for dir in [f.path.as_path(), f.home.as_path()] {
        assert!(!dir.join("written").exists(), "{}", dir.display());
    }
    let raw = b
        .raw(f.id, &h.c1, &RepoPath::parse("-n").unwrap(), "user:1")
        .await
        .expect("raw -n");
    assert!(matches!(raw.body, RawBody::Bytes(ref v) if v == b"dash-n\n"));
}

#[tokio::test]
async fn trees_list_kinds_sizes_and_truncate_at_the_cap() {
    let f = fixture().await;
    let h = history(&f.path);
    let b = f.browser(BrowseLimits::default());
    let t = b.tree(f.id, &h.c1, &RepoPath::root()).await.expect("root");
    assert_eq!(t.oid, h.tree1);
    assert!(!t.truncated);
    let kind = |name: &str| t.entries.iter().find(|e| e.name == name).map(|e| e.kind);
    assert_eq!(kind("src"), Some(EntryKind::Tree));
    assert_eq!(kind("README.md"), Some(EntryKind::Blob));
    assert_eq!(kind("link"), Some(EntryKind::Symlink));
    assert_eq!(kind("sub"), Some(EntryKind::Submodule));
    assert_eq!(kind("--output=x"), Some(EntryKind::Blob));
    let readme = t.entries.iter().find(|e| e.name == "README.md").unwrap();
    assert_eq!(readme.size, Some(12));
    assert_eq!(
        t.entries.iter().find(|e| e.name == "src").unwrap().size,
        None
    );

    let sub = b
        .tree(f.id, &h.c1, &RepoPath::parse("src").unwrap())
        .await
        .expect("src");
    assert_eq!(sub.entries.len(), 1);
    assert_eq!(sub.entries[0].name, "lib.rs");

    let capped = f.browser(BrowseLimits {
        max_tree_entries: 2,
        ..BrowseLimits::default()
    });
    let t = capped
        .tree(f.id, &h.c1, &RepoPath::root())
        .await
        .expect("capped");
    assert_eq!(t.entries.len(), 2);
    assert!(t.truncated);
}

#[tokio::test]
async fn blobs_are_cut_at_the_cap_and_raw_streams_large_files() {
    let f = fixture().await;
    let h = history(&f.path);
    let limits = BrowseLimits {
        max_blob_bytes: 4,
        raw_inline_max_bytes: 4,
        ..BrowseLimits::default()
    };
    let b = f.browser(limits);
    let path = RepoPath::parse("README.md").unwrap();
    let blob = b.blob(f.id, &h.c2, &path).await.expect("blob");
    assert_eq!(blob.content, b"# sy");
    assert!(blob.truncated);
    assert_eq!(blob.size, 20);
    assert_eq!(blob.entry.oid, h.readme2);

    let raw = b.raw(f.id, &h.c2, &path, "user:1").await.expect("raw");
    assert_eq!(raw.size, 20);
    let RawBody::Stream(mut stream) = raw.body else {
        panic!("a large file streams");
    };
    let mut out = Vec::new();
    stream
        .take_stdout()
        .expect("stdout")
        .read_to_end(&mut out)
        .await
        .expect("read");
    stream.finish().await.expect("finish");
    assert_eq!(out, b"# synthetic, second\n");
}

#[tokio::test]
async fn the_ref_list_is_cached_until_a_push_invalidates_it() {
    let f = fixture().await;
    let h = history(&f.path);
    let b = f.browser(BrowseLimits::default());
    assert_eq!(
        b.resolve_ref(f.id, "main").await.expect("main").commit,
        h.c2
    );
    update_ref(&f.path, "refs/heads/main", &h.c1);
    // Within the TTL the cached list answers.
    assert_eq!(
        b.resolve_ref(f.id, "main").await.expect("cached").commit,
        h.c2
    );
    b.invalidate(f.id);
    assert_eq!(
        b.resolve_ref(f.id, "main").await.expect("fresh").commit,
        h.c1
    );

    let short = f.browser(BrowseLimits {
        refs_ttl: Duration::from_millis(1),
        ..BrowseLimits::default()
    });
    short.refs(f.id).await.expect("load");
    update_ref(&f.path, "refs/heads/main", &h.c2);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        short
            .resolve_ref(f.id, "main")
            .await
            .expect("expired")
            .commit,
        h.c2
    );
}

#[tokio::test]
async fn an_empty_repository_has_no_refs_and_resolves_nothing() {
    let f = fixture().await;
    let b = f.browser(BrowseLimits::default());
    let list = b.refs(f.id).await.expect("refs");
    assert!(list.refs.is_empty());
    assert_eq!(list.default_branch, None);
    let e = b
        .resolve_spec(f.id, &segs("main"))
        .await
        .expect_err("empty");
    assert_eq!(reason(&e), "ref_not_found");
}

#[tokio::test]
async fn archives_stream_zip_and_tar_gz_below_the_prefix() {
    let f = fixture().await;
    let h = history(&f.path);
    let b = f.browser(BrowseLimits::default());
    for (format, magic) in [
        (ArchiveFormat::Zip, &b"PK"[..]),
        (ArchiveFormat::TarGz, &[0x1f, 0x8b][..]),
    ] {
        let mut stream = b
            .archive(f.id, &h.c2, format, "app-1234", "user:1")
            .await
            .expect("archive");
        let mut out = Vec::new();
        stream
            .take_stdout()
            .expect("stdout")
            .read_to_end(&mut out)
            .await
            .expect("read");
        stream.finish().await.expect("finish");
        assert!(out.starts_with(magic), "{format:?}");
        if format == ArchiveFormat::Zip {
            let text = String::from_utf8_lossy(&out);
            assert!(text.contains("app-1234/README.md"), "prefix");
        }
    }
    for bad in ["", "-x", "Upper", "a/b"] {
        let e = b
            .archive(f.id, &h.c2, ArchiveFormat::Zip, bad, "user:1")
            .await
            .expect_err(bad);
        assert_eq!(e.code(), ErrorCode::Internal, "{bad}");
    }
}

/// A fake `git` (synthetic): records its pid, then becomes `body`.
#[cfg(unix)]
fn fake_git(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-git.sh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

#[cfg(unix)]
async fn read_pid(file: &Path) -> u32 {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(file) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no pid written");
}

/// Gone for good: no process and no zombie left behind.
#[cfg(unix)]
async fn wait_reaped(pid: u32) -> bool {
    for _ in 0..200 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// The client goes away mid-archive: dropping the stream kills git and
/// reaps it (no orphan, no zombie).
#[cfg(unix)]
#[tokio::test]
async fn an_archive_dropped_mid_stream_kills_and_reaps_git() {
    let f = fixture().await;
    // A large incompressible file, so git blocks on the full pipe.
    let mut data = Vec::with_capacity(4 * 1024 * 1024);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..data.capacity() {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        data.push(x.to_le_bytes()[0]);
    }
    let big = blob(&f.path, &data);
    let tree = mktree(&f.path, &[("100644", "blob", &big, "big.bin")]);
    let c = commit(&f.path, &tree, None);
    let tmp = tempfile::tempdir().expect("tmp");
    let pid_file = tmp.path().join("pid");
    let bin = fake_git(
        tmp.path(),
        &format!("echo $$ > '{}'\nexec git \"$@\"", pid_file.display()),
    );
    let b = f.browser_with_bin(&bin, BrowseLimits::default());
    let mut stream = b
        .archive(f.id, &c, ArchiveFormat::Zip, "big", "user:1")
        .await
        .expect("archive");
    let mut stdout = stream.take_stdout().expect("stdout");
    let mut chunk = vec![0u8; 1024];
    let n = stdout.read(&mut chunk).await.expect("first bytes");
    assert!(n > 0);
    let pid = read_pid(&pid_file).await;
    assert!(Path::new(&format!("/proc/{pid}")).exists(), "git runs");
    drop(stdout);
    drop(stream);
    assert!(wait_reaped(pid).await, "git {pid} killed and reaped");
}

#[cfg(unix)]
#[tokio::test]
async fn a_browse_call_past_its_deadline_is_git_timeout() {
    let f = fixture().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let bin = fake_git(tmp.path(), "exec sleep 5");
    let b = f.browser_with_bin(
        &bin,
        BrowseLimits {
            timeout: Duration::from_millis(300),
            ..BrowseLimits::default()
        },
    );
    let started = std::time::Instant::now();
    let e = b.refs(f.id).await.expect_err("timeout");
    assert_eq!(e.code(), ErrorCode::GitTimeout);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "killed at the deadline"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn calls_over_the_per_repository_cap_are_refused_at_once() {
    let f = fixture().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let bin = fake_git(tmp.path(), "exec sleep 5");
    let b = f.browser_with_bin(
        &bin,
        BrowseLimits {
            max_per_repo: 2,
            timeout: Duration::from_secs(5),
            ..BrowseLimits::default()
        },
    );
    let busy: Vec<_> = (0..2)
        .map(|_| {
            let b = b.clone(); // clone: one handle per task
            let id = f.id;
            tokio::spawn(async move { b.refs(id).await })
        })
        .collect();
    for _ in 0..100 {
        let held = b
            .slots
            .lock()
            .expect("lock")
            .get(&f.id)
            .copied()
            .unwrap_or(0);
        if held == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let started = std::time::Instant::now();
    let e = b.refs(f.id).await.expect_err("busy");
    assert_eq!(e.code(), ErrorCode::GitBusy);
    assert_eq!(reason(&e), "repo_browse");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "refused at once"
    );
    // Another repository is not affected by this one's cap.
    assert!(b.claim(RepoId::new(12)).is_ok());
    for task in busy {
        task.abort();
        let _ = task.await; // ignore: aborted
    }
    // Aborted calls released their slots.
    assert_eq!(b.slots.lock().expect("lock").get(&f.id), None);
}

#[test]
fn repo_paths_refuse_empty_dot_and_control_segments() {
    assert!(RepoPath::parse("").unwrap().is_root());
    assert!(RepoPath::parse("/").unwrap().is_root());
    assert_eq!(RepoPath::parse("/a/b/").unwrap().as_str(), "a/b");
    assert_eq!(RepoPath::parse("a/b").unwrap().file_name(), Some("b"));
    assert_eq!(RepoPath::parse("-n/--x").unwrap().as_str(), "-n/--x");
    assert_eq!(
        RepoPath::parse("日本語/仕様.md").unwrap().file_name(),
        Some("仕様.md")
    );
    for (raw, rule) in [
        ("a//b", "empty_segment"),
        ("a/./b", "dot_segment"),
        ("../x", "dot_segment"),
        ("a\0b", "control"),
        ("a\nb", "control"),
    ] {
        assert_eq!(RepoPath::parse(raw), Err(rule), "{raw:?}");
    }
    assert_eq!(RepoPath::parse(&"a".repeat(4097)), Err("too_long"));
    assert_eq!(RepoPath::root().child("x"), "x");
    assert_eq!(RepoPath::parse("a").unwrap().child("x"), "a/x");
}

#[test]
fn archive_names_lose_their_extension_once() {
    assert_eq!(
        ArchiveFormat::split("main.zip"),
        Some(("main", ArchiveFormat::Zip))
    );
    assert_eq!(
        ArchiveFormat::split("v1.tar.gz"),
        Some(("v1", ArchiveFormat::TarGz))
    );
    assert_eq!(
        ArchiveFormat::split("x.zip.zip"),
        Some(("x.zip", ArchiveFormat::Zip))
    );
    assert_eq!(ArchiveFormat::split("main.tgz"), None);
    assert_eq!(ArchiveFormat::split("main"), None);
}

#[test]
fn ref_lines_that_cannot_be_named_are_skipped_and_counted() {
    let c = "a".repeat(40);
    let out = format!(
        "{c}\0commit\0\0\0*\0refs/heads/main\n{c}\0commit\0\0\0 \0refs/heads/bad..name\nnot a ref line\n"
    );
    let (list, skipped) = parse_refs(out.as_bytes(), false, 10);
    assert_eq!(list.refs.len(), 1);
    assert_eq!(skipped, 2);
    assert_eq!(list.default_branch.as_deref(), Some("main"));
}

/// Review (MR7, HIGH): malformed `ls-tree` records and names that are not
/// UTF-8 are counted, never silently dropped.
#[test]
fn malformed_and_non_utf8_tree_records_are_counted() {
    let c = "b".repeat(40);
    let mut out = Vec::new();
    out.extend_from_slice(format!("100644 blob {c}      7\tok.txt\0").as_bytes());
    out.extend_from_slice(b"no tab at all\0");
    out.extend_from_slice(b"100644 blob\tshort\0");
    out.extend_from_slice(format!("100644 alien {c}      7\tweird\0").as_bytes());
    out.extend_from_slice(format!("100644 blob {c}      7\t").as_bytes());
    out.extend_from_slice(b"bad\xff\0");
    let parsed = parse_tree(&out, usize::MAX);
    assert_eq!(parsed.entries.len(), 1);
    assert_eq!(parsed.entries[0].name, "ok.txt");
    assert_eq!(parsed.malformed, 3);
    assert_eq!(parsed.not_utf8, 1);
    assert!(!parsed.truncated);
}

#[tokio::test]
async fn a_tree_entry_named_in_bytes_that_are_not_utf8_is_left_out() {
    let f = fixture().await;
    let readme = blob(&f.path, b"x\n");
    let mut input = format!("100644 blob {readme}\tREADME.md\n").into_bytes();
    input.extend_from_slice(format!("100644 blob {readme}\tbad").as_bytes());
    input.extend_from_slice(b"\xff\n");
    let tree = plumb(&f.path, &["mktree"], Some(&input));
    let c = commit(&f.path, &tree, None);
    let b = f.browser(BrowseLimits::default());
    let t = b.tree(f.id, &c, &RepoPath::root()).await.expect("tree");
    let names: Vec<&str> = t.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["README.md"]);
}

#[test]
fn a_blob_entry_without_a_size_is_git_process_failed() {
    let e = TreeEntry {
        name: "x".to_owned(),
        kind: EntryKind::Blob,
        mode: "100644".to_owned(),
        oid: "c".repeat(40),
        size: None,
    };
    let err = e.blob_size().expect_err("no size");
    assert_eq!(err.code(), ErrorCode::GitProcessFailed);
    assert_eq!(reason(&err), "blob_size_missing");
}

/// A fake git that answers one subcommand shape itself and runs the real
/// one for everything else (synthetic).
#[cfg(unix)]
fn wrapping_git(dir: &Path, condition: &str, answer: &str) -> PathBuf {
    fake_git(
        dir,
        &format!("if {condition}; then\n{answer}\nexit 0\nfi\nexec git \"$@\""),
    )
}

#[cfg(unix)]
#[tokio::test]
async fn git_answers_forge_cannot_trust_are_git_process_failed() {
    let f = fixture().await;
    let h = history(&f.path);
    let tmp = tempfile::tempdir().expect("tmp");
    let path = RepoPath::parse("README.md").unwrap();

    // cat-file blob prints fewer bytes than ls-tree said.
    let bin = wrapping_git(
        tmp.path(),
        "[ \"$1\" = cat-file ] && [ \"$2\" = blob ]",
        "printf 'xx'",
    );
    let b = f.browser_with_bin(&bin, BrowseLimits::default());
    let e = b
        .raw(f.id, &h.c2, &path, "user:1")
        .await
        .expect_err("mismatch");
    assert_eq!(e.code(), ErrorCode::GitProcessFailed);
    assert_eq!(reason(&e), "blob_size_mismatch");

    // The root tree id is not an id.
    let bin = wrapping_git(
        tmp.path(),
        "[ \"$1\" = cat-file ] && [ \"$2\" = '--batch-check=%(objectname)' ]",
        "cat > /dev/null; echo garbage",
    );
    let b = f.browser_with_bin(&bin, BrowseLimits::default());
    let e = b
        .tree(f.id, &h.c2, &RepoPath::root())
        .await
        .expect_err("tree id");
    assert_eq!(reason(&e), "tree_of_commit_unreadable");

    // The id check prints nothing.
    let bin = wrapping_git(
        tmp.path(),
        "[ \"$1\" = cat-file ] && [ \"$2\" = '--batch-check=%(objectname) %(objecttype)' ]",
        "cat > /dev/null",
    );
    let b = f.browser_with_bin(&bin, BrowseLimits::default());
    let e = b.resolve_ref(f.id, &h.c2).await.expect_err("empty");
    assert_eq!(e.code(), ErrorCode::GitProcessFailed);
    assert_eq!(reason(&e), "batch_check_empty");

    // One literal path answered with more than 64 KiB.
    let bin = wrapping_git(
        tmp.path(),
        "[ \"$1\" = ls-tree ] && [ \"$4\" = --full-tree ]",
        "head -c 70000 /dev/zero",
    );
    let b = f.browser_with_bin(&bin, BrowseLimits::default());
    let e = b.blob(f.id, &h.c2, &path).await.expect_err("cut");
    assert_eq!(reason(&e), "ls_tree_truncated");
}
