//! `repositories.provisioning` and `repositories.storage` against a
//! migrated database and a temporary storage root.

use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::repositories::PROVISIONING_STUCK_AFTER;
use forge_db::test_support::FreshDb;

use super::*;
use crate::doctor::Outcome;
use crate::worker::provision::tests::project;

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn root() -> (tempfile::TempDir, StorageRoot) {
    let dir = tempfile::tempdir().expect("storage root");
    let root = StorageRoot::open(dir.path()).expect("open root");
    (dir, root)
}

fn failure(check: &Check) -> &ForgeError {
    match &check.outcome {
        Outcome::Fail(e) => e,
        Outcome::Ok(d) | Outcome::Warn(d) => panic!("{}: not a failure: {d}", check.name),
        Outcome::Skip(why) => panic!("{}: skipped: {why}", check.name),
    }
}

fn detail(check: &Check) -> (&'static str, &str) {
    match &check.outcome {
        Outcome::Ok(d) => ("ok", d),
        Outcome::Warn(d) => ("warn", d),
        Outcome::Skip(why) => ("skip", why),
        Outcome::Fail(e) => panic!("{}: failed: {e}", check.name),
    }
}

async fn ready(t: &FreshDb, repo: RepoId) {
    let _ = repositories::mark_ready(&t.db, &ctx(&format!("req-ready-{}", repo.get())), repo)
        .await
        .expect("ready");
}

#[tokio::test]
async fn a_repository_stuck_in_provisioning_is_reported_by_doctor() {
    let t = FreshDb::migrated().await;
    let (_dir, root) = root();
    // No repository at all: both checks pass.
    let [provisioning, storage] =
        repository_checks(&t.db, Some(&root), PROVISIONING_STUCK_AFTER).await;
    assert_eq!(detail(&provisioning), ("ok", "0 provisioning, none stuck"));
    assert_eq!(detail(&storage).0, "ok");
    // A fresh project waits for its queued job: not stuck yet.
    let p = project(&t, "noa").await;
    let [provisioning, _] = repository_checks(&t.db, Some(&root), PROVISIONING_STUCK_AFTER).await;
    assert_eq!(detail(&provisioning), ("ok", "1 provisioning, none stuck"));
    // Its job died (e.g. FG-REPO-0004 adopt_refused): stuck at once.
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET state = 'dead', finished_at = now(), \
         last_error_code = 'FG-REPO-0004' WHERE id = {}",
        p.provision_job.get()
    ))
    .await;
    let [provisioning, storage] =
        repository_checks(&t.db, Some(&root), PROVISIONING_STUCK_AFTER).await;
    let err = failure(&provisioning);
    assert_eq!(err.code(), ErrorCode::RepositoryProvisioningStuck);
    assert_eq!(err.code().severity(), forge_errors::Severity::Alert);
    let text = err.to_string();
    for part in [
        "reason=job_not_live".to_owned(),
        format!("repo_id={}", p.repository_id.get()),
        format!("project_id={}", p.project_id.get()),
        "job_state=none".to_owned(),
    ] {
        assert!(text.contains(&part), "{part}: {text}");
    }
    // Still provisioning, so not a ready repository without storage.
    assert_eq!(detail(&storage).0, "ok");
    // Waiting past the threshold with a live job is stuck too.
    let q = project(&t, "oli").await;
    let [provisioning, _] = repository_checks(&t.db, Some(&root), Duration::ZERO).await;
    let err = failure(&provisioning);
    assert!(err.to_string().contains("stuck=2"), "{err}");
    // The oldest first: the dead one.
    assert!(
        err.to_string()
            .contains(&format!("repo_id={}", p.repository_id.get())),
        "{err}"
    );
    let _ = q;
    t.drop_now().await;
}

#[tokio::test]
async fn a_ready_repository_without_storage_is_reported() {
    let t = FreshDb::migrated().await;
    let (_dir, root) = root();
    let a = project(&t, "pia").await;
    let b = project(&t, "rui").await;
    ready(&t, a.repository_id).await;
    ready(&t, b.repository_id).await;
    std::fs::create_dir_all(root.path_for(a.repository_id)).expect("a's directory");
    let check = storage_check(&t.db, &root, STORAGE_CHECK_CAP).await;
    let err = failure(&check);
    assert_eq!(err.code(), ErrorCode::StorageMissing);
    let text = err.to_string();
    for part in [
        format!("repo_id={}", b.repository_id.get()),
        "missing=1".to_owned(),
        "checked=2".to_owned(),
        "truncated=false".to_owned(),
    ] {
        assert!(text.contains(&part), "{part}: {text}");
    }
    // Restored: ok.
    std::fs::create_dir_all(root.path_for(b.repository_id)).expect("b's directory");
    let check = storage_check(&t.db, &root, STORAGE_CHECK_CAP).await;
    assert_eq!(
        detail(&check),
        ("ok", "2 ready repositories have their directory")
    );
    // Over the cap: the rest is not hidden.
    let check = storage_check(&t.db, &root, 1).await;
    let (status, text) = detail(&check);
    assert_eq!(status, "warn");
    assert!(text.contains("truncated at 1"), "{text}");
    // Without a storage root the check is skipped, not passed.
    let [_, storage] = repository_checks(&t.db, None, PROVISIONING_STUCK_AFTER).await;
    assert_eq!(detail(&storage), ("skip", "storage root unavailable"));
    t.drop_now().await;
}

/// A ready repository whose path is rejected (a file where its directory
/// should be) is counted as `other`, next to the missing ones.
#[tokio::test]
async fn a_rejected_storage_path_is_counted_as_other() {
    let t = FreshDb::migrated().await;
    let (_dir, root) = root();
    let a = project(&t, "sol").await;
    let b = project(&t, "tia").await;
    ready(&t, a.repository_id).await;
    ready(&t, b.repository_id).await;
    let file = root.path_for(a.repository_id);
    std::fs::create_dir_all(file.parent().expect("shard")).expect("shard");
    std::fs::write(&file, b"not a repository").expect("a file in its place");
    // b is missing: FG-REPO-0002 comes first, both counts with it.
    let check = storage_check(&t.db, &root, STORAGE_CHECK_CAP).await;
    let err = failure(&check);
    assert_eq!(err.code(), ErrorCode::StorageMissing);
    let text = err.to_string();
    for part in ["missing=1", "other=1", "checked=2"] {
        assert!(text.contains(part), "{part}: {text}");
    }
    // b restored: the rejection is reported with its own code.
    std::fs::create_dir_all(root.path_for(b.repository_id)).expect("b's directory");
    let check = storage_check(&t.db, &root, STORAGE_CHECK_CAP).await;
    let err = failure(&check);
    assert_eq!(err.code(), ErrorCode::StoragePathRejected);
    let text = err.to_string();
    for part in ["missing=0", "other=1", "checked=2"] {
        assert!(text.contains(part), "{part}: {text}");
    }
    t.drop_now().await;
}

/// The walk is bounded as a whole: out of time, it fails with FG-REPO-0003
/// `check_timeout` and how far it got, never an ok.
#[tokio::test]
async fn a_storage_walk_out_of_time_fails_with_check_timeout() {
    let t = FreshDb::migrated().await;
    let (_dir, root) = root();
    let a = project(&t, "uma").await;
    ready(&t, a.repository_id).await;
    let check = storage_check_within(&t.db, &root, STORAGE_CHECK_CAP, Duration::ZERO).await;
    let err = failure(&check);
    assert_eq!(err.code(), ErrorCode::StorageIoFailed);
    let text = err.to_string();
    for part in ["reason=check_timeout", "checked=0"] {
        assert!(text.contains(part), "{part}: {text}");
    }
    t.drop_now().await;
}
