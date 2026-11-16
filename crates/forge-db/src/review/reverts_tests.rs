//! The revert draft's database side against a migrated database (M4 C10):
//! the job payload, the once-per-merge record of what the draft did, and
//! the two reads the draft job asks. The subscriber's whole path (a real
//! merge, a failed `ci/pipeline`, the job) is forge-review's
//! `tests/revert_git.rs`.

use forge_core::{MergeRequestChange, ObjectId, ProjectId, RequestId, RevertDraftOutcome, UserId};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::{open_mr_decided, NewMergeRequest};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn sha(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("sha")
}

fn branch(name: &str) -> RefName {
    RefName::parse(&format!("refs/heads/{name}")).expect("branch")
}

async fn web(t: &FreshDb) -> (UserId, CreatedProject) {
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &system("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    (alice.user_id, web)
}

#[test]
fn the_payload_round_trips_and_refuses_anything_else() {
    let request = RevertDraftRequest {
        project: ProjectId::new(7),
        sha: sha('a'),
    };
    let job = revert_draft_job(&request, RequestId::generate(), Some(3)).expect("job");
    assert_eq!(job.kind.as_str(), REVERT_DRAFT_JOB);
    assert_eq!(
        job.dedup_key.as_ref().map(DedupKey::as_str),
        Some(format!("mr_revert_draft:7:{}", "a".repeat(40)).as_str())
    );
    assert_eq!(revert_draft_payload(&job.payload).expect("parse"), request);
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"project_id": 0, "sha": "a".repeat(40)}),
        serde_json::json!({"project_id": 7, "sha": "HEAD"}),
        serde_json::json!({"project_id": "7", "sha": "a".repeat(40)}),
    ] {
        let err = revert_draft_payload(&bad).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::Internal, "{bad}");
    }
}

/// What the draft did is recorded once per merge attempt (the event's dedup
/// key): a retried job writes nothing more.
#[tokio::test]
async fn a_revert_draft_is_recorded_once_per_merge() {
    let t = FreshDb::migrated().await;
    let (alice, web) = web(&t).await;
    let p = web.project_id;
    let new = NewMergeRequest {
        title: "Add the feature".to_owned(),
        description: "synthetic".to_owned(),
        source_branch: branch("feature"),
        target_branch: branch("main"),
        draft: false,
    };
    let (mr, ()) = open_mr_decided(&t.db, &system("req-open"), p, alice, &new, |_| Ok(()))
        .await
        .expect("open");
    let change = || MergeRequestChange::RevertDrafted {
        merge_request: mr.id,
        iid: mr.iid,
        merge_id: 41,
        target_branch: branch("main"),
        merged_sha: sha('b'),
        outcome: RevertDraftOutcome::Opened,
        revert_iid: Some(2),
        conflict_files: 0,
    };
    assert!(
        record_revert_drafted(&t.db, &system("req-draft"), p, change())
            .await
            .expect("first")
    );
    assert!(
        !record_revert_drafted(&t.db, &system("req-again"), p, change())
            .await
            .expect("second")
    );
    let rows = t
        .json_rows(
            "SELECT event_type, aggregate_id, dedup_key, payload FROM forge.events \
             WHERE event_type = 'merge_request.revert_drafted'",
        )
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["aggregate_id"], mr.id.get());
    assert_eq!(
        rows[0]["dedup_key"],
        format!("merge_request_revert_drafted:{}:41", mr.id.get())
    );
    assert_eq!(rows[0]["payload"]["outcome"], "opened");
    assert_eq!(rows[0]["payload"]["revert_iid"], 2);
    // Another kind of change is a defect of the caller, never written.
    let other = MergeRequestChange::Closed {
        merge_request: mr.id,
        iid: mr.iid,
    };
    let err = record_revert_drafted(&t.db, &system("req-other"), p, other)
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::Internal);
    t.drop_now().await;
}

/// No applied merge produced a commit, and no push pipeline ran on it:
/// both reads answer `None`.
#[tokio::test]
async fn the_reads_answer_none_for_an_unknown_commit() {
    let t = FreshDb::migrated().await;
    let (_, web) = web(&t).await;
    assert!(applied_merge_at(&t.db, web.project_id, &sha('c'))
        .await
        .expect("read")
        .is_none());
    assert!(
        push_pipeline_status(&t.db, web.project_id, &branch("main"), &sha('c'))
            .await
            .expect("read")
            .is_none()
    );
    let mut su = t.superuser().await;
    sqlx::query(
        "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, status, request_id, \
           protected, copy_mark) \
         VALUES ($1, 'refs/heads/main', $2, 'push', 'failed', 'req-pipe', true, 'open')",
    )
    .bind(web.project_id.get())
    .bind(sha('c').as_str())
    .execute(&mut su)
    .await
    .expect("pipeline");
    assert_eq!(
        push_pipeline_status(&t.db, web.project_id, &branch("main"), &sha('c'))
            .await
            .expect("read")
            .as_deref(),
        Some("failed")
    );
    t.drop_now().await;
}
