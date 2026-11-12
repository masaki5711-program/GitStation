//! ADR-0016 Amendment 2026-11-12, against a migrated database: a binding
//! that resolves work items detects the project's open and recently merged
//! merge requests again (`work_items.relink` → `merge_request.relink_requested`
//! → `work-item-sync`), and a link or unlink by hand — like the
//! re-detection — reaches the feed as `merge_request.work_items_changed`
//! with its work item. Synthetic data only.

use forge_core::{LinkActorKind, LinkTarget, WorkItemRef};

use super::*;
use crate::integrations::NewBinding;
use crate::review::{link_work_item_mr_decided, unlink_work_item_mr_decided, LinkOutcome};
use crate::work_items::{
    relink_payload, request_relink, request_relink_with, RelinkLimits, Relinked, RELINK_JOB,
};

const CHANGED: &str = "merge_request.work_items_changed";

/// The payloads of the `work_items.relink` jobs, oldest first.
async fn relink_jobs(w: &World) -> Vec<serde_json::Value> {
    sqlx::query_scalar("SELECT payload FROM forge.jobs WHERE kind = $1 ORDER BY id")
        .bind(RELINK_JOB)
        .fetch_all(&mut w.t.superuser().await)
        .await
        .expect("jobs")
}

/// Run the newest `work_items.relink` job as the worker would.
async fn run_relink(w: &World, limits: Option<RelinkLimits>) -> Relinked {
    let jobs = relink_jobs(w).await;
    let request = relink_payload(jobs.last().expect("a relink job")).expect("payload");
    let ctx = system("req-relink-job");
    match limits {
        Some(limits) => request_relink_with(&w.t.db, &ctx, &request, limits).await,
        None => request_relink(&w.t.db, &ctx, &request).await,
    }
    .expect("relink")
}

/// `mr` merged `days` ago (the merge's end state, as `mark_merged`).
async fn merged_days_ago(w: &World, mr: &StoredMergeRequest, days: i32) {
    let id = mr.id.get();
    let p = mr.project_id.get();
    let sha = "d".repeat(40);
    w.t.commit_audited(
        ("project", p),
        &[
            format!(
                "UPDATE forge.merge_requests SET state = 'merged', revision = revision + 1 \
                 WHERE id = {id}"
            ),
            format!(
                "INSERT INTO forge.merge_request_merges (merge_request_id, project_id, method, \
                 requested_by, credential_kind, credential_id, request_id, head_sha, \
                 target_branch, delete_source_branch) VALUES ({id}, {p}, 'merge_commit', {}, \
                 'session', 1, 'req-merge', '{sha}', 'refs/heads/main', false)",
                w.alice.get()
            ),
            format!(
                "UPDATE forge.merge_request_merges SET state = 'prepared', \
                 expected_old = '{sha}', new_sha = '{sha}' WHERE merge_request_id = {id}"
            ),
            format!(
                "UPDATE forge.merge_request_merges SET state = 'executing', \
                 executing_at = now() WHERE merge_request_id = {id}"
            ),
            format!(
                "UPDATE forge.merge_request_merges SET state = 'applied', \
                 finished_at = now() - interval '{days} days' WHERE merge_request_id = {id}"
            ),
        ],
    )
    .await;
}

fn reference(raw: &str) -> WorkItemRef {
    WorkItemRef::parse(raw).expect("ref")
}

async fn link_by_hand(w: &World, mr: &StoredMergeRequest, raw: &str, request: &str) {
    let (outcome, _, ()) = link_work_item_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), request),
        (mr.project_id, mr.iid),
        &reference(raw),
        (w.alice, LinkActorKind::Human),
        |_| Ok(()),
    )
    .await
    .expect("link");
    assert_eq!(outcome, LinkOutcome::Changed);
}

async fn unlink_by_hand(w: &World, mr: &StoredMergeRequest, raw: &str, request: &str) {
    unlink_work_item_mr_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), request),
        (mr.project_id, mr.iid),
        &LinkTarget::Ref(reference(raw)),
        (w.alice, LinkActorKind::Human),
        |_| Ok(()),
    )
    .await
    .expect("unlink");
}

/// (external project, ref, source) of the merge request's open links.
async fn open_links_of(w: &World, mr: &StoredMergeRequest) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT w.external_project_id, w.ref, l.source FROM forge.merge_request_work_items AS l \
         JOIN forge.work_items AS w ON w.id = l.work_item_id \
         WHERE l.merge_request_id = $1 AND l.unlinked_at IS NULL ORDER BY l.id",
    )
    .bind(mr.id.get())
    .fetch_all(&mut w.t.superuser().await)
    .await
    .expect("links")
}

fn s(v: &str) -> String {
    v.to_owned()
}

/// The `merge_request.work_items_changed` events of `events` naming `mr`.
fn changed_of<'e>(events: &'e [FeedEvent], mr: &StoredMergeRequest) -> Vec<&'e FeedEvent> {
    events
        .iter()
        .filter(|e| e.event_type == CHANGED && e.payload["merge_request_id"] == mr.id.get())
        .collect()
}

async fn count_events(w: &World, event_type: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM forge.events WHERE event_type = $1")
        .bind(event_type)
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("count")
}

/// Every binding that resolves work items enqueues one relink job in its
/// own change: the first bind, a re-bind (a new binding id), a shared
/// binding becoming the resolving one. An unchanged bind and a shared
/// bind enqueue nothing.
#[tokio::test]
async fn a_resolving_binding_enqueues_one_relink_job() {
    let w = world().await;
    let p = w.web.project_id;
    let first = relink_jobs(&w).await;
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0]["project_id"], p.get());
    bind(&w, p, "kf-1").await; // unchanged
    assert_eq!(relink_jobs(&w).await.len(), 1);
    unbind(&w, p).await;
    bind(&w, p, "kf-2").await;
    let jobs = relink_jobs(&w).await;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert_ne!(jobs[0]["binding_id"], jobs[1]["binding_id"]);
    // A shared spec project: no job until it becomes the resolving binding.
    let spec = second_project(&w, "spec").await;
    let (_, shared, ()) = integrations::bind_as_decided(
        &w.t.db,
        &system("req-bind-shared"),
        spec.project_id,
        w.alice,
        &NewBinding {
            integration: w.integration,
            external_project_id: "kf-3",
            resolves_work_items: false,
        },
        None,
        |_| Ok(()),
    )
    .await
    .expect("shared bind");
    assert_eq!(relink_jobs(&w).await.len(), 2);
    let (_, _, ()) = integrations::update_binding_decided(
        &w.t.db,
        &system("req-resolve"),
        spec.project_id,
        w.alice,
        w.integration,
        shared.id,
        true,
        None,
        |_| Ok(()),
    )
    .await
    .expect("resolve");
    let jobs = relink_jobs(&w).await;
    assert_eq!(jobs.len(), 3, "{jobs:?}");
    assert_eq!(jobs[2]["binding_id"], shared.id);
    assert_eq!(jobs[2]["project_id"], spec.project_id.get());
    w.t.drop_now().await;
}

/// The live-test gap: after unbind + bind, open merge requests (and ones
/// merged in the last 90 days) are linked again by detection, each with
/// one `merge_request.work_items_changed` the integration reads with its
/// work item, and the development status lists them. Closed merge
/// requests and older merges are not detected again; the internal
/// trigger never reaches the feed; a second run writes nothing.
#[tokio::test]
async fn rebinding_detects_open_and_recently_merged_merge_requests_again() {
    let w = world().await;
    let p = w.web.project_id;
    let open = open_mr(&w, p, "Fix login (PBI-1)", "fix-login").await;
    let recent = open_mr(&w, p, "Feature PBI-2", "feature-two").await;
    let old = open_mr(&w, p, "Old PBI-3", "old-three").await;
    let closed = open_mr(&w, p, "Closed PBI-4", "closed-four").await;
    close_mr(&w, &closed, "req-close-four").await;
    merged_days_ago(&w, &recent, 5).await;
    merged_days_ago(&w, &old, 100).await;
    drain(&w.t).await;
    unbind(&w, p).await;
    drain(&w.t).await;
    for mr in [&open, &recent, &old, &closed] {
        assert!(open_links_of(&w, mr).await.is_empty(), "unbound");
    }
    bind(&w, p, "kf-1").await;
    let start = head(&w).await;
    let ran = run_relink(&w, None).await;
    assert_eq!(
        (ran.binding_gone, ran.open, ran.merged, ran.requested),
        (false, 1, 1, 2),
        "{ran:?}"
    );
    assert!(!ran.open_capped && !ran.merged_capped);
    drain(&w.t).await;
    assert_eq!(
        open_links_of(&w, &open).await,
        [(s("kf-1"), s("PBI-1"), s("auto"))]
    );
    assert_eq!(
        open_links_of(&w, &recent).await,
        [(s("kf-1"), s("PBI-2"), s("auto"))]
    );
    assert!(
        open_links_of(&w, &old).await.is_empty(),
        "merged too long ago"
    );
    assert!(open_links_of(&w, &closed).await.is_empty(), "closed");
    let events = read_all(&w, start).await;
    assert!(
        !types(&events).contains(&"merge_request.relink_requested"),
        "{:?}",
        types(&events)
    );
    for (mr, wanted) in [(&open, "PBI-1"), (&recent, "PBI-2")] {
        let changed = changed_of(&events, mr);
        assert_eq!(changed.len(), 1, "{:?}", types(&events));
        let event = changed[0];
        assert_eq!(refs(event), [("kanflow", "kf-1", wanted)]);
        assert_eq!(event.actor.kind, "system");
        assert_eq!(event.payload["iid"], mr.iid);
        assert_eq!(event.payload["reason"], "relink");
        assert_eq!(event.payload["added"][0]["work_item"]["ref"], wanted);
        assert_eq!(event.payload["added"][0]["source"], "auto");
        assert_eq!(event.payload["removed"], serde_json::json!([]));
    }
    let status = dev_status(&w, "PBI-1", MrStateFilter::All).await;
    assert_eq!(iid_states(&status[0].1), [(open.iid, "open")]);
    let status = dev_status(&w, "PBI-2", MrStateFilter::All).await;
    assert_eq!(iid_states(&status[0].1), [(recent.iid, "merged")]);
    // At least once: a retried job writes only what is missing.
    let before = count_events(&w, "merge_request.relink_requested").await;
    assert_eq!(before, 2);
    let again = run_relink(&w, None).await;
    assert_eq!(again.requested, 0, "{again:?}");
    assert_eq!(
        count_events(&w, "merge_request.relink_requested").await,
        before
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.audit_log WHERE action = 'work_item.relink_requested' \
         AND resource_type = 'project' AND resource_id = $1",
    )
    .bind(p.get())
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("audit");
    assert_eq!(audited, 1);
    w.t.drop_now().await;
}

/// Re-binding to another kanflow project links the references to the new
/// project's work items.
#[tokio::test]
async fn rebinding_to_another_kanflow_project_links_its_work_items() {
    let w = world().await;
    let p = w.web.project_id;
    let mr = open_mr(&w, p, "Fix (PBI-7)", "fix-seven").await;
    drain(&w.t).await;
    unbind(&w, p).await;
    bind(&w, p, "kf-2").await;
    let start = head(&w).await;
    run_relink(&w, None).await;
    drain(&w.t).await;
    assert_eq!(
        open_links_of(&w, &mr).await,
        [(s("kf-2"), s("PBI-7"), s("auto"))]
    );
    let events = read_all(&w, start).await;
    let changed = changed_of(&events, &mr);
    assert_eq!(changed.len(), 1, "{:?}", types(&events));
    assert_eq!(refs(changed[0]), [("kanflow", "kf-2", "PBI-7")]);
    w.t.drop_now().await;
}

/// A work item a person unlinked from a merge request stays unlinked
/// after a re-bind (the I1b rule holds for the re-detection too); the
/// merge request's other references are linked again.
#[tokio::test]
async fn a_persons_unlink_survives_a_rebind() {
    let w = world().await;
    let p = w.web.project_id;
    let mr = open_mr(&w, p, "Fix PBI-1 and PBI-2", "fix-both").await;
    drain(&w.t).await;
    unlink_by_hand(&w, &mr, "PBI-1", "req-unlink-1").await;
    drain(&w.t).await;
    unbind(&w, p).await;
    bind(&w, p, "kf-1").await;
    let start = head(&w).await;
    run_relink(&w, None).await;
    drain(&w.t).await;
    assert_eq!(
        open_links_of(&w, &mr).await,
        [(s("kf-1"), s("PBI-2"), s("auto"))]
    );
    let events = read_all(&w, start).await;
    let changed = changed_of(&events, &mr);
    assert_eq!(changed.len(), 1);
    assert_eq!(refs(changed[0]), [("kanflow", "kf-1", "PBI-2")]);
    w.t.drop_now().await;
}

/// The caps: at most `open` open merge requests (oldest first) are asked
/// for, the cut is reported; batches are separate changes.
#[tokio::test]
async fn relink_is_bounded_per_binding() {
    let w = world().await;
    let p = w.web.project_id;
    let a = open_mr(&w, p, "A PBI-1", "a").await;
    let b = open_mr(&w, p, "B PBI-2", "b").await;
    let c = open_mr(&w, p, "C PBI-3", "c").await;
    drain(&w.t).await;
    unbind(&w, p).await;
    bind(&w, p, "kf-1").await;
    let ran = run_relink(
        &w,
        Some(RelinkLimits {
            open: 2,
            merged: 0,
            batch: 1,
        }),
    )
    .await;
    assert_eq!((ran.open, ran.open_capped, ran.requested), (2, true, 2));
    drain(&w.t).await;
    assert_eq!(open_links_of(&w, &a).await.len(), 1);
    assert_eq!(open_links_of(&w, &b).await.len(), 1);
    assert!(open_links_of(&w, &c).await.is_empty(), "beyond the cap");
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.audit_log WHERE action = 'work_item.relink_requested'",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("audit");
    assert_eq!(audited, 2, "one change per batch");
    w.t.drop_now().await;
}

/// A job whose binding is gone when it runs writes nothing.
#[tokio::test]
async fn relink_after_the_binding_is_gone_writes_nothing() {
    let w = world().await;
    let p = w.web.project_id;
    open_mr(&w, p, "Fix PBI-1", "fix-one").await;
    drain(&w.t).await;
    unbind(&w, p).await;
    let ran = run_relink(&w, None).await;
    assert!(ran.binding_gone, "{ran:?}");
    assert_eq!(ran.requested, 0);
    assert_eq!(count_events(&w, "merge_request.relink_requested").await, 0);
    w.t.drop_now().await;
}

/// The live-test gap 2: a link and an unlink by hand reach the feed as
/// `merge_request.work_items_changed` (kanflow follows `merge_request.*`)
/// with the work item — also the unlinked one — and who did it.
#[tokio::test]
async fn a_link_and_unlink_by_hand_reach_the_feed_as_work_items_changed() {
    let w = world().await;
    let p = w.web.project_id;
    let mr = open_mr(&w, p, "No reference here", "plain").await;
    drain(&w.t).await;
    let start = head(&w).await;
    link_by_hand(&w, &mr, "PBI-9", "req-link-9").await;
    unlink_by_hand(&w, &mr, "PBI-9", "req-unlink-9").await;
    drain(&w.t).await;
    let events = read_all(&w, start).await;
    assert_eq!(
        types(&events),
        ["work_item.linked", CHANGED, "work_item.unlinked", CHANGED]
    );
    let changed = changed_of(&events, &mr);
    assert_eq!(changed.len(), 2);
    for event in &changed {
        assert_eq!(refs(event), [("kanflow", "kf-1", "PBI-9")]);
        assert_eq!(event.actor.kind, "user");
        assert_eq!(event.payload["iid"], mr.iid);
        assert_eq!(event.payload["reason"], "manual");
        assert_eq!(event.payload["by"], w.alice.get());
        assert_eq!(event.payload["by_kind"], "human");
    }
    assert_eq!(changed[0].payload["added"][0]["work_item"]["ref"], "PBI-9");
    assert_eq!(changed[0].payload["added"][0]["source"], "manual");
    assert_eq!(changed[0].payload["removed"], serde_json::json!([]));
    assert_eq!(changed[1].payload["added"], serde_json::json!([]));
    assert_eq!(changed[1].payload["removed"][0]["unlink_source"], "manual");
    // The development status agrees: nothing linked now.
    let status = dev_status(&w, "PBI-9", MrStateFilter::All).await;
    assert!(status[0].1.merge_requests.is_empty());
    w.t.drop_now().await;
}
