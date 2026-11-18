//! Path-aware mergeability against a real bare repository and the
//! development database (plan 18 S4, ADR-0037 (c)), wired as `forge serve`
//! and the worker wire it: the check stores the head's changed paths, a
//! landing says what it changed, the `merge-request-refresh` subscriber
//! carries the results the landing cannot interact with — and the train
//! stays the authority: a carried "mergeable" that conflicts with what the
//! queue lands in front of it never lands (`FG-REVIEW-0006`), and its
//! carried answer is replaced by a computed one.
//!
//! The repository has no CI definition, so a train is green at once and
//! the merge is what decides.

use forge_core::MergeabilityStatus;
use forge_db::review::{mergeability, stored_paths, MergeRequestRefresh, StoredMergeability};
use forge_db::subscribers::{self, Progress};

use super::*;

/// `review.paths_max`'s default, as the worker and `forge serve` pass it.
const PATHS_MAX: usize = 5000;

/// A world whose landings say what they changed (`review.path_filter`).
async fn world_with_paths() -> World {
    let mut w = world_with(DOC_BASE).await;
    w.merger = w.merger.clone().with_paths(Some(PATHS_MAX)); // clone: a builder over the world's merger
    w
}

/// Check `mr` again as the worker does with the path filter on: the
/// result is stored with the head's changed-path set.
async fn check_with_paths(w: &World, mr: &StoredMergeRequest) {
    let seq = request_check(&w.t.db, mr.id, RequestId::parse("req-q-paths").expect("id"))
        .await
        .expect("request")
        .expect("open");
    let checker = Mergeability::new(w.git.clone(), w.storage.clone()).with_paths(Some(PATHS_MAX));
    check(
        &w.t.db,
        &checker,
        MergeabilityRequest {
            merge_request: mr.id,
            check_seq: seq,
        },
        &RequestId::parse("req-check-paths").expect("id"),
    )
    .await
    .expect("check");
}

/// Sequence the events written so far and let the refresh subscriber
/// handle every one (the worker's loop, here to the end).
async fn refresh_after_landing(w: &World, refresher: &MergeRequestRefresh) {
    forge_db::sequencer::run_once(
        &w.t.db,
        forge_db::sequencer::BatchSize::MAX,
        forge_db::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
    loop {
        match subscribers::run_once(&w.t.db, refresher)
            .await
            .expect("step")
        {
            Progress::Handled { .. } => {}
            Progress::CaughtUp => break,
            other @ (Progress::Busy
            | Progress::Disabled
            | Progress::Failed(_)
            | Progress::Stuck(_)
            | Progress::Quarantined(_)) => panic!("unexpected {other:?}"),
        }
    }
}

async fn stored(w: &World, mr: &StoredMergeRequest) -> StoredMergeability {
    mergeability(&w.t.db, mr.id)
        .await
        .expect("read")
        .expect("a row")
}

#[tokio::test]
async fn a_carried_mr_that_conflicts_in_the_train_is_removed_with_fg_review_0006() {
    let mut w = world_with_paths().await;
    // a and b both add `shared.txt`, differently: each merges cleanly into
    // `main` alone, never both. c touches another file.
    let a = w.mr("pa", &[("shared.txt", "from a\n")]).await;
    let b = w.mr("pb", &[("shared.txt", "from b\n")]).await;
    let c = w.mr("pc", &[("other.txt", "c\n")]).await;
    check_with_paths(&w, &a).await;
    check_with_paths(&w, &b).await;
    let base = oid(&w.base);
    for mr in [&a, &b] {
        let paths = stored_paths(&w.t.db, mr.id)
            .await
            .expect("read")
            .expect("a stored set");
        assert!(paths.is_complete(), "!{}", mr.iid);
        assert_eq!(paths.files(), ["shared.txt"], "!{}", mr.iid);
        let row = stored(&w, mr).await;
        assert_eq!(row.status, MergeabilityStatus::CanBeMerged);
        assert_eq!(row.computed_target_sha.as_ref(), Some(&base));
        assert_eq!(row.carried_from(), None, "computed, not carried yet");
    }

    // c lands through the queue; its record says it changed `other.txt`.
    let refresher = MergeRequestRefresh::new(50).with_path_filter(true);
    subscribers::register(&w.t.db, &refresher, subscribers::Start::Beginning)
        .await
        .expect("register");
    let worker = w.worker("w1");
    w.enqueue(&c).await.expect("enqueue c");
    w.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(
        w.state_of(&c).await,
        forge_core::MergeRequestState::Merged,
        "c lands"
    );
    let tip = oid(&w.tip("main"));
    assert_ne!(tip, base);
    refresh_after_landing(&w, &refresher).await;

    // Neither a nor b can interact with `other.txt`: both results are kept
    // and answer for the new tip, saying where they were computed.
    let mut seqs = Vec::new();
    for mr in [&a, &b] {
        let row = stored(&w, mr).await;
        assert_eq!(row.status, MergeabilityStatus::CanBeMerged, "!{}", mr.iid);
        assert_eq!(row.checked_target_sha.as_ref(), Some(&tip), "!{}", mr.iid);
        assert_eq!(row.carried_from(), Some(&base), "!{} is carried", mr.iid);
        seqs.push(row.check_seq);
    }

    // The carried answers let both into the queue (the merge conditions
    // count them for the live tip) …
    w.enqueue(&a).await.expect("enqueue a");
    w.enqueue(&b).await.expect("enqueue b");
    w.drive(&worker, &BTreeSet::new()).await;

    // … and the train is the authority: a lands, b conflicts with it and
    // is removed with FG-REVIEW-0006. `main` holds a's file only.
    let entry_a = w.entry_of(&a).await;
    assert_eq!(entry_a.state, EntryState::Merged);
    assert_eq!(w.state_of(&a).await, forge_core::MergeRequestState::Merged);
    let entry_b = w.entry_of(&b).await;
    assert_eq!(entry_b.state, EntryState::Removed);
    assert_eq!(entry_b.reason.as_deref(), Some("conflict"));
    assert_eq!(
        entry_b.error_code.as_deref(),
        Some(ErrorCode::MergeRequestConflict.id())
    );
    assert_eq!(ErrorCode::MergeRequestConflict.id(), "FG-REVIEW-0006");
    assert_eq!(w.state_of(&b).await, forge_core::MergeRequestState::Open);
    assert_eq!(
        plumb(
            &w.path,
            &["cat-file", "-p", "refs/heads/main:shared.txt"],
            None
        ),
        "from a"
    );

    // The disproved carried answer is not left standing: a computed check
    // was requested (a new seq; the row no longer says "mergeable") …
    let after = stored(&w, &b).await;
    assert!(after.check_seq > seqs[1], "a new check was requested");
    assert_ne!(after.status, MergeabilityStatus::CanBeMerged);
    // … and that check says what the train found.
    check_with_paths(&w, &b).await;
    let computed = stored(&w, &b).await;
    assert_eq!(computed.status, MergeabilityStatus::CannotBeMerged);
    assert_eq!(computed.carried_from(), None);
    assert_eq!(
        computed.checked_target_sha.as_ref(),
        Some(&oid(&w.tip("main")))
    );
}

#[tokio::test]
async fn a_landing_on_the_same_file_rechecks_instead_of_carrying() {
    let mut w = world_with_paths().await;
    let a = w.mr("sa", &[("shared.txt", "from a\n")]).await;
    let b = w.mr("sb", &[("shared.txt", "from b\n")]).await;
    check_with_paths(&w, &a).await;
    check_with_paths(&w, &b).await;
    let before = stored(&w, &b).await;
    assert_eq!(before.status, MergeabilityStatus::CanBeMerged);

    let refresher = MergeRequestRefresh::new(50).with_path_filter(true);
    subscribers::register(&w.t.db, &refresher, subscribers::Start::Beginning)
        .await
        .expect("register");
    w.enqueue(&a).await.expect("enqueue a");
    w.drive(&w.worker("w1"), &BTreeSet::new()).await;
    assert_eq!(w.state_of(&a).await, forge_core::MergeRequestState::Merged);
    refresh_after_landing(&w, &refresher).await;

    // a's landing changed b's file: nothing is kept, a check is requested.
    let after = stored(&w, &b).await;
    assert_eq!(after.status, MergeabilityStatus::Unchecked);
    assert!(after.check_seq > before.check_seq);
    assert_eq!(after.carried_from(), None);
    // Not mergeable for the queue until the check has run.
    let refused = w
        .enqueue(&b)
        .await
        .expect_err("not checked for the new tip");
    assert_eq!(refused.code(), ErrorCode::MergeBlocked);
    assert!(refused
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "mergeability_unknown"));
}
