#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

//! M3 O3: the maintenance steps against the real git (synthetic content),
//! and a fake git for what real git does not show on demand (a helper that
//! outlives its leader, a failing gc).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use forge_core::{ObjectId, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use super::*;
use crate::hidden_refs::merge_request_version_ref;
use crate::smart_http::Phase;
use crate::{GitConfig, RefChange};

const T: Duration = Duration::from_secs(60);
const GRACE: Duration = Duration::from_secs(48 * 3600);

struct Fixture {
    home: tempfile::TempDir,
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
    let id = RepoId::new(9);
    let path = root
        .init_bare(&git, id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    Fixture {
        home,
        _root_dir: root_dir,
        git,
        root,
        id,
        path,
    }
}

impl Fixture {
    /// The same repository with another git binary (the fake).
    fn with_bin(&self, bin: PathBuf) -> Git {
        let mut cfg = GitConfig::new(bin, self.home.path());
        cfg.timeout_total = T;
        Git::new(cfg)
    }

    async fn gc(&self) -> Result<GcRun, ForgeError> {
        self.git.gc_objects(&self.root, self.id, GRACE, T).await
    }

    fn exists(&self, id: &ObjectId) -> bool {
        Command::new("git")
            .arg("--git-dir")
            .arg(&self.path)
            .args(["cat-file", "-e", id.as_str()])
            .status()
            .expect("git")
            .success()
    }
}

/// Plumbing on the bare repository (test setup only).
fn plumb(repo: &Path, args: &[&str], input: Option<&str>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
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
        stdin.write_all(input.as_bytes()).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// A loose commit with one file holding `content`; returns (commit, blob).
fn commit(repo: &Path, content: &str) -> (ObjectId, ObjectId) {
    let blob = plumb(repo, &["hash-object", "-w", "--stdin"], Some(content));
    let tree = plumb(
        repo,
        &["mktree"],
        Some(&format!("100644 blob {blob}\tREADME\n")),
    );
    let commit = plumb(repo, &["commit-tree", &tree], Some("synthetic\n"));
    (
        ObjectId::parse(&commit).expect("commit id"),
        ObjectId::parse(&blob).expect("blob id"),
    )
}

/// The loose file of `id`.
fn loose_path(repo: &Path, id: &ObjectId) -> PathBuf {
    let (dir, file) = id.as_str().split_at(2);
    repo.join("objects").join(dir).join(file)
}

/// Make the loose objects of `ids` look `age` old (the prune compares
/// file times with the grace).
fn age_objects(repo: &Path, ids: &[&ObjectId], age: Duration) {
    let then = SystemTime::now() - age;
    for id in ids {
        let path = loose_path(repo, id);
        let mut perms = std::fs::metadata(&path)
            .expect("loose object")
            .permissions();
        #[allow(clippy::permissions_set_readonly_false)] // test setup: git writes 0444
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).expect("writable");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_modified(then).expect("set mtime");
    }
}

/// The objects of the trees and blobs under `commit` (aged with it).
fn reachable(repo: &Path, commit: &ObjectId) -> Vec<ObjectId> {
    plumb(
        repo,
        &[
            "rev-list",
            "--objects",
            "--no-object-names",
            commit.as_str(),
        ],
        None,
    )
    .lines()
    .map(|l| ObjectId::parse(l).expect("id"))
    .collect()
}

fn age_all(repo: &Path, commit: &ObjectId, age: Duration) {
    let ids = reachable(repo, commit);
    let refs: Vec<&ObjectId> = ids.iter().collect();
    age_objects(repo, &refs, age);
}

fn argv(cmd: &crate::GitCommand) -> Vec<String> {
    cmd.argv()
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// The argument vectors of every step, pinned, and each one accepted by
/// the installed git (verified on git 2.53; the minimum 2.38 accepts every
/// option used: `gc --cruft` since 2.37, `--no-progress` of commit-graph
/// and fsck long before).
#[tokio::test]
async fn maintenance_argv_on_git_2_38() {
    let f = fixture().await;
    let hooks = f.git.config().empty_hooks_dir();
    let hooks = format!("core.hooksPath={}", hooks.display());
    let gc = f.git.gc_command(&f.path, GRACE, T).expect("gc");
    assert_eq!(
        argv(&gc),
        [
            "-c",
            "gc.packRefs=false",
            "-c",
            "gc.writeCommitGraph=false",
            "-c",
            "repack.writeBitmaps=true",
            "-c",
            "pack.writeBitmapHashCache=true",
            "-c",
            hooks.as_str(),
            "gc",
            "--cruft",
            "--prune=172800.seconds.ago",
            "--quiet",
        ]
    );
    let pack = f.git.pack_refs_command(&f.path, T).expect("pack-refs");
    assert_eq!(
        argv(&pack),
        ["-c", hooks.as_str(), "pack-refs", "--all", "--prune"]
    );
    let graph = f.git.commit_graph_command(&f.path, T).expect("graph");
    assert_eq!(
        argv(&graph),
        [
            "-c",
            hooks.as_str(),
            "commit-graph",
            "write",
            "--reachable",
            "--no-progress"
        ]
    );
    for (mode, flag) in [
        (FsckMode::Connectivity, "--connectivity-only"),
        (FsckMode::Full, "--full"),
    ] {
        let fsck = f.git.fsck_command(&f.path, mode, T).expect("fsck");
        assert_eq!(
            argv(&fsck),
            [
                "-c",
                hooks.as_str(),
                "fsck",
                flag,
                "--no-dangling",
                "--no-progress"
            ]
        );
    }
    // Every step runs on the installed git against a repository with history.
    let (c1, _) = commit(&f.path, "one\n");
    plumb(
        &f.path,
        &["update-ref", "refs/heads/main", c1.as_str()],
        None,
    );
    assert_eq!(f.gc().await.expect("gc"), GcRun::Ran);
    f.git.pack_refs(&f.root, f.id, T).await.expect("pack-refs");
    f.git
        .write_commit_graph(&f.root, f.id, T)
        .await
        .expect("commit-graph");
    for mode in [FsckMode::Connectivity, FsckMode::Full] {
        f.git.fsck(&f.root, f.id, mode, T).await.expect("fsck");
    }
    assert!(
        f.path.join("objects/info/commit-graph").exists()
            || f.path.join("objects/info/commit-graphs").exists(),
        "the commit-graph is written"
    );
    let packs: Vec<String> = std::fs::read_dir(f.path.join("objects/pack"))
        .expect("pack dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        packs.iter().any(|p| p.ends_with(".bitmap")),
        "gc writes a bitmap: {packs:?}"
    );
}

/// A commit only `refs/merge-requests/<iid>/v<n>` reaches (the branch was
/// force-pushed away), old enough to be pruned if it were unreachable,
/// survives the maintenance (ADR-0024 §2, plan 11 §5 O3).
#[tokio::test]
async fn a_commit_only_a_version_ref_reaches_survives_maintenance() {
    let f = fixture().await;
    let (reviewed, _) = commit(&f.path, "reviewed\n");
    f.git
        .update_refs(
            &f.root,
            f.id,
            &[RefChange::Create {
                name: merge_request_version_ref(3, 1).expect("ref"),
                new: reviewed.clone(),
            }],
            T,
        )
        .await
        .expect("v1");
    age_all(&f.path, &reviewed, Duration::from_secs(30 * 86_400));
    assert_eq!(f.gc().await.expect("gc"), GcRun::Ran);
    f.git.pack_refs(&f.root, f.id, T).await.expect("pack-refs");
    f.git
        .fsck(&f.root, f.id, FsckMode::Full, T)
        .await
        .expect("fsck");
    for id in reachable(&f.path, &reviewed) {
        assert!(f.exists(&id), "{id:?} survives");
    }
}

/// An unreferenced commit younger than the grace (a merge's `new_sha`
/// before its ref moved, a trial merge's tree) survives.
#[tokio::test]
async fn an_unreferenced_commit_within_grace_survives() {
    let f = fixture().await;
    let (pending, blob) = commit(&f.path, "pending merge\n");
    age_all(&f.path, &pending, Duration::from_secs(47 * 3600));
    assert_eq!(f.gc().await.expect("gc"), GcRun::Ran);
    assert!(f.exists(&pending), "the commit survives");
    assert!(f.exists(&blob), "its blob survives");
}

/// Unreferenced objects older than the grace are pruned.
#[tokio::test]
async fn unreferenced_objects_past_grace_are_pruned() {
    let f = fixture().await;
    let (kept, _) = commit(&f.path, "kept\n");
    plumb(
        &f.path,
        &["update-ref", "refs/heads/main", kept.as_str()],
        None,
    );
    let (old, old_blob) = commit(&f.path, "old trial merge\n");
    age_all(&f.path, &old, Duration::from_secs(49 * 3600));
    assert_eq!(f.gc().await.expect("gc"), GcRun::Ran);
    assert!(!f.exists(&old), "the old commit is pruned");
    assert!(!f.exists(&old_blob), "its blob is pruned");
    assert!(f.exists(&kept), "the branch's commit stays");
}

/// A purged version's ref is deleted (secret removal, ADR-0024 §2): its
/// objects go once they are past the grace.
#[tokio::test]
async fn purged_version_objects_are_pruned_after_grace() {
    let f = fixture().await;
    let (secret, secret_blob) = commit(&f.path, "synthetic secret\n");
    let v1 = merge_request_version_ref(4, 1).expect("ref");
    f.git
        .update_refs(
            &f.root,
            f.id,
            &[RefChange::Create {
                name: v1.clone(),
                new: secret.clone(),
            }],
            T,
        )
        .await
        .expect("v1");
    // A gc while the ref stands packs the objects (they are reachable).
    assert_eq!(f.gc().await.expect("gc"), GcRun::Ran);
    f.git
        .update_refs(
            &f.root,
            f.id,
            &[RefChange::Delete {
                name: v1,
                old: secret.clone(),
            }],
            T,
        )
        .await
        .expect("purge the version");
    // The objects sit in the pack the first gc wrote; an object of a pack
    // is as old as its pack. A pack 49 h old: the next gc drops them.
    let pack_dir = f.path.join("objects/pack");
    let then = SystemTime::now() - Duration::from_secs(49 * 3600);
    for entry in std::fs::read_dir(&pack_dir).expect("pack dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "pack") {
            let mut perms = std::fs::metadata(&path).expect("pack").permissions();
            #[allow(clippy::permissions_set_readonly_false)] // test setup: git writes 0444
            perms.set_readonly(false);
            std::fs::set_permissions(&path, perms).expect("writable");
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|p| p.set_modified(then))
                .expect("age the pack");
        }
    }
    assert_eq!(f.gc().await.expect("gc"), GcRun::Ran);
    assert!(!f.exists(&secret), "the purged commit is pruned");
    assert!(!f.exists(&secret_blob), "the purged blob is pruned");
}

/// pack-refs moves the merge request refs into `packed-refs`; they stay
/// hidden from upload-pack's advertisement (transfer.hideRefs reads both).
#[tokio::test]
async fn hidden_refs_stay_hidden_after_pack_refs() {
    let f = fixture().await;
    let (c, _) = commit(&f.path, "head\n");
    plumb(
        &f.path,
        &["update-ref", "refs/heads/main", c.as_str()],
        None,
    );
    let v1 = merge_request_version_ref(7, 1).expect("ref");
    f.git
        .update_refs(
            &f.root,
            f.id,
            &[RefChange::Create {
                name: v1.clone(),
                new: c.clone(),
            }],
            T,
        )
        .await
        .expect("v1");
    f.git.pack_refs(&f.root, f.id, T).await.expect("pack-refs");
    let packed = std::fs::read_to_string(f.path.join("packed-refs")).expect("packed-refs");
    assert!(packed.contains(v1.as_str()), "the version ref is packed");
    assert!(
        !f.path.join(v1.as_str()).exists(),
        "its loose file is pruned"
    );
    let advert = f
        .git
        .upload_pack(&f.root, f.id, Phase::Advertise, None)
        .expect("upload-pack")
        .timeout(T)
        .output()
        .await
        .expect("advertise");
    let text = advert.stdout_str();
    assert!(text.contains("refs/heads/main"), "{text}");
    assert!(!text.contains("refs/merge-requests/"), "{text}");
}

/// A fake git that starts a helper (like `gc` → `repack` →
/// `pack-objects`) which keeps writing to `marker`, then stalls. Synthetic.
#[cfg(unix)]
fn tree_fake(dir: &Path, marker: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-git.sh");
    let body = format!(
        "#!/bin/sh\n(while true; do echo x >> '{m}'; sleep 0.1; done) &\nsleep 300\n",
        m = marker.display()
    );
    std::fs::write(&path, body).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

#[cfg(windows)]
fn tree_fake(dir: &Path, marker: &Path) -> PathBuf {
    let path = dir.join("fake-git.cmd");
    let body = format!(
        "@echo off\r\nstart \"\" /b powershell -NoProfile -NonInteractive -Command \
         \"while ($true) {{ Add-Content -LiteralPath '{m}' -Value x; Start-Sleep -Milliseconds 100 }}\"\r\n\
         ping -n 300 127.0.0.1 >nul\r\n",
        m = marker.display()
    );
    std::fs::write(&path, body).expect("script");
    path
}

/// A fake git that exits `code` after printing `stderr`. Synthetic.
#[cfg(unix)]
fn exit_fake(dir: &Path, code: i32, stderr: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-exit.sh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho '{stderr}' >&2\nexit {code}\n"),
    )
    .expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

#[cfg(windows)]
fn exit_fake(dir: &Path, code: i32, stderr: &str) -> PathBuf {
    let path = dir.join("fake-exit.cmd");
    std::fs::write(
        &path,
        format!("@echo off\r\necho {stderr} 1>&2\r\nexit /b {code}\r\n"),
    )
    .expect("script");
    path
}

fn marker_len(marker: &Path) -> u64 {
    std::fs::metadata(marker).map_or(0, |m| m.len())
}

/// A step past its timeout kills git's whole process tree: the helper (the
/// stand-in for `repack` / `pack-objects`) stops writing, not only the
/// leader. With only the direct child killed (captured runs before O3) the
/// helper would keep writing.
#[tokio::test]
async fn maintenance_timeout_kills_the_process_tree() {
    let f = fixture().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let marker = tmp.path().join("helper-alive");
    let fake = f.with_bin(tree_fake(tmp.path(), &marker));
    let timeout = Duration::from_secs(8);
    let started = std::time::Instant::now();
    let err = fake
        .gc_objects(&f.root, f.id, GRACE, timeout)
        .await
        .expect_err("timed out");
    assert_eq!(err.code(), ErrorCode::GitTimeout, "{err}");
    assert!(err.to_string().contains("step=gc"), "{err}");
    assert!(started.elapsed() >= timeout, "ran until its timeout");
    assert!(marker_len(&marker) > 0, "the helper ran before the timeout");
    // Let a kill settle, then the helper must have stopped writing.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = marker_len(&marker);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(marker_len(&marker), before, "the helper died with the tree");
}

/// A push's quarantine (`objects/tmp_objdir-incoming-*`) defers the run:
/// gc never starts while objects of a push are not migrated yet. An old
/// one (a crashed push) does not.
#[tokio::test]
async fn incoming_quarantine_defers_maintenance() {
    let f = fixture().await;
    assert_eq!(
        f.git
            .maintenance_blocker(&f.root, f.id)
            .await
            .expect("check"),
        None
    );
    let q = f
        .path
        .join("objects")
        .join(format!("{QUARANTINE_PREFIX}AbC123"));
    std::fs::create_dir(&q).expect("quarantine");
    assert_eq!(
        f.git
            .maintenance_blocker(&f.root, f.id)
            .await
            .expect("check"),
        Some(Blocked::IncomingPush { quarantines: 1 })
    );
    assert_eq!(
        Blocked::IncomingPush { quarantines: 1 }.as_str(),
        "incoming_push"
    );
    // Seven hours later it is a crashed push's leftover, not a running one.
    let (fresh, stale) = scan_quarantines(
        &f.path.join("objects"),
        SystemTime::now() + Duration::from_secs(7 * 3600),
    )
    .expect("scan");
    assert_eq!((fresh, stale), (0, 1), "seven hours later it is stale");
}

/// Another gc holding `gc.pid` (same host, live pid) is a skip, not a
/// failure: git's own check decides.
#[tokio::test]
async fn gc_already_running_is_skipped_not_failed() {
    let f = fixture().await;
    let host = Command::new("hostname").output().expect("hostname");
    let host = String::from_utf8_lossy(&host.stdout).trim().to_owned();
    std::fs::write(
        f.path.join("gc.pid"),
        format!("{} {host}", std::process::id()),
    )
    .expect("gc.pid");
    assert_eq!(f.gc().await.expect("not a failure"), GcRun::AlreadyRunning);
}

/// fsck finds a reachable object that is missing: FG-GIT-0019 with the
/// finding (connectivity and full).
#[tokio::test]
async fn fsck_reports_corruption() {
    let f = fixture().await;
    let (c, blob) = commit(&f.path, "will be lost\n");
    plumb(
        &f.path,
        &["update-ref", "refs/heads/main", c.as_str()],
        None,
    );
    f.git
        .fsck(&f.root, f.id, FsckMode::Connectivity, T)
        .await
        .expect("clean before");
    let lost = loose_path(&f.path, &blob);
    let mut perms = std::fs::metadata(&lost).expect("blob").permissions();
    #[allow(clippy::permissions_set_readonly_false)] // test setup
    perms.set_readonly(false);
    std::fs::set_permissions(&lost, perms).expect("writable");
    std::fs::remove_file(&lost).expect("lose the blob");
    for mode in [FsckMode::Connectivity, FsckMode::Full] {
        let err = f
            .git
            .fsck(&f.root, f.id, mode, T)
            .await
            .expect_err("corrupt");
        assert_eq!(err.code(), ErrorCode::RepositoryCorrupt, "{err}");
        let text = err.to_string();
        assert!(text.contains(&format!("mode={}", mode.as_str())), "{text}");
        assert!(
            text.contains(blob.as_str()),
            "the finding names the blob: {text}"
        );
        assert!(text.contains("step=fsck"), "{text}");
    }
}

/// The counts: loose and packed objects, the pack size and every ref
/// (hidden merge request refs included).
#[tokio::test]
async fn ref_and_object_counts_are_measured() {
    let f = fixture().await;
    let (c, _) = commit(&f.path, "counted\n");
    plumb(
        &f.path,
        &["update-ref", "refs/heads/main", c.as_str()],
        None,
    );
    plumb(&f.path, &["update-ref", "refs/tags/v1", c.as_str()], None);
    f.git
        .update_refs(
            &f.root,
            f.id,
            &[RefChange::Create {
                name: merge_request_version_ref(1, 1).expect("ref"),
                new: c.clone(),
            }],
            T,
        )
        .await
        .expect("v1");
    let before = f
        .git
        .repository_counts(&f.root, f.id, T)
        .await
        .expect("counts");
    assert_eq!(before.loose_objects, 3, "commit, tree, blob");
    assert_eq!(before.packed_objects, 0);
    assert_eq!(before.refs, 3);
    assert_eq!(f.gc().await.expect("gc"), GcRun::Ran);
    let after = f
        .git
        .repository_counts(&f.root, f.id, T)
        .await
        .expect("counts");
    assert_eq!(after.loose_objects, 0);
    assert_eq!(after.packed_objects, 3);
    assert_eq!(after.objects(), 3);
    assert!(after.packs >= 1);
    assert_eq!(after.refs, 3);
}

/// gc exiting non-zero for any other reason is FG-GIT-0018 with the step
/// and git's exit code and stderr.
#[tokio::test]
async fn a_failing_gc_is_repository_maintenance_failed() {
    let f = fixture().await;
    let tmp = tempfile::tempdir().expect("tmp");
    for (code, words) in [(1, "synthetic failure"), (128, "fatal synthetic")] {
        let fake = f.with_bin(exit_fake(tmp.path(), code, words));
        let err = fake
            .gc_objects(&f.root, f.id, GRACE, T)
            .await
            .expect_err("failed");
        assert_eq!(err.code(), ErrorCode::RepositoryMaintenanceFailed, "{err}");
        let text = err.to_string();
        assert!(text.contains("step=gc"), "{text}");
        assert!(text.contains(&format!("exit_code={code}")), "{text}");
        assert!(text.contains("synthetic"), "{text}");
    }
}

/// A prune grace below 24 h is refused before git runs.
#[tokio::test]
async fn a_prune_grace_below_a_day_is_refused() {
    let f = fixture().await;
    let err = f
        .git
        .gc_objects(&f.root, f.id, Duration::from_secs(3600), T)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{err}");
}

#[test]
fn count_objects_needs_every_key() {
    let text = "count: 2\nsize: 8\nin-pack: 10\npacks: 1\nsize-pack: 4\nprune-packable: 0\ngarbage: 0\nsize-garbage: 0\n";
    let c = parse_count_objects(text).expect("parsed");
    assert_eq!(
        (c.loose_objects, c.packed_objects, c.size_pack_kib),
        (2, 10, 4)
    );
    assert_eq!(parse_count_objects("count: 2\n"), None);
    assert_eq!(
        parse_count_objects(&text.replace("in-pack: 10", "in-pack: x")),
        None
    );
}
