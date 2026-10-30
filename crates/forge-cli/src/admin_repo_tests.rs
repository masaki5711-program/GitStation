//! `forge admin repo reprovision` against a migrated database: a dead job
//! gets a new attempt that the worker's handler then completes; a live job
//! and a ready repository change nothing.

use std::sync::Arc;

use forge_db::jobs::{self, JobRunner, Ran, WorkerId};
use forge_db::test_support::{assert_reason, FreshDb};
use forge_errors::ErrorCode;

use super::*;
use crate::worker::loop_tests::Captured;
use crate::worker::provision::tests::{project, storage};
use crate::worker::provision::ProvisionHandler;

fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

#[tokio::test]
async fn reprovision_requeues_a_dead_job() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let p = project(&t, "sam").await;
    let repo = p.repository_id;
    // The creating job is still queued: nothing to do.
    let out = reprovision_with(&t.db, &rid("req-cli-rp-1"), repo.get())
        .await
        .expect("live");
    assert!(out.contains("already queued or running"), "{out}");
    // It dies (as after FG-REPO-0004 adopt_refused, once the operator has
    // dealt with the directory).
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET state = 'dead', finished_at = now(), \
         last_error_code = 'FG-REPO-0004' WHERE id = {}",
        p.provision_job.get()
    ))
    .await;
    let logs = Captured::default();
    let guard = logs.install();
    let out = reprovision_with(&t.db, &rid("req-cli-rp-2"), repo.get())
        .await
        .expect("requeued");
    drop(guard);
    assert!(out.contains("queued"), "{out}");
    let events = logs.events();
    let logged: Vec<_> = events
        .iter()
        .filter(|e| e["fields"]["event"] == "forge.admin.repo.reprovisioned")
        .collect();
    assert_eq!(logged.len(), 1, "{events:?}");
    assert_eq!(logged[0]["fields"]["outcome"], "queued");
    assert_eq!(logged[0]["fields"]["request_id"], "req-cli-rp-2");
    // The worker's handler runs the new job and the repository is ready.
    let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
    let handler = ProvisionHandler::new(&t.db, storage.clone()).expect("handler");
    runner.register(Arc::new(handler)).expect("register");
    let ran = runner.run_once().await.expect("claim").expect("a job");
    assert!(
        matches!(ran, Ran::Succeeded(id) if id != p.provision_job),
        "{ran:?}"
    );
    let target = repositories::provision_target(&t.db, repo)
        .await
        .expect("target");
    assert_eq!(target.state, forge_core::RepositoryState::Ready);
    let health = jobs::health(&t.db)
        .await
        .expect("health")
        .expect("migrated");
    assert_eq!(health.dead_total, 1, "the old dead job stays as it was");
    // Ready now: refused with a clear code, nothing enqueued.
    let err = reprovision_with(&t.db, &rid("req-cli-rp-3"), repo.get())
        .await
        .expect_err("ready");
    assert_reason(&err, ErrorCode::ProjectRequestInvalid, "repository_ready");
    let err = reprovision_with(&t.db, &rid("req-cli-rp-4"), 987_654)
        .await
        .expect_err("unknown");
    assert_eq!(err.code(), ErrorCode::NotFound);
    t.drop_now().await;
}
