//! Plan 18 S9 (ADR-0037 (d) 4): speculative trains. With
//! `merge_queue.speculation_depth` D > 1 a queue whose front train waits
//! for its pipeline builds up to D live batches, each on the previous
//! train's tip. They land in order, each only when the target is its own
//! base; a red or dropped batch drops every batch behind it
//! (`reason=predecessor_failed`, the entries back in order); a target
//! moved by someone else drops them all; a worker that died mid-landing
//! records only what git shows, and the batches behind stay. Depth 1 (the
//! default) is ADR-0030's queue: one live batch, none built behind it.

use super::*;

impl World {
    /// A worker whose queue keeps `depth` live batches of at most `ci`.
    fn speculating(&self, name: &str, depth: usize, ci: usize) -> MergeQueue {
        MergeQueue::new(self.merger.clone(), name, self.rules()).with_limits(QueueLimits {
            poll: Duration::ZERO,
            idle: Duration::ZERO,
            ci_batch_max: ci,
            speculation_depth: depth,
            ..QueueLimits::default()
        })
    }

    /// `n` merge requests `s<i>` adding their own file, all enqueued.
    async fn queued(&mut self, n: usize) -> Vec<StoredMergeRequest> {
        let mut mrs = Vec::with_capacity(n);
        for i in 0..n {
            let name = format!("s{i:02}");
            let mr = self.mr(&name, &[(&format!("{name}.txt"), "x\n")]).await;
            mrs.push(mr);
        }
        for mr in &mrs {
            self.enqueue(mr).await.expect("enqueue");
        }
        mrs
    }

    /// The live batches of the queue, front first.
    async fn live(&self) -> Vec<qdb::StoredBatch> {
        qdb::live_batches(&self.t.db, self.queue_id().await)
            .await
            .expect("live")
    }

    async fn batch_now(&self, id: i64) -> qdb::StoredBatch {
        qdb::batch(&self.t.db, id)
            .await
            .expect("read")
            .expect("batch")
    }

    /// Finish the pipeline of every live testing batch that has none yet:
    /// red when it holds one of `bad`.
    async fn finish_all(&self, bad: &BTreeSet<i64>) {
        for batch in self.live().await {
            if batch.state != BatchState::Testing || self.has_pipeline(&batch.train_sha).await {
                continue;
            }
            let entries = qdb::batch_entries(&self.t.db, batch.id)
                .await
                .expect("entries");
            let red = entries.iter().any(|e| bad.contains(&e.iid));
            self.pipeline(&batch.train_sha, if red { "failed" } else { "succeeded" })
                .await;
        }
    }

    async fn has_pipeline(&self, sha: &ObjectId) -> bool {
        count(
            &self.t,
            &format!(
                "SELECT count(*) AS n FROM forge.ci_pipelines WHERE sha = '{}' \
                 AND source = 'merge_queue'",
                sha.as_str()
            ),
        )
        .await
            > 0
    }

    /// Tick until no queue is due, finishing every live train as `bad`
    /// says; the most live batches seen at once.
    async fn drive_all(&self, worker: &MergeQueue, bad: &BTreeSet<i64>) -> usize {
        let mut most = 0;
        for _ in 0..120 {
            let report = worker.tick().await.expect("tick");
            most = most.max(self.live().await.len());
            self.finish_all(bad).await;
            if report.is_none() {
                return most;
            }
        }
        panic!("the queue never settled");
    }

    async fn merged_events(&self) -> i64 {
        count(
            &self.t,
            "SELECT count(*) AS n FROM forge.events WHERE event_type = 'merge_request.merged'",
        )
        .await
    }

    fn train_ref(&self, queue: i64) -> String {
        plumb(
            &self.path,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                &format!("refs/merge-queue/{queue}/train"),
            ],
            None,
        )
    }
}

/// Build two speculative trains of two entries each (four entries).
async fn two_trains(w: &mut World, worker: &MergeQueue) -> (i64, i64, Vec<StoredMergeRequest>) {
    let mrs = w.queued(4).await;
    let first = worker.tick().await.expect("tick").expect("due");
    assert_eq!(first.outcome, "built");
    let second = worker.tick().await.expect("tick").expect("due");
    assert_eq!(second.outcome, "built", "built behind the waiting train");
    let (a, b) = (first.batch_id.expect("a"), second.batch_id.expect("b"));
    assert_ne!(a, b);
    (a, b, mrs)
}

#[tokio::test]
async fn a_second_train_lands_only_after_the_first() {
    let mut w = world().await;
    let worker = w.speculating("w1", 2, 2);
    let (a, b, mrs) = two_trains(&mut w, &worker).await;
    let queue = w.queue_id().await;
    let (fa, fb) = (w.batch_now(a).await, w.batch_now(b).await);
    // B is built on A's tip, one deeper, A its parent.
    assert_eq!((fa.depth, fa.parent_batch_id), (1, None));
    assert_eq!((fb.depth, fb.parent_batch_id), (2, Some(a)));
    assert_eq!(fa.base_sha.as_str(), w.base);
    assert_eq!(fb.base_sha, fa.train_sha);
    // The one train ref is at the deepest train (A's is its ancestor).
    assert_eq!(w.train_ref(queue), fb.train_sha.as_str());
    // Both trains test at once: two pipeline jobs.
    assert_eq!(train_jobs(&w).await, 2);
    // The depth is full: nothing more is built, the queue waits.
    let full = worker.tick().await.expect("tick").expect("due");
    assert_eq!(full.outcome, "waiting");
    assert_eq!(w.live().await.len(), 2);

    // B goes green first: it may not land before A (its base is not the
    // target yet).
    w.pipeline(&fb.train_sha, "succeeded").await;
    let early = worker.tick().await.expect("tick").expect("due");
    assert_eq!(early.outcome, "waiting");
    assert_eq!(w.tip("main"), w.base, "nothing lands out of order");
    assert_eq!(w.batch_now(b).await.state, BatchState::Testing);

    // A goes green: A lands, B becomes the front (its base is the target).
    w.pipeline(&fa.train_sha, "succeeded").await;
    let landed = worker.tick().await.expect("tick").expect("due");
    assert_eq!((landed.outcome, landed.batch_id), ("landed", Some(a)));
    assert_eq!(w.tip("main"), fa.train_sha.as_str());
    let front = w.batch_now(b).await;
    assert_eq!((front.state, front.depth), (BatchState::Testing, 1));
    assert_eq!(w.train_ref(queue), fb.train_sha.as_str(), "B's train kept");
    let next = worker.tick().await.expect("tick").expect("due");
    assert_eq!((next.outcome, next.batch_id), ("landed", Some(b)));
    assert_eq!(w.tip("main"), fb.train_sha.as_str());
    assert_outcome(&w, &mrs, &BTreeSet::new()).await;
    assert_eq!(w.merged_events().await, 4);
    assert!(w.train_ref(queue).is_empty(), "the train ref is deleted");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_red_first_train_abandons_the_second() {
    let mut w = world().await;
    let worker = w.speculating("w1", 2, 2);
    let (a, b, mrs) = two_trains(&mut w, &worker).await;
    let fa = w.batch_now(a).await;
    // B is green, A (holding the bad entry) red: A bisects, B is dropped
    // although its own pipeline passed (it was built on a red train).
    let bad: BTreeSet<i64> = [mrs[1].iid].into();
    w.finish_all(&BTreeSet::new()).await;
    w.t.exec_superuser(&format!(
        "UPDATE forge.ci_pipelines SET status = 'failed' WHERE sha = '{}'",
        fa.train_sha.as_str()
    ))
    .await;
    let red = worker.tick().await.expect("tick").expect("due");
    assert_eq!((red.outcome, red.batch_id), ("bisected", Some(a)));
    let dropped = w.batch_now(b).await;
    assert_eq!(dropped.state, BatchState::Abandoned);
    assert_eq!(dropped.reason.as_deref(), Some("predecessor_failed"));
    assert!(w.live().await.is_empty());
    // Every entry is back in order; nothing landed.
    for mr in &mrs {
        assert_eq!(w.entry_of(mr).await.state, EntryState::Queued);
    }
    assert_eq!(w.tip("main"), w.base);
    assert!(w.train_ref(w.queue_id().await).is_empty());
    // The bisection goes on; the good entries land, the bad one leaves.
    w.drive_all(&worker, &bad).await;
    assert_outcome(&w, &mrs, &bad).await;
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_target_moved_by_a_direct_push_abandons_every_live_train() {
    let mut w = world().await;
    let worker = w.speculating("w1", 3, 1);
    let mrs = w.queued(3).await;
    let mut built = Vec::new();
    for _ in 0..3 {
        let r = worker.tick().await.expect("tick").expect("due");
        assert_eq!(r.outcome, "built");
        built.push(r.batch_id.expect("id"));
    }
    let depths: Vec<usize> = w.live().await.iter().map(|b| b.depth).collect();
    assert_eq!(depths, [1, 2, 3]);
    w.finish_all(&BTreeSet::new()).await;
    // Someone pushes to main under the three trains.
    let moved = commit(
        &w.path,
        &[w.base_files, &[("other.txt", "o\n")]].concat(),
        Some(&w.base),
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &moved], None);
    let next = worker.tick().await.expect("tick").expect("due");
    assert_eq!((next.outcome, next.batch_id), ("abandoned", Some(built[0])));
    assert_eq!(
        w.batch_now(built[0]).await.reason.as_deref(),
        Some("target_moved")
    );
    for id in &built[1..] {
        let b = w.batch_now(*id).await;
        assert_eq!(b.state, BatchState::Abandoned);
        assert_eq!(b.reason.as_deref(), Some("predecessor_failed"));
    }
    assert!(w.train_ref(w.queue_id().await).is_empty());
    // Rebuilt on the push and landed on top of it.
    w.drive_all(&worker, &BTreeSet::new()).await;
    assert_outcome(&w, &mrs, &BTreeSet::new()).await;
    assert!(w.main_has("other.txt"));
    w.t.drop_now().await;
}

#[tokio::test]
async fn recovery_with_two_live_batches_records_only_what_git_shows() {
    let mut w = world().await;
    let dying = w.speculating("dying", 2, 2).with_crash_after_cas();
    let (a, b, mrs) = two_trains(&mut w, &dying).await;
    w.finish_all(&BTreeSet::new()).await;
    let crashed = dying.tick().await.expect_err("stops after the swap");
    assert_eq!(crashed.code(), ErrorCode::Internal);
    let fa = w.batch_now(a).await;
    assert_eq!(fa.state, BatchState::Landing);
    assert_eq!(w.tip("main"), fa.train_sha.as_str());
    // The restart records A (git shows the target at A's train); B, built
    // on that tip, stays and becomes the front.
    let restarted = w.speculating("restarted", 2, 2);
    let recovered = restarted.tick().await.expect("tick").expect("due");
    assert_eq!(
        (recovered.outcome, recovered.batch_id),
        ("recovered", Some(a))
    );
    assert_eq!(w.batch_now(a).await.state, BatchState::Landed);
    let fb = w.batch_now(b).await;
    assert_eq!((fb.state, fb.depth), (BatchState::Testing, 1));
    for mr in &mrs[..2] {
        assert_eq!(w.entry_of(mr).await.state, EntryState::Merged);
    }
    assert_eq!(w.merged_events().await, 2);
    // A second restart finds nothing of A to record again.
    let again = w.speculating("again", 2, 2);
    let next = again.tick().await.expect("tick").expect("due");
    assert_eq!((next.outcome, next.batch_id), ("landed", Some(b)));
    assert_eq!(w.tip("main"), fb.train_sha.as_str());
    assert_outcome(&w, &mrs, &BTreeSet::new()).await;
    assert_eq!(w.merged_events().await, 4, "every merge recorded once");
    assert!(again.tick().await.expect("tick").is_none());
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_landing_interrupted_before_the_swap_drops_the_trains_behind_it() {
    let mut w = world().await;
    let worker = w.speculating("w1", 2, 2);
    let (a, b, mrs) = two_trains(&mut w, &worker).await;
    // A worker that stopped right after `testing → landing` of A.
    assert!(qdb::begin_landing(&w.t.db, a).await.expect("landing"));
    let recovered = worker.tick().await.expect("tick").expect("due");
    assert_eq!(recovered.outcome, "recovered");
    assert_eq!(
        w.batch_now(a).await.reason.as_deref(),
        Some("landing_interrupted")
    );
    let fb = w.batch_now(b).await;
    assert_eq!(fb.state, BatchState::Abandoned);
    assert_eq!(fb.reason.as_deref(), Some("predecessor_failed"));
    assert_eq!(w.tip("main"), w.base, "git shows nothing landed");
    for mr in &mrs {
        assert_eq!(w.entry_of(mr).await.state, EntryState::Queued);
    }
    w.drive_all(&worker, &BTreeSet::new()).await;
    assert_outcome(&w, &mrs, &BTreeSet::new()).await;
    assert_eq!(w.merged_events().await, 4);
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_entry_leaving_a_speculative_train_drops_it_and_those_behind() {
    let mut w = world().await;
    let worker = w.speculating("w1", 3, 1);
    let mrs = w.queued(3).await;
    let mut built = Vec::new();
    for _ in 0..3 {
        built.push(
            worker
                .tick()
                .await
                .expect("tick")
                .expect("due")
                .batch_id
                .expect("id"),
        );
    }
    // dave takes the second one out while the front still tests.
    let who = session(w.dev);
    let proof = w
        .mr_proof(&who, Action::MergeQueueDequeue, &mrs[1])
        .await
        .expect("proof");
    w.review
        .dequeue(&request("req-dequeue"), &proof)
        .await
        .expect("dequeue");
    let next = worker.tick().await.expect("tick").expect("due");
    // The front waits; the second and third are dropped and the third's
    // entry is built again on the front.
    assert_eq!(next.outcome, "built", "{next:?}");
    assert_eq!(w.batch_now(built[0]).await.state, BatchState::Testing);
    assert_eq!(
        w.batch_now(built[1]).await.reason.as_deref(),
        Some("entry_left")
    );
    assert_eq!(
        w.batch_now(built[2]).await.reason.as_deref(),
        Some("predecessor_failed")
    );
    let live = w.live().await;
    assert_eq!(live.len(), 2);
    assert_eq!(live[1].base_sha, live[0].train_sha);
    // The train ref moved back with the chain and on to the new train.
    assert_eq!(w.train_ref(w.queue_id().await), live[1].train_sha.as_str());
    w.drive_all(&worker, &BTreeSet::new()).await;
    assert_eq!(w.entry_of(&mrs[0]).await.state, EntryState::Merged);
    assert_eq!(w.entry_of(&mrs[1]).await.state, EntryState::Removed);
    assert_eq!(w.entry_of(&mrs[2]).await.state, EntryState::Merged);
    w.t.drop_now().await;
}

#[tokio::test]
async fn depth_1_behaves_like_adr_0030() {
    let mut w = world().await;
    assert_eq!(QueueLimits::default().speculation_depth, 1, "the default");
    let worker = w.speculating("w1", 1, 2);
    let mrs = w.queued(4).await;
    let built = worker.tick().await.expect("tick").expect("due");
    assert_eq!(built.outcome, "built");
    // A waiting train has nothing built behind it.
    let waiting = worker.tick().await.expect("tick").expect("due");
    assert_eq!(waiting.outcome, "waiting");
    assert_eq!(w.live().await.len(), 1);
    assert_eq!(train_jobs(&w).await, 1);
    let bad: BTreeSet<i64> = [mrs[2].iid].into();
    assert_eq!(
        w.drive_all(&worker, &bad).await,
        1,
        "one live batch at most"
    );
    assert_outcome(&w, &mrs, &bad).await;
    let shapes =
        w.t.json_rows(
            "SELECT count(*) FILTER (WHERE depth <> 1 OR parent_batch_id IS NOT NULL) AS n \
             FROM forge.merge_queue_batches",
        )
        .await;
    assert_eq!(shapes[0]["n"].as_i64(), Some(0), "every batch at depth 1");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_new_head_in_a_speculative_train_drops_it_and_leaves_the_queue() {
    let mut w = world().await;
    let worker = w.speculating("w1", 2, 1);
    let mrs = w.queued(2).await;
    let a = worker.tick().await.expect("tick").expect("due");
    let b = worker.tick().await.expect("tick").expect("due");
    assert_eq!((a.outcome, b.outcome), ("built", "built"));
    // A new version of the second merge request while its train tests.
    let newer = commit(
        &w.path,
        &[w.base_files, &[("s01.txt", "y\n")]].concat(),
        Some(&w.base),
    );
    plumb(&w.path, &["update-ref", "refs/heads/s01", &newer], None);
    w.checked(mrs[1].id).await;
    let next = worker.tick().await.expect("tick").expect("due");
    assert_eq!((next.outcome, next.batch_id), ("abandoned", b.batch_id));
    let dropped = w.batch_now(b.batch_id.expect("b")).await;
    assert_eq!(dropped.reason.as_deref(), Some("entry_left"));
    let entry = w.entry_of(&mrs[1]).await;
    assert_eq!(entry.state, EntryState::Removed);
    assert_eq!(entry.reason.as_deref(), Some("head_changed"));
    // The front goes on and lands alone; the ref follows it.
    let front = w.batch_now(a.batch_id.expect("a")).await;
    assert_eq!(front.state, BatchState::Testing);
    assert_eq!(w.train_ref(w.queue_id().await), front.train_sha.as_str());
    w.drive_all(&worker, &BTreeSet::new()).await;
    assert_eq!(w.entry_of(&mrs[0]).await.state, EntryState::Merged);
    w.t.drop_now().await;
}
