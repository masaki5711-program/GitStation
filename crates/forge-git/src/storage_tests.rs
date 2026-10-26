use forge_errors::ErrorCode;

use super::*;
use crate::GitConfig;

fn root() -> (tempfile::TempDir, StorageRoot) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = StorageRoot::open(dir.path()).expect("open root");
    (dir, root)
}

fn git() -> (tempfile::TempDir, Git) {
    let home = tempfile::tempdir().expect("home");
    let git = Git::new(GitConfig::new("git", home.path()));
    (home, git)
}

async fn config_get(git: &Git, repo: &Path, key: &'static str) -> String {
    git.command("config")
        .repo(repo)
        .arg("--get")
        .arg(key)
        .output()
        .await
        .expect("config --get")
        .stdout_str()
        .trim()
        .to_string()
}

#[test]
fn path_for_never_escapes_root() {
    let (_d, root) = root();
    for raw in [0, 1, -1, 42, i64::MAX, i64::MIN] {
        let p = root.path_for(RepoId::new(raw));
        assert!(p.starts_with(root.path()), "{}", p.display());
        assert!(!p.to_string_lossy().contains(".."));
    }
}

/// The layout is a persisted contract (ADR-0009): changing the hash or the
/// format would orphan every repository.
#[test]
fn path_for_layout_is_stable() {
    let (_d, root) = root();
    let p = root.path_for(RepoId::new(1));
    // sha256("1") = 6b86b273…
    assert_eq!(
        p.strip_prefix(root.path()).expect("under root"),
        Path::new("repos").join("6b").join("1.git")
    );
    // sha256("42") = 73475cb4…
    assert!(root
        .path_for(RepoId::new(42))
        .ends_with(Path::new("repos").join("73").join("42.git")));
}

#[test]
fn missing_repository_dir_is_storage_missing() {
    let (_d, root) = root();
    let err = root.existing(RepoId::new(7)).expect_err("missing");
    assert_eq!(err.code(), ErrorCode::StorageMissing);
}

#[test]
fn root_that_is_a_file_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").expect("write");
    let err = StorageRoot::open(&file).expect_err("file root");
    assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    assert!(err.to_string().contains("reason=root_not_dir"), "{err}");
}

#[test]
fn missing_root_is_rejected_with_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = StorageRoot::open(dir.path().join("nope")).expect_err("missing root");
    assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    assert!(err.to_string().contains("reason=root_missing"), "{err}");
}

#[cfg(unix)]
#[test]
fn path_outside_root_is_rejected() {
    let (_d, root) = root();
    let outside = tempfile::tempdir().expect("tempdir");
    let path = root.path_for(RepoId::new(9));
    std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
    std::os::unix::fs::symlink(outside.path(), &path).expect("symlink");
    let err = root.existing(RepoId::new(9)).expect_err("symlink escape");
    assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    assert!(err.to_string().contains("reason=symlink"), "{err}");
}

#[cfg(unix)]
#[test]
fn shard_dir_symlink_is_rejected() {
    let (_d, root) = root();
    let outside = tempfile::tempdir().expect("tempdir");
    let path = root.path_for(RepoId::new(9));
    let shard = path.parent().expect("shard");
    std::fs::create_dir_all(shard.parent().expect("repos")).expect("repos");
    std::os::unix::fs::symlink(outside.path(), shard).expect("symlink");
    std::fs::create_dir(outside.path().join("9.git")).expect("repo in outside dir");
    let err = root.existing(RepoId::new(9)).expect_err("symlinked shard");
    assert_eq!(err.code(), ErrorCode::StoragePathRejected);
}

#[cfg(unix)]
#[test]
fn root_symlink_is_rejected() {
    let real = tempfile::tempdir().expect("real");
    let links = tempfile::tempdir().expect("links");
    let link = links.path().join("root");
    std::os::unix::fs::symlink(real.path(), &link).expect("symlink");
    let err = StorageRoot::open(&link).expect_err("symlink root");
    assert!(err.to_string().contains("reason=root_symlink"), "{err}");
}

/// Windows: creating symlinks needs a privilege the test runner may lack, so
/// this variant checks the other rejection on the same path: an entry that is
/// not a directory. Linux CI runs the symlink cases.
#[cfg(windows)]
#[test]
fn path_outside_root_is_rejected() {
    let (_d, root) = root();
    let path = root.path_for(RepoId::new(9));
    std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
    std::fs::write(&path, b"not a repository").expect("write");
    let err = root.existing(RepoId::new(9)).expect_err("not a dir");
    assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    assert!(err.to_string().contains("reason=not_dir"), "{err}");
}

#[tokio::test]
async fn init_bare_creates_a_configured_bare_repository() {
    let (_d, root) = root();
    let (_h, git) = git();
    let hooks = tempfile::tempdir().expect("hooks");
    let path = root
        .init_bare(&git, RepoId::new(5), Some(hooks.path()))
        .await
        .expect("init");
    assert!(path.join("HEAD").is_file());
    for (key, value) in REPO_SETTINGS {
        assert_eq!(&config_get(&git, &path, key).await, value, "{key}");
    }
    assert_eq!(
        config_get(&git, &path, "core.hooksPath").await,
        hooks.path().display().to_string()
    );
    let head = git
        .command("symbolic-ref")
        .repo(&path)
        .arg("HEAD")
        .output()
        .await
        .expect("HEAD");
    assert_eq!(head.stdout_str().trim(), "refs/heads/main");
    assert!(
        !path.join("hooks").join("pre-commit.sample").exists(),
        "no template content"
    );
    // No temporary directory is left next to the repository.
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("shard"))
        .expect("read shard")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
        .collect();
    assert!(leftovers.is_empty());
}

#[tokio::test]
async fn init_bare_refuses_an_existing_path() {
    let (_d, root) = root();
    let (_h, git) = git();
    root.init_bare(&git, RepoId::new(6), None)
        .await
        .expect("first init");
    let err = root
        .init_bare(&git, RepoId::new(6), None)
        .await
        .expect_err("second init must not reinitialize");
    assert_eq!(err.code(), ErrorCode::RepoPathOccupied);
}

#[tokio::test]
async fn init_bare_failure_leaves_nothing_behind() {
    let (_d, root) = root();
    let home = tempfile::tempdir().expect("home");
    let broken = Git::new(GitConfig::new("/nonexistent/forge-test/git", home.path()));
    let err = root
        .init_bare(&broken, RepoId::new(8), None)
        .await
        .expect_err("git missing");
    assert_eq!(err.code(), ErrorCode::GitUnavailable);
    let shard = root.path_for(RepoId::new(8));
    let shard = shard.parent().expect("shard");
    let entries: Vec<_> = std::fs::read_dir(shard)
        .expect("shard exists")
        .filter_map(Result::ok)
        .collect();
    assert!(entries.is_empty(), "no repository and no temp dir remain");
}

#[test]
fn unreadable_or_unwritable_storage_is_storage_io_failed() {
    // An OS-level failure that is not "missing" (here: a path the OS refuses
    // to look up) is an I/O failure, not a path rejection.
    let err = StorageRoot::open("bad\0root").expect_err("invalid path");
    assert_eq!(err.code(), ErrorCode::StorageIoFailed);
    assert!(err.to_string().contains("operation=inspect"), "{err}");
}

#[tokio::test]
async fn a_file_where_repos_dir_belongs_is_rejected_not_followed() {
    let (_d, root) = root();
    let (_h, git) = git();
    std::fs::write(root.path().join("repos"), b"blocking file").expect("write");
    let err = root
        .init_bare(&git, RepoId::new(3), None)
        .await
        .expect_err("repos is a file");
    assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    assert!(err.to_string().contains("reason=not_dir"), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_repos_dir_is_rejected_before_anything_is_created_outside() {
    let (_d, root) = root();
    let (_h, git) = git();
    let outside = tempfile::tempdir().expect("outside");
    std::os::unix::fs::symlink(outside.path(), root.path().join("repos")).expect("symlink");
    let err = root
        .init_bare(&git, RepoId::new(3), None)
        .await
        .expect_err("symlinked repos");
    assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    assert_eq!(
        std::fs::read_dir(outside.path()).expect("read").count(),
        0,
        "nothing created outside the root"
    );
}

#[tokio::test]
async fn relative_hooks_path_is_refused() {
    let (_d, root) = root();
    let (_h, git) = git();
    let err = root
        .init_bare(&git, RepoId::new(4), Some(Path::new("relative/hooks")))
        .await
        .expect_err("relative hooks path");
    assert_eq!(err.code(), ErrorCode::ConfigInvalid);
    assert!(
        root.existing(RepoId::new(4)).is_err(),
        "nothing was created"
    );
}
