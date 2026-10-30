//! Provisioning retry (D4 MR3a): the provision-token marker and adoption.
//! A retry adopts only a repository marked with its own token; anything else
//! at the path stays `FG-REPO-0004`.

use forge_errors::ErrorCode;

use super::*;
use crate::GitConfig;

pub(super) const MARKER: &str = "forge.provisionToken";

pub(super) fn root() -> (tempfile::TempDir, StorageRoot) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = StorageRoot::open(dir.path()).expect("open root");
    (dir, root)
}

pub(super) fn git() -> (tempfile::TempDir, Git) {
    let home = tempfile::tempdir().expect("home");
    let git = Git::new(GitConfig::new("git", home.path()));
    (home, git)
}

pub(super) async fn config_get(git: &Git, repo: &Path, key: &'static str) -> String {
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

pub(super) async fn config_set(git: &Git, repo: &Path, key: &'static str, value: &'static str) {
    git.command("config")
        .repo(repo)
        .arg(key)
        .arg(value)
        .output()
        .await
        .expect("config set");
}

/// A bare repository at the final path that forge did not mark.
pub(super) async fn plain_bare_repo(git: &Git, path: &Path) {
    std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
    git.command("init")
        .arg("--bare")
        .arg("--quiet")
        .arg("--template=")
        .path(path)
        .output()
        .await
        .expect("git init --bare");
}

#[tokio::test]
async fn init_bare_marks_the_repository_with_its_provision_token() {
    let (_d, root) = root();
    let (_h, git) = git();
    let token = uuid::Uuid::new_v4();
    let path = root
        .init_bare(&git, RepoId::new(11), token, None)
        .await
        .expect("init");
    assert_eq!(config_get(&git, &path, MARKER).await, token.to_string());
}

#[tokio::test]
async fn a_first_attempt_creates_the_repository() {
    let (_d, root) = root();
    let (_h, git) = git();
    let token = uuid::Uuid::new_v4();
    let outcome = root
        .init_bare_or_adopt(&git, RepoId::new(12), token, None)
        .await
        .expect("create");
    assert!(matches!(outcome, Provisioned::Created(_)), "{outcome:?}");
    assert_eq!(outcome.path(), root.path_for(RepoId::new(12)));
    assert_eq!(
        config_get(&git, outcome.path(), MARKER).await,
        token.to_string()
    );
}

#[tokio::test]
async fn a_retry_adopts_its_own_marked_repository() {
    let (_d, root) = root();
    let (_h, git) = git();
    let hooks = tempfile::tempdir().expect("hooks");
    let token = uuid::Uuid::new_v4();
    let first = root
        .init_bare_or_adopt(&git, RepoId::new(13), token, Some(hooks.path()))
        .await
        .expect("first attempt");
    // Something changed the configuration between the attempts; adoption
    // leaves the repository as init_bare would have.
    config_set(&git, first.path(), "core.hooksPath", "/elsewhere").await;
    config_set(&git, first.path(), "receive.fsckObjects", "false").await;

    let retry = root
        .init_bare_or_adopt(&git, RepoId::new(13), token, Some(hooks.path()))
        .await
        .expect("retry");
    assert!(matches!(retry, Provisioned::Adopted(_)), "{retry:?}");
    assert_eq!(retry.path(), first.path());
    assert_eq!(
        config_get(&git, retry.path(), "core.hooksPath").await,
        hooks.path().display().to_string()
    );
    for (key, value) in REPO_SETTINGS {
        assert_eq!(&config_get(&git, retry.path(), key).await, value, "{key}");
    }
    assert_eq!(
        config_get(&git, retry.path(), MARKER).await,
        token.to_string()
    );
}

/// The worker died after the rename but before `mark_ready`: the repository
/// is complete and marked, and the retried job adopts it.
#[tokio::test]
async fn a_crash_after_the_rename_is_adopted_on_retry() {
    let (_d, root) = root();
    let (_h, git) = git();
    let token = uuid::Uuid::new_v4();
    let path = root
        .init_bare(&git, RepoId::new(14), token, None)
        .await
        .expect("attempt that crashed before mark_ready");
    let retry = root
        .init_bare_or_adopt(&git, RepoId::new(14), token, None)
        .await
        .expect("retry");
    assert!(matches!(retry, Provisioned::Adopted(_)), "{retry:?}");
    assert_eq!(retry.into_path(), path);
}

/// The worker died before the rename: only a temporary directory remains.
/// It does not block the retry (each attempt uses its own temp name).
#[tokio::test]
async fn a_temp_directory_left_by_a_crash_does_not_block_the_retry() {
    let (_d, root) = root();
    let (_h, git) = git();
    let id = RepoId::new(15);
    let shard = root.path_for(id).parent().expect("shard").to_path_buf();
    std::fs::create_dir_all(&shard).expect("shard");
    let leftover = shard.join(format!(".tmp-{}-{}.git", id.get(), "0".repeat(32)));
    std::fs::create_dir(&leftover).expect("leftover");
    std::fs::write(leftover.join("HEAD"), b"ref: refs/heads/main\n").expect("partial repo");

    let token = uuid::Uuid::new_v4();
    let outcome = root
        .init_bare_or_adopt(&git, id, token, None)
        .await
        .expect("retry after crash");
    assert!(matches!(outcome, Provisioned::Created(_)), "{outcome:?}");
    assert_eq!(
        config_get(&git, outcome.path(), MARKER).await,
        token.to_string()
    );
}

#[tokio::test]
async fn an_unmarked_or_foreign_directory_is_still_repo_path_occupied() {
    let (_d, root) = root();
    let (_h, git) = git();
    let token = uuid::Uuid::new_v4();

    // An unmarked bare repository (e.g. a restored backup from before D4).
    let unmarked = root.path_for(RepoId::new(21));
    plain_bare_repo(&git, &unmarked).await;
    // A repository forge provisioned for another token (a restored database
    // pointing at a directory that is not its own).
    root.init_bare(&git, RepoId::new(22), uuid::Uuid::new_v4(), None)
        .await
        .expect("foreign repository");
    // An empty directory, a plain file, and a directory with a broken config.
    let empty = root.path_for(RepoId::new(23));
    std::fs::create_dir_all(&empty).expect("empty dir");
    let file = root.path_for(RepoId::new(24));
    std::fs::create_dir_all(file.parent().expect("shard")).expect("shard");
    std::fs::write(&file, b"not a repository").expect("file");
    let broken = root.path_for(RepoId::new(25));
    std::fs::create_dir_all(&broken).expect("broken dir");
    std::fs::write(broken.join("config"), b"[broken").expect("broken config");

    for (raw, reason) in [
        (21, "unmarked"),
        (22, "token_mismatch"),
        (23, "not_a_repository"),
        (24, "not_dir"),
        (25, "unreadable"),
    ] {
        let err = root
            .init_bare_or_adopt(&git, RepoId::new(raw), token, None)
            .await
            .expect_err("must not adopt");
        assert_eq!(err.code(), ErrorCode::RepoPathOccupied, "repo {raw}: {err}");
        assert!(
            err.to_string().contains(&format!("adopt_refused={reason}")),
            "repo {raw}: {err}"
        );
    }
    // Nothing was written into the unmarked or the foreign repository.
    let err = git
        .command("config")
        .repo(&unmarked)
        .arg("--get")
        .arg(MARKER)
        .output()
        .await
        .expect_err("still unmarked");
    assert_eq!(err.code(), ErrorCode::GitProcessFailed);
    assert_ne!(
        config_get(&git, &root.path_for(RepoId::new(22)), MARKER).await,
        token.to_string()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_to_a_marked_repository_outside_the_root_is_not_adopted() {
    // A repository with the right marker, but outside the storage root.
    let (_other, other_root) = root();
    let (_d, root) = root();
    let (_h, git) = git();
    let token = uuid::Uuid::new_v4();
    let outside = other_root
        .init_bare(&git, RepoId::new(31), token, None)
        .await
        .expect("outside repo");
    let path = root.path_for(RepoId::new(31));
    std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
    std::os::unix::fs::symlink(&outside, &path).expect("symlink");

    let hooks = tempfile::tempdir().expect("hooks");
    let err = root
        .init_bare_or_adopt(&git, RepoId::new(31), token, Some(hooks.path()))
        .await
        .expect_err("symlink must not be adopted");
    assert_eq!(err.code(), ErrorCode::RepoPathOccupied, "{err}");
    assert!(err.to_string().contains("adopt_refused=symlink"), "{err}");
    let hooks_err = git
        .command("config")
        .repo(&outside)
        .arg("--get")
        .arg("core.hooksPath")
        .output()
        .await
        .expect_err("outside repository untouched");
    assert_eq!(hooks_err.code(), ErrorCode::GitProcessFailed);
}

/// A repository whose `config` is a symlink would make git read (and adopt
/// write) a file outside the root.
#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_config_is_not_adopted() {
    let (_d, root) = root();
    let (_h, git) = git();
    let token = uuid::Uuid::new_v4();
    let path = root
        .init_bare(&git, RepoId::new(32), token, None)
        .await
        .expect("init");
    let outside = tempfile::tempdir().expect("outside");
    let moved = outside.path().join("config");
    std::fs::rename(path.join("config"), &moved).expect("move config out");
    std::os::unix::fs::symlink(&moved, path.join("config")).expect("symlink config");

    let err = root
        .init_bare_or_adopt(&git, RepoId::new(32), token, None)
        .await
        .expect_err("symlinked config");
    assert_eq!(err.code(), ErrorCode::RepoPathOccupied, "{err}");
}
