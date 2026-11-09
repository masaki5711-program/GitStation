//! `forge backup` against a migrated database and real repositories, with
//! the fake database step (pgBackRest is not in the test environment; its
//! command line is tested in db_tool.rs with scripts). Synthetic data only.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use forge_core::config::{BackupConfig, BackupKey};
use forge_core::ids::{ProjectId, RepoId, UserId};
use forge_core::RequestId;
use forge_db::jobs::{JobRunner, Ran, WorkerId};
use forge_db::test_support::{assert_reason, FreshDb};
use forge_errors::ErrorCode;

use super::db_tool::tests::FakeDb;
use super::*;
use crate::worker::loop_tests::Captured;
use crate::worker::provision::tests::{project, storage};
use crate::worker::provision::{ProvisionHandler, Storage};

/// The synthetic key of the tests.
pub(crate) const KEY_HEX: &str = "5a17e7c0ffee5a17e7c0ffee5a17e7c0ffee5a17e7c0ffee5a17e7c0ffee5a17";

pub(crate) fn rid(s: &str) -> RequestId {
    RequestId::parse(s).expect("request id")
}

#[cfg(unix)]
pub(crate) fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Everything a backup test needs.
pub(crate) struct Fixture {
    pub t: FreshDb,
    pub storage: Storage,
    pub cfg: BackupConfig,
    pub prepared: Prepared,
    pub backup_dir: PathBuf,
    pub _root: tempfile::TempDir,
    pub _tmp: tempfile::TempDir,
}

pub(crate) async fn fixture(state_copy: bool) -> Fixture {
    let t = FreshDb::migrated().await;
    let (root, storage) = storage();
    let tmp = tempfile::tempdir().expect("tmp");
    let backup_dir = tmp.path().join("backup");
    create_private_dir(&backup_dir).expect("backup dir");
    let key = tmp.path().join("backup.key");
    fs::write(&key, format!("{KEY_HEX}\n")).expect("key");
    chmod(&key, 0o600);
    let state_dir = tmp.path().join("state");
    create_private_dir(&state_dir).expect("state dir");
    let cfg = BackupConfig {
        dir: Some(backup_dir.clone()), // clone: the fixture keeps the path too
        key_file: Some(key),
        state_copy_dir: state_copy.then_some(state_dir),
        generations: 3,
        ..BackupConfig::default()
    };
    let prepared = prepare(&cfg).expect("prepare");
    Fixture {
        t,
        storage,
        cfg,
        prepared,
        backup_dir: fs::canonicalize(&backup_dir).expect("canonical"),
        _root: root,
        _tmp: tmp,
    }
}

impl Fixture {
    pub(crate) async fn backup(&self, tool: &FakeDb, request: &str) -> Result<Done, ForgeError> {
        Run {
            db: &self.t.db,
            cfg: &self.cfg,
            prepared: &self.prepared,
            storage: &self.storage.root,
            git: &self.storage.git,
            tool,
            request_id: &rid(request),
        }
        .run()
        .await
    }
}

/// Run git plumbing on a bare repository (test setup only).
pub(crate) fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// A commit with the empty tree on `branch` (`refs/heads/<branch>`).
pub(crate) fn commit(repo: &Path, branch: &str, message: &str) -> String {
    let tree = git(repo, &["hash-object", "-t", "tree", "-w", "/dev/null"]);
    let parent = git(
        repo,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            &format!("refs/heads/{branch}"),
        ],
    );
    let mut args = vec!["commit-tree", tree.as_str(), "-m", message];
    if !parent.is_empty() {
        args.extend(["-p", parent.as_str()]);
    }
    let sha = git(repo, &args);
    git(repo, &["update-ref", &format!("refs/heads/{branch}"), &sha]);
    sha
}

/// A ready repository of a new user `name` (provisioned by the worker's
/// handler): (repository, project, owner, path).
pub(crate) async fn ready_repo(
    t: &FreshDb,
    storage: &Storage,
    name: &str,
) -> (RepoId, ProjectId, UserId, PathBuf) {
    let p = project(t, name).await;
    let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
    runner
        .register(Arc::new(
            ProvisionHandler::new(&t.db, storage.clone()).expect("handler"), // clone: test handles
        ))
        .expect("register");
    let ran = runner.run_once().await.expect("claim").expect("a job ran");
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    let owner = forge_db::users::id_by_username(&t.db, name)
        .await
        .expect("owner");
    let path = storage.root.existing(p.repository_id).expect("exists");
    (p.repository_id, p.project_id, owner, path)
}

/// A merge request `iid` with one version per sha (rows only; the refs
/// are the caller's).
pub(crate) async fn versions(
    t: &FreshDb,
    project: ProjectId,
    author: UserId,
    iid: i64,
    shas: &[&str],
) {
    let (p, a) = (project.get(), author.get());
    let head = shas.last().expect("a version");
    let mut stmts = vec![format!(
        "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, source_branch, \
         target_branch, head_sha) VALUES ({p}, {iid}, 'synthetic', {a}, 'refs/heads/topic-{iid}', \
         'refs/heads/main', '{head}')"
    )];
    for (n, sha) in shas.iter().enumerate() {
        stmts.push(format!(
            "INSERT INTO forge.merge_request_versions (merge_request_id, project_id, version_no, \
             head_sha) SELECT id, {p}, {}, '{sha}' FROM forge.merge_requests WHERE project_id = \
             {p} AND iid = {iid}",
            n + 1
        ));
    }
    t.commit_audited(("project", p), &stmts).await;
}

/// A repository with commits, a merge request with two versions and their
/// refs: (repository, project, owner, path, version shas).
pub(crate) async fn world(
    f: &Fixture,
    name: &str,
) -> (RepoId, ProjectId, UserId, PathBuf, Vec<String>) {
    let (repo, project, owner, path) = ready_repo(&f.t, &f.storage, name).await;
    commit(&path, "main", "synthetic: first");
    let v1 = commit(&path, "topic", "synthetic: v1");
    let v2 = commit(&path, "topic", "synthetic: v2");
    git(&path, &["update-ref", "refs/merge-requests/1/v1", &v1]);
    git(&path, &["update-ref", "refs/merge-requests/1/v2", &v2]);
    git(&path, &["update-ref", "refs/merge-requests/1/head", &v2]);
    versions(&f.t, project, owner, 1, &[&v1, &v2]).await;
    (repo, project, owner, path, vec![v1, v2])
}

#[tokio::test]
async fn backup_then_delete_then_restore_then_verify() {
    let f = fixture(false).await;
    let (repo, _, _, path, shas) = world(&f, "ann").await;
    let refs_before = git(
        &path,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    );
    let tool = FakeDb::default();
    let done = f.backup(&tool, "req-backup-1").await.expect("backup");
    assert_eq!(done.manifest.repositories.len(), 1);
    assert_eq!(done.manifest.repositories[0].id, repo.get());
    assert!(done.path.join(ARCHIVE).is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let mode = fs::metadata(done.path.join(ARCHIVE)).expect("meta").mode() & 0o777;
        assert_eq!(mode, 0o600, "the archive is private");
        let mode = fs::metadata(&done.path).expect("meta").mode() & 0o777;
        assert_eq!(mode, 0o700, "the generation is private");
    }
    // The disaster: every repository is gone.
    fs::remove_dir_all(f.storage.root.path().join("repos")).expect("delete");
    let v = crate::restore::verify(&f.t.db, &f.storage.root, &f.storage.git)
        .await
        .expect("verify");
    assert_eq!(
        v.counts().get(&crate::restore::Kind::RepositoryMissing),
        Some(&1)
    );
    assert!(v.error(true).is_some(), "missing repositories block finish");
    // Restore needs `begin` first.
    let err = crate::restore::repositories(
        &f.prepared,
        &f.storage.root,
        &done.manifest.generation,
        false,
        &rid("req-restore-1"),
    )
    .expect_err("not begun");
    assert_reason(&err, ErrorCode::RestoreVerificationFailed, "not_in_restore");
    crate::restore::begin(f.storage.root.path(), &rid("req-restore-2")).expect("begin");
    crate::restore::repositories(
        &f.prepared,
        &f.storage.root,
        &done.manifest.generation,
        false,
        &rid("req-restore-3"),
    )
    .expect("restore");
    let v = crate::restore::verify(&f.t.db, &f.storage.root, &f.storage.git)
        .await
        .expect("verify");
    assert!(v.findings.is_empty(), "{}", v.report());
    assert_eq!(v.versions, 2);
    let refs_after = git(
        &path,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    );
    assert_eq!(
        refs_after, refs_before,
        "every ref, the version refs included"
    );
    assert!(refs_after.contains(&shas[1]));
    // A second restore without --replace refuses to overwrite.
    let err = crate::restore::repositories(
        &f.prepared,
        &f.storage.root,
        &done.manifest.generation,
        false,
        &rid("req-restore-4"),
    )
    .expect_err("exists");
    assert_reason(&err, ErrorCode::RestoreVerificationFailed, "target_exists");
    // --replace moves the current one aside.
    crate::restore::repositories(
        &f.prepared,
        &f.storage.root,
        &done.manifest.generation,
        true,
        &rid("req-restore-5"),
    )
    .expect("replace");
    assert!(f
        .storage
        .root
        .path()
        .join(format!("pre-restore-{}", done.manifest.generation))
        .is_dir());
    f.t.drop_now().await;
}

#[tokio::test]
async fn a_changed_archive_is_refused_before_anything_is_written() {
    let f = fixture(false).await;
    world(&f, "bea").await;
    let done = f
        .backup(&FakeDb::default(), "req-backup-c")
        .await
        .expect("backup");
    let archive = done.path.join(ARCHIVE);
    let mut bytes = fs::read(&archive).expect("read");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&archive, &bytes).expect("write");
    fs::remove_dir_all(f.storage.root.path().join("repos")).expect("delete");
    crate::restore::begin(f.storage.root.path(), &rid("req-restore-c1")).expect("begin");
    let err = crate::restore::repositories(
        &f.prepared,
        &f.storage.root,
        &done.manifest.generation,
        false,
        &rid("req-restore-c2"),
    )
    .expect_err("changed");
    assert_reason(
        &err,
        ErrorCode::RestoreVerificationFailed,
        "archive_corrupt",
    );
    assert!(
        !f.storage.root.path().join("repos").exists(),
        "nothing written"
    );
    // The right checksum but a wrong key: refused while decrypting.
    let wrong = Prepared {
        key: BackupKey::from_bytes([9; 32]),
        ..f.prepared.clone()
    };
    bytes[last] ^= 0xff;
    fs::write(&archive, &bytes).expect("write");
    let err = crate::restore::repositories(
        &wrong,
        &f.storage.root,
        &done.manifest.generation,
        false,
        &rid("req-restore-c3"),
    )
    .expect_err("wrong key");
    assert_reason(
        &err,
        ErrorCode::RestoreVerificationFailed,
        "archive_corrupt",
    );
    f.t.drop_now().await;
}

#[cfg(unix)]
#[tokio::test]
async fn wide_permission_storage_is_refused() {
    let f = fixture(true).await;
    chmod(&f.backup_dir, 0o750);
    let err = prepare(&f.cfg).expect_err("group access");
    assert_reason(&err, ErrorCode::BackupFailed, "insecure_permissions");
    chmod(&f.backup_dir, 0o700);
    // The state copy directory too.
    let state = f.cfg.state_copy_dir.clone().expect("state dir"); // clone: the fixture keeps it
    chmod(&state, 0o705);
    let err = prepare(&f.cfg).expect_err("other access");
    assert_reason(&err, ErrorCode::BackupFailed, "insecure_permissions");
    chmod(&state, 0o700);
    // A symlink is not a directory forge owns.
    let link = f.backup_dir.with_file_name("link");
    std::os::unix::fs::symlink(&f.backup_dir, &link).expect("symlink");
    let linked = BackupConfig {
        dir: Some(link),
        ..f.cfg.clone()
    };
    let err = prepare(&linked).expect_err("symlink");
    assert_reason(&err, ErrorCode::BackupFailed, "insecure_permissions");
    // A key readable by others is a configuration error (FG-SYS-0002).
    let key = f.cfg.key_file.clone().expect("key"); // clone: the fixture keeps it
    chmod(&key, 0o644);
    let err = prepare(&f.cfg).expect_err("key mode");
    assert_reason(&err, ErrorCode::ConfigInvalid, "insecure_permissions");
    chmod(&key, 0o600);
    // The key inside the backup directory would travel with the backup.
    let inside = f.backup_dir.join("key");
    fs::write(&inside, KEY_HEX).expect("key");
    chmod(&inside, 0o600);
    let carried = BackupConfig {
        key_file: Some(inside),
        ..f.cfg.clone()
    };
    let err = prepare(&carried).expect_err("key inside");
    assert_reason(&err, ErrorCode::BackupFailed, "key_inside_backup");
    // Nothing is configured: refused, not skipped.
    let err = prepare(&BackupConfig::default()).expect_err("unconfigured");
    assert_reason(&err, ErrorCode::BackupFailed, "not_configured");
    f.t.drop_now().await;
}

#[tokio::test]
async fn rotation_keeps_n_generations() {
    let f = fixture(false).await;
    world(&f, "cid").await;
    // Older generations of earlier runs.
    for name in ["20200101T000000Z", "20200102T000000Z", "20200103T000000Z"] {
        let g = f.backup_dir.join(name);
        create_private_dir(&g).expect("gen");
        fs::write(g.join(MANIFEST), b"{}").expect("manifest");
    }
    let done = f
        .backup(&FakeDb::default(), "req-backup-r")
        .await
        .expect("backup");
    assert_eq!(done.removed, ["20200101T000000Z"]);
    let left: Vec<String> = generations(&f.backup_dir)
        .expect("list")
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(left.len(), 3, "backup.generations = 3: {left:?}");
    assert_eq!(left[0], "20200102T000000Z");
    assert_eq!(left[2], done.manifest.generation);
    f.t.drop_now().await;
}

#[tokio::test]
async fn a_failed_database_step_leaves_no_generation_and_the_next_run_cleans_up() {
    let f = fixture(false).await;
    world(&f, "dee").await;
    let failing = FakeDb {
        fail: true,
        ..FakeDb::default()
    };
    let err = f
        .backup(&failing, "req-backup-f1")
        .await
        .expect_err("db fails");
    assert_reason(&err, ErrorCode::BackupFailed, "db_backup_failed");
    assert!(generations(&f.backup_dir).expect("list").is_empty());
    let partials: Vec<_> = fs::read_dir(&f.backup_dir)
        .expect("list")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".partial"))
        .collect();
    assert_eq!(
        partials.len(),
        1,
        "the failed run's .partial stays for diagnosis"
    );
    assert!(!f.backup_dir.join(".lock").exists(), "the lock is released");
    // A lock left by another run refuses the start.
    fs::write(f.backup_dir.join(".lock"), b"other\n").expect("lock");
    let err = f
        .backup(&FakeDb::default(), "req-backup-f2")
        .await
        .expect_err("locked");
    assert_reason(&err, ErrorCode::BackupFailed, "backup_running");
    fs::remove_file(f.backup_dir.join(".lock")).expect("unlock");
    // The partial was made in an earlier second: the next run removes it.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    f.backup(&FakeDb::default(), "req-backup-f3")
        .await
        .expect("backup");
    let partials = fs::read_dir(&f.backup_dir)
        .expect("list")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".partial"))
        .count();
    assert_eq!(partials, 0);
    f.t.drop_now().await;
}

#[tokio::test]
async fn no_secrets_in_logs_or_in_the_backup_files() {
    let f = fixture(true).await;
    let (_, _, owner, path, _) = world(&f, "eve").await;
    git(
        &path,
        &[
            "update-ref",
            "refs/heads/synthetic-marker-branch",
            &commit(&path, "main", "m"),
        ],
    );
    let issued = forge_db::tokens::issue(
        &f.t.db,
        &crate::admin_token::context(&rid("req-token-s")),
        &forge_db::tokens::NewToken {
            user: owner,
            name: "cli".to_owned(),
            scopes: vec![forge_core::Scope::ReadApi],
            expires_in: None,
            is_agent: false,
        },
    )
    .await
    .expect("token");
    let secret = issued.token.reveal();
    let logs = Captured::default();
    let guard = logs.install();
    let done = f
        .backup(&FakeDb::default(), "req-backup-s")
        .await
        .expect("backup");
    drop(guard);
    let text = serde_json::to_string(&logs.events()).expect("json");
    assert!(text.contains("forge.backup.completed"), "{text}");
    for needle in [KEY_HEX, secret.as_str()] {
        assert!(!text.contains(needle), "a secret reached the logs");
    }
    for file in [
        done.path.join(ARCHIVE),
        done.state_copy.clone().expect("copy"),
    ] {
        let bytes = fs::read(&file).expect("read");
        for needle in [
            b"synthetic-marker-branch".as_slice(),
            b"refs/merge-requests",
            KEY_HEX.as_bytes(),
        ] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle),
                "plaintext in {}",
                file.display()
            );
        }
    }
    let manifest = fs::read_to_string(done.path.join(MANIFEST)).expect("manifest");
    assert!(!manifest.contains(KEY_HEX) && !manifest.contains(&secret));
    f.t.drop_now().await;
}
