//! `remove_until` (D5 MR4): the purge's removal of a bare repository is
//! idempotent, sweeps the leftovers of this repository only, refuses a
//! symlink or a file, and names a passed deadline.

use std::time::{Duration, Instant};

use forge_errors::ErrorCode;

use super::adopt_tests::{git, root};
use super::*;

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

#[tokio::test]
async fn an_existing_repository_is_removed_and_a_second_call_finds_nothing() {
    let (_d, root) = root();
    let (_h, git) = git();
    let id = RepoId::new(71);
    let path = root
        .init_bare(&git, id, Uuid::new_v4(), None)
        .await
        .expect("created");
    let removed = root.remove_until(id, soon()).await.expect("removed");
    assert!(removed.existed);
    assert_eq!(removed.swept, 1, "the renamed directory");
    assert!(!path.exists());
    let shard = path.parent().expect("shard");
    assert_eq!(std::fs::read_dir(shard).expect("shard").count(), 0);
    let again = root.remove_until(id, soon()).await.expect("idempotent");
    assert_eq!(
        again,
        Removal {
            existed: false,
            swept: 0
        }
    );
}

#[tokio::test]
async fn a_missing_directory_shard_or_repos_dir_is_not_an_error() {
    let (_d, root) = root();
    let id = RepoId::new(72);
    // Nothing at all under the root.
    let removed = root.remove_until(id, soon()).await.expect("no repos dir");
    assert!(!removed.existed);
    // The shard of another repository exists, not this one's directory.
    let path = root.path_for(id);
    std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
    let removed = root.remove_until(id, soon()).await.expect("no directory");
    assert_eq!(removed.swept, 0);
}

#[tokio::test]
async fn leftovers_of_this_repository_are_swept_and_others_kept() {
    let (_d, root) = root();
    let id = RepoId::new(7);
    let path = root.path_for(id);
    let shard = path.parent().expect("shard").to_path_buf();
    std::fs::create_dir_all(shard.join(".purge-7-aaaa.git/objects")).expect("leftover");
    std::fs::create_dir_all(shard.join(".tmp-7-bbbb.git")).expect("tmp");
    // Look-alike names of other repositories in the same shard stay.
    std::fs::create_dir_all(shard.join(".purge-70-cccc.git")).expect("other");
    std::fs::create_dir_all(shard.join(".tmp-77-dddd.git")).expect("other tmp");
    let removed = root.remove_until(id, soon()).await.expect("swept");
    assert!(!removed.existed, "the repository itself was not there");
    assert_eq!(removed.swept, 2);
    assert!(!shard.join(".purge-7-aaaa.git").exists());
    assert!(!shard.join(".tmp-7-bbbb.git").exists());
    assert!(shard.join(".purge-70-cccc.git").exists());
    assert!(shard.join(".tmp-77-dddd.git").exists());
}

#[tokio::test]
async fn a_file_at_the_path_is_refused_and_left_alone() {
    let (_d, root) = root();
    let id = RepoId::new(73);
    let path = root.path_for(id);
    std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
    std::fs::write(&path, b"not a repository").expect("file");
    let err = root.remove_until(id, soon()).await.expect_err("a file");
    assert_eq!(err.code(), ErrorCode::StoragePathRejected, "{err}");
    assert!(err.to_string().contains("reason=not_dir"), "{err}");
    assert_eq!(std::fs::read(&path).expect("kept"), b"not a repository");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_at_the_path_or_as_a_leftover_is_refused_and_its_target_kept() {
    let (_d, root) = root();
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("keep"), b"x").expect("target file");
    let id = RepoId::new(74);
    let path = root.path_for(id);
    let shard = path.parent().expect("shard").to_path_buf();
    std::fs::create_dir_all(&shard).expect("shard");
    std::os::unix::fs::symlink(outside.path(), &path).expect("symlink");
    let err = root.remove_until(id, soon()).await.expect_err("symlink");
    assert_eq!(err.code(), ErrorCode::StoragePathRejected, "{err}");
    assert!(err.to_string().contains("reason=symlink"), "{err}");
    std::fs::remove_file(&path).expect("unlink");
    std::os::unix::fs::symlink(outside.path(), shard.join(".purge-74-x.git")).expect("symlink");
    let err = root
        .remove_until(id, soon())
        .await
        .expect_err("leftover symlink");
    assert_eq!(err.code(), ErrorCode::StoragePathRejected, "{err}");
    assert!(outside.path().join("keep").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_shard_is_refused() {
    let (_d, root) = root();
    let outside = tempfile::tempdir().expect("outside");
    let id = RepoId::new(75);
    let path = root.path_for(id);
    let shard = path.parent().expect("shard");
    std::fs::create_dir_all(shard.parent().expect("repos")).expect("repos");
    std::os::unix::fs::symlink(outside.path(), shard).expect("symlink");
    std::fs::create_dir_all(outside.path().join("75.git")).expect("target repo");
    let err = root
        .remove_until(id, soon())
        .await
        .expect_err("symlinked shard");
    assert_eq!(err.code(), ErrorCode::StoragePathRejected, "{err}");
    assert!(outside.path().join("75.git").exists());
}

#[tokio::test]
async fn a_passed_deadline_removes_nothing_and_says_deadline() {
    let (_d, root) = root();
    let (_h, git) = git();
    let id = RepoId::new(76);
    let path = root
        .init_bare(&git, id, Uuid::new_v4(), None)
        .await
        .expect("created");
    let err = root
        .remove_until(id, Instant::now())
        .await
        .expect_err("no time left");
    assert_eq!(err.code(), ErrorCode::StorageIoFailed, "{err}");
    assert!(err.to_string().contains("reason=deadline"), "{err}");
    assert!(path.exists(), "nothing was removed");
}
