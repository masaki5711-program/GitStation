//! `init_bare_or_adopt_until` (D4 MR3b): the provisioning job's deadline
//! bounds every git call and file-system step, and an overrun names the
//! step that hung with a retryable code.

use std::time::{Duration, Instant};

use forge_errors::ErrorCode;

use super::adopt_tests::{git, root};
use super::*;
#[cfg(unix)]
use crate::GitConfig;

#[tokio::test]
async fn a_passed_deadline_stops_before_anything_is_created() {
    let (_d, root) = root();
    let (_h, git) = git();
    let id = RepoId::new(61);
    let err = root
        .init_bare_or_adopt_until(&git, id, Uuid::new_v4(), None, Instant::now())
        .await
        .expect_err("no time left");
    assert_eq!(err.code(), ErrorCode::StorageIoFailed, "{err}");
    let text = err.to_string();
    assert!(text.contains("reason=deadline"), "{text}");
    assert!(text.contains("operation=create_shard"), "{text}");
    assert!(provision_error_is_retryable(&err));
    assert!(!root.path_for(id).exists());
}

#[tokio::test]
async fn a_deadline_with_time_left_provisions_as_usual() {
    let (_d, root) = root();
    let (_h, git) = git();
    let id = RepoId::new(62);
    let token = Uuid::new_v4();
    let deadline = Instant::now() + Duration::from_secs(30);
    let made = root
        .init_bare_or_adopt_until(&git, id, token, None, deadline)
        .await
        .expect("created");
    assert!(matches!(made, Provisioned::Created(_)), "{made:?}");
    let again = root
        .init_bare_or_adopt_until(&git, id, token, None, deadline)
        .await
        .expect("adopted");
    assert!(matches!(again, Provisioned::Adopted(_)), "{again:?}");
}

/// A git that never answers: the call is cut at the deadline (not at its
/// own, longer timeout) as FG-GIT-0002, and the temporary directory goes.
#[cfg(unix)]
#[tokio::test]
async fn a_hanging_git_is_cut_at_the_deadline_as_a_git_timeout() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_d, root) = root();
    let bin = tempfile::tempdir().expect("bin dir");
    let fake = bin.path().join("git");
    // `exec`: the killed child is the sleep itself (no orphan holding the pipes).
    std::fs::write(&fake, "#!/bin/sh\nexec sleep 30\n").expect("fake git");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let home = tempfile::tempdir().expect("home");
    let git = Git::new(GitConfig::new(&fake, home.path()));
    let id = RepoId::new(63);
    let started = Instant::now();
    let err = root
        .init_bare_or_adopt_until(
            &git,
            id,
            Uuid::new_v4(),
            None,
            started + Duration::from_millis(800),
        )
        .await
        .expect_err("git hangs");
    assert_eq!(err.code(), ErrorCode::GitTimeout, "{err}");
    assert!(provision_error_is_retryable(&err));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cut at the deadline"
    );
    assert!(!root.path_for(id).exists());
    let shard = root.path_for(id).parent().expect("shard").to_path_buf();
    let left: Vec<_> = std::fs::read_dir(&shard)
        .expect("shard")
        .filter_map(Result::ok)
        .collect();
    assert!(left.is_empty(), "temporary directory removed: {left:?}");
}
