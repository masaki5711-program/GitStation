//! Plan 18 S7 (ADR-0037 (b) 5–6): the document fast path. A queue whose
//! base and train have no `.forge-ci.yml` builds batches of
//! `doc_batch_max` (32) and lands each in the tick that built it; a queue
//! with a CI definition keeps `ci_batch_max` (4) and waits for its train's
//! pipeline; a worker that dies after the swap of a same-tick batch is
//! recovered like any landing.

use super::*;

impl World {
    /// `n` merge requests, each adding its own file, all enqueued.
    async fn enqueued(&mut self, n: usize) -> Vec<StoredMergeRequest> {
        let mut mrs = Vec::with_capacity(n);
        for i in 0..n {
            let name = format!("d{i:02}");
            let mr = self.mr(&name, &[(&format!("{name}.txt"), "x\n")]).await;
            mrs.push(mr);
        }
        for mr in &mrs {
            self.enqueue(mr).await.expect("enqueue");
        }
        mrs
    }

    /// Every batch's size, oldest first.
    async fn batch_sizes(&self) -> Vec<i64> {
        self.t
            .json_rows("SELECT size AS n FROM forge.merge_queue_batches ORDER BY id")
            .await
            .iter()
            .map(|r| r["n"].as_i64().expect("size"))
            .collect()
    }
}

#[tokio::test]
async fn a_doc_queue_builds_batches_of_32_and_a_ci_queue_of_4() {
    // Documents: 33 entries are a batch of 32, then one of 1.
    let mut docs = world_with(DOC_BASE).await;
    let mrs = docs.enqueued(33).await;
    let worker = docs.worker("w1");
    let sizes = docs.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [32, 1]);
    assert_eq!(docs.batch_sizes().await, [32, 1]);
    for mr in &mrs {
        assert_eq!(docs.entry_of(mr).await.state, EntryState::Merged);
    }
    // Two compare-and-swaps: the target's first-parent chain is the base
    // and the 33 merge commits.
    let chain = plumb(
        &docs.path,
        &["rev-list", "--first-parent", "refs/heads/main"],
        None,
    );
    assert_eq!(chain.lines().count(), 34);
    docs.t.drop_now().await;

    // CI: 5 entries are a batch of 4, then one of 1.
    let mut ci = world().await;
    let mrs = ci.enqueued(5).await;
    let worker = ci.worker("w1");
    let sizes = ci.drive(&worker, &BTreeSet::new()).await;
    assert_eq!(sizes, [4, 1]);
    for mr in &mrs {
        assert_eq!(ci.entry_of(mr).await.state, EntryState::Merged);
    }
    ci.t.drop_now().await;
}

/// The bounds are the keys' (`merge_queue.doc_batch_max` /
/// `ci_batch_max`), and a document batch that brings a `.forge-ci.yml`
/// in is cut back to the CI bound: the entries after the cut stay queued
/// (nothing is removed for a train that never tests).
#[tokio::test]
async fn a_train_that_brings_a_ci_definition_in_takes_the_ci_bound() {
    let mut w = world_with(DOC_BASE).await;
    // A root entry `.forge-ci.yml` of any kind is a definition for the
    // queue (its pipeline job judges a directory `invalid`); a directory
    // needs no Maintainer approval, so a plain MR brings it in.
    let first = w
        .mr("ci", &[(".forge-ci.yml/readme.txt", "synthetic\n")])
        .await;
    w.enqueue(&first).await.expect("enqueue");
    let rest = w.enqueued(5).await;
    let worker = w.worker("w1").with_limits(QueueLimits {
        poll: Duration::ZERO,
        idle: Duration::ZERO,
        doc_batch_max: 6,
        ci_batch_max: 2,
        ..QueueLimits::default()
    });
    let built = worker.tick().await.expect("tick").expect("due");
    assert_eq!(built.outcome, "built", "a pipeline to wait for");
    assert_eq!(w.batch_sizes().await, [2]);
    assert_eq!(train_jobs(&w).await, 1);
    for mr in &rest[1..] {
        assert_eq!(w.entry_of(mr).await.state, EntryState::Queued);
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_doc_batch_lands_in_the_tick_that_built_it() {
    let mut w = world_with(DOC_BASE).await;
    let mrs = w.enqueued(3).await;
    let worker = w.worker("w1");
    let report = worker.tick().await.expect("tick").expect("due");
    assert_eq!(report.outcome, "landed");
    let batch = qdb::batch(&w.t.db, report.batch_id.expect("id"))
        .await
        .expect("read")
        .expect("batch");
    assert_eq!(batch.state, BatchState::Landed);
    assert_eq!(batch.size, 3);
    assert_eq!(w.tip("main"), batch.train_sha.as_str());
    for mr in &mrs {
        assert_eq!(w.entry_of(mr).await.state, EntryState::Merged);
        assert_eq!(w.state_of(mr).await, forge_core::MergeRequestState::Merged);
    }
    // No pipeline and no job: the verdict was in process.
    assert_eq!(train_jobs(&w).await, 0);
    let pipelines = count(&w.t, "SELECT count(*) AS n FROM forge.ci_pipelines").await;
    assert_eq!(pipelines, 0);
    // Nothing left: the next tick finds the queue idle.
    let next = worker.tick().await.expect("tick");
    assert!(
        next.as_ref().is_none_or(|r| r.outcome == "idle"),
        "{next:?}"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_restart_mid_landing_recovers_a_same_tick_batch() {
    let mut w = world_with(DOC_BASE).await;
    let mrs = w.enqueued(2).await;
    let dying = w.worker("dying").with_crash_after_cas();
    // The landing stops after the swap: the tick still reports its build
    // (`retry`, the failure logged) and the batch is left `landing`.
    let crashed = dying.tick().await.expect("tick").expect("due");
    assert_eq!(crashed.outcome, "retry");
    let batches =
        w.t.json_rows("SELECT id, state, train_sha FROM forge.merge_queue_batches")
            .await;
    assert_eq!(batches.len(), 1, "{batches:?}");
    assert_eq!(batches[0]["state"].as_str(), Some("landing"));
    let train = batches[0]["train_sha"].as_str().expect("train").to_owned();
    assert_eq!(w.tip("main"), train, "the swap happened");
    for mr in &mrs {
        let entry = w.entry_of(mr).await;
        let attempt = merges::merge_attempt(&w.t.db, entry.merge_id.expect("written"))
            .await
            .expect("read")
            .expect("attempt");
        assert_eq!(attempt.state, MergeState::Executing);
    }
    // A fresh worker (the restart) records what is left.
    let restarted = w.worker("restarted");
    let recovered = restarted.tick().await.expect("tick").expect("due");
    assert_eq!(recovered.outcome, "recovered");
    for mr in &mrs {
        assert_eq!(w.entry_of(mr).await.state, EntryState::Merged);
        assert_eq!(w.state_of(mr).await, forge_core::MergeRequestState::Merged);
    }
    let landed = qdb::batch(&w.t.db, batches[0]["id"].as_i64().expect("id"))
        .await
        .expect("read")
        .expect("batch");
    assert_eq!(landed.state, BatchState::Landed);
    assert_eq!(w.tip("main"), train, "the target was not moved twice");
    w.t.drop_now().await;
}
