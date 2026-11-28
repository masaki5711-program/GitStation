//! Lock order against the review commands (a-burst 2026-11-28, M5): every
//! review command locks the project row `FOR UPDATE` first
//! (`review::commands::facts`), then the merge request, then (an enqueue)
//! the queue row. A writer that inserts a row referencing the project
//! takes the project's `FOR KEY SHARE` through the foreign key — which
//! waits for that `FOR UPDATE` — so it must take it before any row a
//! review command locks later, or the two deadlock (`FG-DB-0002
//! reason=deadlock`, logged by the auto-merge as `FG-REVIEW-0028`).
//!
//! Each test holds the review command's first lock in a fixture
//! transaction, starts the writer, waits until it waits on a lock, then
//! takes the review command's next lock: it must be granted, and the
//! writer must finish once the fixture commits.

use std::time::Duration;

use forge_core::{MergeMethod, ObjectId, RequestId};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::merge_results::{
    record_merge_result, replace_merge_result, MergeRequestPipeline,
};
use crate::review::merges::{enqueue_mr_decided, CredentialRef, NewEntry};
use crate::review::{open_mr_decided, NewMergeRequest, StoredMergeRequest};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

struct World {
    t: FreshDb,
    alice: forge_core::UserId,
    web: CreatedProject,
    mr: StoredMergeRequest,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let system = ChangeContext::new(Actor::System, RequestId::parse("req-lo").expect("id"));
    let alice = users::create(
        &t.db,
        &system,
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &system,
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    let new = NewMergeRequest {
        title: "Revise".to_owned(),
        description: "synthetic".to_owned(),
        source_branch: forge_core::RefName::parse("refs/heads/a").expect("ref"),
        target_branch: forge_core::RefName::parse("refs/heads/main").expect("ref"),
        draft: false,
    };
    let as_alice = ChangeContext::new(
        Actor::User(alice.user_id),
        RequestId::parse("req-lo-open").expect("id"),
    );
    let (mr, ()) = open_mr_decided(
        &t.db,
        &as_alice,
        web.project_id,
        alice.user_id,
        &new,
        |_| Ok(()),
    )
    .await
    .expect("open");
    World {
        t,
        alice: alice.user_id,
        web,
        mr,
    }
}

/// The review command's first lock (the project row), held open.
async fn hold_project(w: &World) -> HeldChange {
    let project = w.web.project_id.get();
    w.t.hold_audited(
        ("project", project),
        &[format!(
            "SELECT 1 FROM forge.projects WHERE id = {project} FOR UPDATE"
        )],
    )
    .await
}

/// `create_batch` (the merge-queue worker) against an enqueue (the
/// auto-merge): the enqueue holds the project and upserts the queue row
/// (`merges::queue_on`); the batch locks the queue row and inserts a batch
/// referencing the project.
#[tokio::test]
async fn a_batch_built_while_an_enqueue_holds_the_project_does_not_deadlock() {
    let w = world().await;
    let ctx = ChangeContext::new(
        Actor::User(w.alice),
        RequestId::parse("req-lo-enqueue").expect("id"),
    );
    let new = NewEntry {
        head_sha: oid('a'),
        method: MergeMethod::Squash,
        message: None,
        credential: CredentialRef::Session(1),
    };
    let (entry, ()) = enqueue_mr_decided(
        &w.t.db,
        &ctx,
        (w.web.project_id, w.mr.iid),
        w.alice,
        &new,
        |_| Ok(()),
        |_| "synthetic".to_owned(),
    )
    .await
    .expect("enqueue");
    let mut held = hold_project(&w).await;
    let batch = NewBatch {
        queue: entry.queue_id,
        project: w.web.project_id,
        base: oid('b'),
        train: oid('c'),
        members: vec![BatchMember {
            entry: entry.id,
            train_sha: oid('c'),
            isolate: None,
        }],
        request_id: RequestId::parse("req-lo-batch").expect("id"),
        deadline: Duration::from_secs(600),
        pipeline: None,
        parent: None,
        max_depth: 1,
    };
    let db = w.t.db.clone();
    let built = tokio::spawn(async move { create_batch(&db, &batch).await });
    w.t.until_waiting("forge.").await;
    // The enqueue's next lock: the queue row (`merges::queue_on`).
    let queue = held
        .fetch_text(&format!(
            "INSERT INTO forge.merge_queues (project_id, target_branch) \
             VALUES ({}, 'refs/heads/main') ON CONFLICT (project_id, target_branch) \
             DO UPDATE SET updated_at = now() RETURNING id::text",
            w.web.project_id.get()
        ))
        .await;
    assert_eq!(queue, entry.queue_id.to_string());
    held.commit().await;
    let built = tokio::time::timeout(Duration::from_secs(30), built)
        .await
        .expect("the batch finishes")
        .expect("joined");
    assert!(
        matches!(built, Ok(Some(_))),
        "the batch is built after the enqueue commits: {built:?}"
    );
    w.t.drop_now().await;
}

/// `record_merge_result` (the version job) against a review command: the
/// command holds the project and locks the merge request `FOR UPDATE`;
/// the record inserts a row referencing both.
#[tokio::test]
async fn a_merge_result_recorded_while_a_review_command_holds_the_project_does_not_deadlock() {
    let w = world().await;
    let mut held = hold_project(&w).await;
    let db = w.t.db.clone();
    let mr = w.mr.id;
    let recorded = tokio::spawn(async move {
        record_merge_result(
            &db,
            &RequestId::parse("req-lo-result").expect("id"),
            mr,
            (&oid('a'), &oid('b')),
            &oid('c'),
            MergeRequestPipeline::NoDefinition,
        )
        .await
    });
    w.t.until_waiting("forge.").await;
    // The command's next lock: the merge request row (`review.lock`).
    let locked = held
        .fetch_text(&format!(
            "SELECT id::text FROM forge.merge_requests WHERE id = {} FOR UPDATE",
            mr.get()
        ))
        .await;
    assert_eq!(locked, mr.get().to_string());
    held.commit().await;
    let recorded = tokio::time::timeout(Duration::from_secs(30), recorded)
        .await
        .expect("the record finishes")
        .expect("joined");
    assert!(
        matches!(recorded, Ok(Some(_))),
        "the result is recorded after the command commits: {recorded:?}"
    );
    w.t.drop_now().await;
}

/// `replace_merge_result` (the version job, the result's commit pruned)
/// against a review command: the same order as the record.
#[tokio::test]
async fn a_merge_result_replaced_while_a_review_command_holds_the_project_does_not_deadlock() {
    let w = world().await;
    let origin = RequestId::parse("req-lo-result").expect("id");
    let first = record_merge_result(
        &w.t.db,
        &origin,
        w.mr.id,
        (&oid('a'), &oid('b')),
        &oid('c'),
        MergeRequestPipeline::NoDefinition,
    )
    .await
    .expect("record");
    assert!(first.is_some());
    let mut held = hold_project(&w).await;
    let db = w.t.db.clone();
    let mr = w.mr.id;
    let replaced = tokio::spawn(async move {
        replace_merge_result(
            &db,
            &RequestId::parse("req-lo-replace").expect("id"),
            mr,
            (&oid('a'), &oid('b')),
            (&oid('c'), &oid('d')),
        )
        .await
    });
    w.t.until_waiting("forge.").await;
    let locked = held
        .fetch_text(&format!(
            "SELECT id::text FROM forge.merge_requests WHERE id = {} FOR UPDATE",
            mr.get()
        ))
        .await;
    assert_eq!(locked, mr.get().to_string());
    held.commit().await;
    let replaced = tokio::time::timeout(Duration::from_secs(30), replaced)
        .await
        .expect("the replace finishes")
        .expect("joined");
    assert!(
        matches!(&replaced, Ok(Some(r)) if r.result == oid('d')),
        "the result is replaced after the command commits: {replaced:?}"
    );
    w.t.drop_now().await;
}

/// The merge request read before the project's lock, removed while the
/// writer waited for it (a purge): `Ok(None)` (gone), not the insert's
/// foreign-key error.
async fn remove_mr_while_waiting(w: &World, mut held: HeldChange) {
    let mr = w.mr.id.get();
    w.t.until_waiting("forge.").await;
    for table in [
        "merge_result_refs",
        "merge_request_mergeability",
        "merge_request_versions",
    ] {
        let _n = held
            .fetch_text(&format!(
                "WITH d AS (DELETE FROM forge.{table} WHERE merge_request_id = {mr} RETURNING 1) \
                 SELECT count(*)::text FROM d"
            ))
            .await;
    }
    let removed = held
        .fetch_text(&format!(
            "WITH d AS (DELETE FROM forge.merge_requests WHERE id = {mr} RETURNING id) \
             SELECT count(*)::text FROM d"
        ))
        .await;
    assert_eq!(removed, "1");
    held.commit().await;
}

#[tokio::test]
async fn a_merge_result_of_a_merge_request_removed_meanwhile_is_none() {
    let w = world().await;
    let held = hold_project(&w).await;
    let db = w.t.db.clone();
    let mr = w.mr.id;
    let recorded = tokio::spawn(async move {
        record_merge_result(
            &db,
            &RequestId::parse("req-lo-result").expect("id"),
            mr,
            (&oid('a'), &oid('b')),
            &oid('c'),
            MergeRequestPipeline::NoDefinition,
        )
        .await
    });
    remove_mr_while_waiting(&w, held).await;
    let recorded = tokio::time::timeout(Duration::from_secs(30), recorded)
        .await
        .expect("the record finishes")
        .expect("joined");
    assert!(matches!(recorded, Ok(None)), "{recorded:?}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_merge_result_replaced_on_a_merge_request_removed_meanwhile_is_none() {
    let w = world().await;
    let first = record_merge_result(
        &w.t.db,
        &RequestId::parse("req-lo-result").expect("id"),
        w.mr.id,
        (&oid('a'), &oid('b')),
        &oid('c'),
        MergeRequestPipeline::NoDefinition,
    )
    .await
    .expect("record");
    assert!(first.is_some());
    let held = hold_project(&w).await;
    let db = w.t.db.clone();
    let mr = w.mr.id;
    let replaced = tokio::spawn(async move {
        replace_merge_result(
            &db,
            &RequestId::parse("req-lo-replace").expect("id"),
            mr,
            (&oid('a'), &oid('b')),
            (&oid('c'), &oid('d')),
        )
        .await
    });
    remove_mr_while_waiting(&w, held).await;
    let replaced = tokio::time::timeout(Duration::from_secs(30), replaced)
        .await
        .expect("the replace finishes")
        .expect("joined");
    assert!(matches!(replaced, Ok(None)), "{replaced:?}");
    w.t.drop_now().await;
}

/// A batch for a project that is gone (purged with its queue) writes
/// nothing: `Ok(None)`, not the queue lookup's error.
#[tokio::test]
async fn a_batch_for_a_project_that_is_gone_is_none() {
    let w = world().await;
    let batch = NewBatch {
        queue: i64::MAX,
        project: forge_core::ProjectId::new(i64::MAX),
        base: oid('b'),
        train: oid('c'),
        members: vec![BatchMember {
            entry: i64::MAX,
            train_sha: oid('c'),
            isolate: None,
        }],
        request_id: RequestId::parse("req-lo-gone").expect("id"),
        deadline: Duration::from_secs(600),
        pipeline: None,
        parent: None,
        max_depth: 1,
    };
    let built = create_batch(&w.t.db, &batch).await;
    assert!(matches!(built, Ok(None)), "{built:?}");
    w.t.drop_now().await;
}
