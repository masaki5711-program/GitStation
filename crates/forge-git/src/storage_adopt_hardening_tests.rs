//! Adoption hardening (review of D4 MR3a): includes, duplicate markers,
//! non-bare / garbage config, permissions, history kept, rename races and the
//! retryable-error rule.

use forge_errors::ErrorCode;

use super::adopt_tests::{config_set, git, plain_bare_repo, root, MARKER};
use super::*;

async fn assert_refused(root: &StorageRoot, git: &Git, raw: i64, token: Uuid, reason: &str) {
    let err = root
        .init_bare_or_adopt(git, RepoId::new(raw), token, None)
        .await
        .expect_err("must not adopt");
    assert_eq!(err.code(), ErrorCode::RepoPathOccupied, "{err}");
    assert!(
        err.to_string().contains(&format!("adopt_refused={reason}")),
        "expected {reason}: {err}"
    );
    assert!(!provision_error_is_retryable(&err), "{err}");
}

fn append_include(repo: &Path, section: &str, included: &Path) {
    let mut config = std::fs::read_to_string(repo.join("config")).expect("read config");
    config.push_str(&format!("[{section}]\n\tpath = {}\n", included.display()));
    std::fs::write(repo.join("config"), config).expect("write config");
}

/// `--local` still follows `include.path`: a token carried by an included
/// file (possibly outside the root) must not make a directory adoptable.
#[tokio::test]
async fn a_token_carried_by_an_include_is_refused() {
    let (_d, root) = root();
    let (_h, git) = git();
    let token = Uuid::new_v4();
    let outside = tempfile::tempdir().expect("outside");
    let included = outside.path().join("marker.cfg");
    std::fs::write(&included, format!("[forge]\n\tprovisionToken = {token}\n")).expect("include");

    for (raw, section) in [(41, "include"), (42, "includeIf \"gitdir:/\"")] {
        let path = root.path_for(RepoId::new(raw));
        plain_bare_repo(&git, &path).await;
        append_include(&path, section, &included);
        assert_refused(&root, &git, raw, token, "includes").await;
    }
    // Also when the repository carries its own matching marker.
    let path = root
        .init_bare(&git, RepoId::new(43), token, None)
        .await
        .expect("init");
    append_include(&path, "include", &included);
    assert_refused(&root, &git, 43, token, "includes").await;
}

#[tokio::test]
async fn a_duplicated_marker_is_refused() {
    let (_d, root) = root();
    let (_h, git) = git();
    let token = Uuid::new_v4();
    let path = root
        .init_bare(&git, RepoId::new(44), token, None)
        .await
        .expect("init");
    git.command("config")
        .repo(&path)
        .arg("--add")
        .arg(MARKER)
        .arg_owned(token.to_string())
        .output()
        .await
        .expect("add second marker");
    assert_refused(&root, &git, 44, token, "duplicate_marker").await;
}

#[tokio::test]
async fn a_non_bare_or_garbage_config_is_refused() {
    let (_d, root) = root();
    let (_h, git) = git();
    let token = Uuid::new_v4();
    let path = root
        .init_bare(&git, RepoId::new(45), token, None)
        .await
        .expect("init");
    config_set(&git, &path, "core.bare", "false").await;
    assert_refused(&root, &git, 45, token, "not_bare").await;

    let garbage = root.path_for(RepoId::new(46));
    std::fs::create_dir_all(&garbage).expect("dir");
    std::fs::write(garbage.join("config"), [0xff_u8, 0x00, b'[', b'\n', b'=']).expect("garbage");
    assert_refused(&root, &git, 46, token, "unreadable").await;
}

#[cfg(unix)]
#[tokio::test]
async fn an_uninspectable_config_is_a_permanent_refusal() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, root) = root();
    let (_h, git) = git();
    let token = Uuid::new_v4();
    let path = root
        .init_bare(&git, RepoId::new(47), token, None)
        .await
        .expect("init");
    // An unsearchable repository directory: `config` cannot be inspected.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    if std::fs::symlink_metadata(path.join("config")).is_ok() {
        // Running as root: permissions are not enforced, nothing to check.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        return;
    }
    let result = root
        .init_bare_or_adopt(&git, RepoId::new(47), token, None)
        .await;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let err = result.expect_err("uninspectable config");
    assert_eq!(err.code(), ErrorCode::RepoPathOccupied, "{err}");
    assert!(
        err.to_string().contains("adopt_refused=unreadable"),
        "{err}"
    );
    assert!(!provision_error_is_retryable(&err));
}

#[tokio::test]
async fn adopting_a_repository_with_history_keeps_it() {
    let (_d, root) = root();
    let (_h, git) = git();
    let token = Uuid::new_v4();
    let path = root
        .init_bare(&git, RepoId::new(48), token, None)
        .await
        .expect("init");
    let blob = git
        .command("hash-object")
        .repo(&path)
        .arg("-w")
        .arg("--stdin")
        .stdin(b"synthetic content\n".to_vec())
        .output()
        .await
        .expect("hash-object")
        .stdout_str()
        .trim()
        .to_string();
    git.command("update-ref")
        .repo(&path)
        .arg("refs/tags/keep")
        .arg_owned(&blob)
        .output()
        .await
        .expect("update-ref");

    let adopted = root
        .init_bare_or_adopt(&git, RepoId::new(48), token, None)
        .await
        .expect("adopt");
    assert!(matches!(adopted, Provisioned::Adopted(_)), "{adopted:?}");
    let kept = git
        .command("rev-parse")
        .repo(adopted.path())
        .arg("--verify")
        .arg("refs/tags/keep")
        .output()
        .await
        .expect("ref kept")
        .stdout_str();
    assert_eq!(kept.trim(), blob);
}

/// Two attempts racing for the same path: the loser of the rename adopts the
/// winner's repository when the token is the same.
#[tokio::test]
async fn concurrent_attempts_with_the_same_token_create_once_and_adopt_once() {
    let (_d, root) = root();
    let (_h, git) = git();
    let token = Uuid::new_v4();
    let id = RepoId::new(51);
    let (a, b) = tokio::join!(
        root.init_bare_or_adopt(&git, id, token, None),
        root.init_bare_or_adopt(&git, id, token, None),
    );
    let (a, b) = (a.expect("first"), b.expect("second"));
    let created = [&a, &b]
        .iter()
        .filter(|p| matches!(p, Provisioned::Created(_)))
        .count();
    assert_eq!(created, 1, "{a:?} {b:?}");
    assert_eq!(a.path(), b.path());
}

/// ... and is refused when the tokens differ.
#[tokio::test]
async fn concurrent_attempts_with_different_tokens_create_once_and_refuse_once() {
    let (_d, root) = root();
    let (_h, git) = git();
    let id = RepoId::new(52);
    let (a, b) = tokio::join!(
        root.init_bare_or_adopt(&git, id, Uuid::new_v4(), None),
        root.init_bare_or_adopt(&git, id, Uuid::new_v4(), None),
    );
    let (ok, err) = match (a, b) {
        (Ok(p), Err(e)) | (Err(e), Ok(p)) => (p, e),
        other => panic!("expected one success and one refusal: {other:?}"),
    };
    assert!(matches!(ok, Provisioned::Created(_)), "{ok:?}");
    assert_eq!(err.code(), ErrorCode::RepoPathOccupied, "{err}");
    assert!(
        err.to_string().contains("adopt_refused=token_mismatch"),
        "{err}"
    );
    assert!(!provision_error_is_retryable(&err));
}

#[test]
fn retryable_errors_are_the_transient_ones() {
    for code in [
        ErrorCode::GitTimeout,
        ErrorCode::GitBusy,
        ErrorCode::GitUnavailable,
        ErrorCode::GitProcessFailed,
        ErrorCode::StorageIoFailed,
    ] {
        assert!(
            provision_error_is_retryable(&ForgeError::new(code)),
            "{code}"
        );
    }
    for code in [
        ErrorCode::RepoPathOccupied,
        ErrorCode::StoragePathRejected,
        ErrorCode::StorageMissing,
        ErrorCode::ConfigInvalid,
        ErrorCode::GitOutputTooLarge,
    ] {
        assert!(
            !provision_error_is_retryable(&ForgeError::new(code)),
            "{code}"
        );
    }
}
