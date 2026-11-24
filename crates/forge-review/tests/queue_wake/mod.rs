//! The merge queue's and auto-merge's wakes (plan 18 S3, ADR-0037 (a))
//! against a real bare repository, the development database and a real
//! LISTEN connection (`forge_db::wake::WakeHub`), with the production
//! safety-net periods (idle 30 s, waiting for a pipeline 60 s): what used
//! to wait for a poll now happens well before any period ends.
//!
//! - an enqueue, a resume and a dequeue end an idle turn's wait;
//! - a train pipeline the runner finished lands at once;
//! - a lost notification is recovered by the safety net and reported
//!   (`FG-DB-0024`) only when the queue had been ready for a whole period;
//! - a completed check run is sequenced, announced, and auto-merge decides
//!   the merge request it names;
//! - two queues progress on two turns, and one queue is never stepped by
//!   two turns at once;
//! - without a live listener a turn and the auto-merge worker poll by
//!   themselves.
//!
//! The writers use the hub-less handles of `World`, so what wakes a turn
//! here is the database's NOTIFY (another process's path) unless a test
//! says otherwise.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use forge_db::ci_runners::{self, lease};
use forge_db::pipelines::{
    self, CopyMark, NewCiJob, NewPipeline, PipelineRequest, Planned, Source,
};
use forge_db::sequencer::{self, AuditSequencing, BatchSize};
use forge_db::wake::{Channel, WakeHub, Woken};
use forge_db::Db;
use forge_review::{AutoMergeWake, AutoMergeWatch, QueueTurn, Roused};
use tokio::sync::Notify;

use super::*;

/// The bound of a step that must happen at all.
const SOON: Duration = Duration::from_secs(30);
/// "Woken, not polled": far below every safety-net period (30 s, 60 s),
/// and room for a landing's git work on a busy machine.
const WELL_BEFORE: Duration = Duration::from_secs(10);

/// The production periods: nothing in these tests may wait for one.
fn nets() -> QueueLimits {
    QueueLimits {
        idle: Duration::from_secs(30),
        poll: Duration::from_secs(60),
        ..QueueLimits::default()
    }
}

/// Wait until `$cond` holds (looked at every 25 ms); the time it took.
macro_rules! until {
    ($limit:expr, $what:literal, $cond:expr) => {{
        let began = Instant::now();
        loop {
            if $cond {
                break began.elapsed();
            }
            assert!(began.elapsed() < $limit, "never happened: {}", $what);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }};
}

/// The turn's idle wait is in progress (it began before `$change` and
/// has a whole period to go), then `$change` happens: how the wait ended.
/// Only a wake ends it well before its period.
macro_rules! roused_by {
    ($turn:expr, $change:expr) => {{
        let idle = $turn.idle();
        tokio::pin!(idle);
        assert!(
            tokio::time::timeout(Duration::from_millis(700), &mut idle)
                .await
                .is_err(),
            "the turn is in its idle wait"
        );
        $change;
        tokio::time::timeout(WELL_BEFORE, &mut idle)
            .await
            .expect("woken well before the period")
            .expect("idle")
    }};
}

/// A wake hub listening on every channel of `w`'s database (up before this
/// returns: a waiter created now misses nothing), and the database handle
/// that carries it.
async fn hub_on(w: &World) -> (WakeHub, Db) {
    let hub = WakeHub::spawn(&w.t.db, &Channel::ALL).expect("hub");
    let began = Instant::now();
    while !Channel::ALL.iter().all(|c| hub.is_live(*c)) {
        assert!(began.elapsed() < SOON, "the listener never came up");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // clone: handles; the hub rides on this copy only
    let db =
        w.t.db
            .clone()
            .with_wake(hub.clone(), Duration::from_secs(10));
    (hub, db)
}

/// The queue's worker on `db` (the handle with the hub).
fn queue_on(w: &World, db: &Db, name: &str, limits: QueueLimits) -> MergeQueue {
    let merger =
        Merger::new(db.clone(), w.git.clone(), w.storage.clone()).with_browser(w.browser.clone());
    MergeQueue::new(merger, name, w.rules()).with_limits(limits)
}

/// The loop of `forge serve`'s turn task: tick until no queue is due, then
/// the idle wait. A failing tick (the merge lock of a repository whose
/// other queue is landing) is tried again, as the serve loop does.
fn run(mut turn: QueueTurn) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match turn.tick().await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if turn.idle().await.is_err() {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
    })
}

/// Tick until no queue is due and the turn's idle wait does not end by
/// itself (a notification of the setup is consumed).
async fn settled(turn: &mut QueueTurn) {
    for _ in 0..20 {
        while turn.tick().await.expect("tick").is_some() {}
        match tokio::time::timeout(Duration::from_millis(700), turn.idle()).await {
            Ok(roused) => {
                roused.expect("idle");
            }
            Err(_) => return,
        }
    }
    panic!("the turn never went idle");
}

async fn stop(tasks: Vec<tokio::task::JoinHandle<()>>, hub: &WakeHub) {
    for task in tasks {
        task.abort();
    }
    hub.stop(SOON).await.expect("stop");
}

/// The `FG-DB-0024` lines among the captured log lines.
fn missed_wakes(logs: &Captured) -> Vec<serde_json::Value> {
    logs.lines()
        .into_iter()
        .filter(|e| e["fields"]["error_code"] == "FG-DB-0024")
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_enqueue_builds_a_batch_without_waiting_for_the_idle_period() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let (hub, db) = hub_on(&w).await;
    let task = run(queue_on(&w, &db, "w", nets()).turn(0));
    // The turn found no queue and is in its idle wait (30 s).
    tokio::time::sleep(Duration::from_millis(500)).await;
    w.enqueue(&a).await.expect("enqueue");
    let queue = w.queue_id().await;
    let took = until!(
        Duration::from_secs(45),
        "a batch",
        qdb::live_batch(&w.t.db, queue)
            .await
            .expect("batch")
            .is_some()
    );
    assert!(
        took < WELL_BEFORE,
        "the enqueue woke the turn; the batch came after {took:?}"
    );
    stop(vec![task], &hub).await;
}

/// The queue `queue` is released waiting for its train's pipeline (its
/// next look by itself is the waiting safety net, a minute away).
async fn waits_for_its_pipeline(w: &World, queue: i64) -> bool {
    count(
        &w.t,
        &format!(
            "SELECT count(*) AS n FROM forge.merge_queues q WHERE q.id = {queue} \
             AND q.lease_owner IS NULL AND q.next_run_at > now() + interval '30 seconds' \
             AND EXISTS (SELECT 1 FROM forge.merge_queue_batches b \
                          WHERE b.queue_id = q.id AND b.state = 'testing')"
        ),
    )
    .await
        == 1
}

/// The train's pipeline as the pipeline-create job writes it.
fn train_pipeline(w: &World, train: &ObjectId, jobs: Vec<NewCiJob>) -> NewPipeline {
    NewPipeline {
        ai_eval_job: None,
        request: PipelineRequest {
            project: w.project,
            git_ref: forge_core::RefName::parse("refs/heads/main").expect("ref"),
            sha: train.clone(), // clone: the request owns its sha
            source: Source::MergeQueue,
            merge_request: None,
            triggered_by: None,
        },
        request_id: "req-plan".to_owned(),
        definition_hash: Some(pipelines::definition_hash(b"version: 1")),
        protected: false,
        copy_mark: CopyMark::Open,
        planned: Planned::Jobs(jobs),
    }
}

fn build_job() -> NewCiJob {
    NewCiJob {
        name: "build".to_owned(),
        stage: "test".to_owned(),
        wave: 0,
        needs: Vec::new(),
        image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
        timeout_s: 600,
        tags: Vec::new(),
        secrets: Vec::new(),
        manual: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_train_pipeline_lands_without_waiting_for_the_poll() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let queue = w.queue_id().await;
    let (hub, db) = hub_on(&w).await;
    let task = run(queue_on(&w, &db, "w", nets()).turn(0));
    until!(
        SOON,
        "the queue waits for its pipeline",
        waits_for_its_pipeline(&w, queue).await
    );
    // The turn is in its idle wait by now.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let batch = qdb::live_batch(&w.t.db, queue)
        .await
        .expect("batch")
        .expect("a batch");
    // The runner's path: the pipeline, its one job claimed and reported.
    let runner = ci_runners::register(
        &w.t.db,
        &forge_db::change::ChangeContext::new(
            forge_db::change::Actor::User(w.owner),
            RequestId::parse("req-reg").expect("id"),
        ),
        &ci_runners::NewRunner {
            name: "r1".to_owned(),
            description: String::new(),
            tags: Vec::new(),
        },
    )
    .await
    .expect("runner")
    .runner
    .id;
    pipelines::create(
        &w.t.db,
        &train_pipeline(&w, &batch.train_sha, vec![build_job()]),
    )
    .await
    .expect("pipeline");
    let job = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("job");
    assert_eq!(w.state_of(&a).await, forge_core::MergeRequestState::Open);
    let began = Instant::now();
    let done = lease::complete(
        &w.t.db,
        runner,
        job.job_id,
        job.attempt,
        lease::JobResult::Succeeded,
    )
    .await
    .expect("complete");
    assert_eq!(done.pipeline_status, "succeeded");
    until!(
        Duration::from_secs(75),
        "the train landed",
        w.state_of(&a).await == forge_core::MergeRequestState::Merged
    );
    let took = began.elapsed();
    assert!(
        took < WELL_BEFORE,
        "the job report woke the turn; the landing came after {took:?}"
    );
    assert!(w.main_has("fa.txt"));
    stop(vec![task], &hub).await;
}

/// A pipeline with nothing to run is born `skipped`: the batch's verdict
/// exists at the insert, and the queue is told there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_train_pipeline_born_terminal_lands_without_waiting_for_the_poll() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let queue = w.queue_id().await;
    let (hub, db) = hub_on(&w).await;
    let task = run(queue_on(&w, &db, "w", nets()).turn(0));
    until!(
        SOON,
        "the queue waits for its pipeline",
        waits_for_its_pipeline(&w, queue).await
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let batch = qdb::live_batch(&w.t.db, queue)
        .await
        .expect("batch")
        .expect("a batch");
    let began = Instant::now();
    pipelines::create(&w.t.db, &train_pipeline(&w, &batch.train_sha, Vec::new()))
        .await
        .expect("pipeline");
    until!(
        Duration::from_secs(75),
        "the train landed",
        w.state_of(&a).await == forge_core::MergeRequestState::Merged
    );
    let took = began.elapsed();
    assert!(took < WELL_BEFORE, "the landing came after {took:?}");
    stop(vec![task], &hub).await;
}

/// The turn is idle with a batch waiting for its pipeline; the pipeline
/// then finishes green WITHOUT its notification (a silent row), and the
/// queue's due time is set to `due` (SQL) as the sender would have set it.
/// Returns how the idle wait that follows ended.
async fn lost_wake(w: &World, turn: &mut QueueTurn, queue: i64, due: &str) -> Roused {
    for _ in 0..20 {
        while turn.tick().await.expect("tick").is_some() {}
        let idle = turn.idle();
        tokio::pin!(idle);
        match tokio::time::timeout(Duration::from_secs(1), &mut idle).await {
            // The enqueue's own wake, or a due time: look again.
            Ok(roused) => {
                roused.expect("idle");
            }
            // One second into the safety-net period: the wake gets lost.
            Err(_) => {
                let batch = qdb::live_batch(&w.t.db, queue)
                    .await
                    .expect("batch")
                    .expect("a batch");
                assert_eq!(batch.state, BatchState::Testing);
                w.pipeline(&batch.train_sha, "succeeded").await;
                w.t.exec_superuser(&format!(
                    "UPDATE forge.merge_queues SET next_run_at = {due} WHERE id = {queue}"
                ))
                .await;
                return idle.await.expect("idle");
            }
        }
    }
    panic!("the turn never went idle");
}

/// The limits of the lost-wake tests: a safety net short enough to wait
/// out, the waiting period as in production.
fn short_net() -> QueueLimits {
    QueueLimits {
        idle: Duration::from_secs(4),
        poll: Duration::from_secs(60),
        ..QueueLimits::default()
    }
}

/// Current-thread runtime: the turn is driven inline, so its log lines are
/// written on the thread that captures them.
#[tokio::test]
async fn a_lost_wake_is_recovered_by_the_safety_net() {
    let logs = Captured::default();
    let _guard = logs.install();
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let queue = w.queue_id().await;
    let (hub, db) = hub_on(&w).await;
    let mut turn = queue_on(&w, &db, "w", short_net()).turn(0);
    let began = Instant::now();
    // The pipeline finished ten seconds ago and nobody was told.
    let roused = lost_wake(&w, &mut turn, queue, "now() - interval '10 seconds'").await;
    while turn.tick().await.expect("tick").is_some() {}
    assert_eq!(
        w.state_of(&a).await,
        forge_core::MergeRequestState::Merged,
        "the safety net found the finished train"
    );
    assert!(
        began.elapsed() < Duration::from_secs(4) + Duration::from_secs(10),
        "within one safety-net period (plus the setup): {:?}",
        began.elapsed()
    );
    let missed = missed_wakes(&logs);
    assert_eq!(missed.len(), 1, "one missed wake reported: {missed:?}");
    let context = missed[0]["fields"]["context"].as_str().unwrap_or_default();
    for field in [
        "channel=forge_merge_queue",
        "oldest_ready_ms=",
        "safety_poll_ms=4000",
    ] {
        assert!(context.contains(field), "{field} in {context}");
    }
    assert_eq!(roused, Roused::SafetyNet);
    hub.stop(SOON).await.expect("stop");
}

/// The S2 rule: a queue that became ready DURING the safety-net period was
/// found by the net, but no wake is provably lost — nothing is reported.
#[tokio::test]
async fn a_queue_ready_for_less_than_the_safety_period_reports_no_missed_wake() {
    let logs = Captured::default();
    let _guard = logs.install();
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let queue = w.queue_id().await;
    let (hub, db) = hub_on(&w).await;
    let mut turn = queue_on(&w, &db, "w", short_net()).turn(0);
    // Ready one second into the four-second wait: three seconds at most.
    let roused = lost_wake(&w, &mut turn, queue, "now()").await;
    assert_eq!(roused, Roused::SafetyNet);
    while turn.tick().await.expect("tick").is_some() {}
    assert_eq!(w.state_of(&a).await, forge_core::MergeRequestState::Merged);
    let missed = missed_wakes(&logs);
    assert!(missed.is_empty(), "nothing provably lost: {missed:?}");
    hub.stop(SOON).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_merge_decides_a_merge_request_right_after_its_check_completes() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let (hub, db) = hub_on(&w).await;
    let review = Review::new(db.clone(), Arc::new(w.browser.clone()))
        .with_review_git(ReviewGit::new(w.git.clone(), w.storage.clone()));
    let merger =
        Merger::new(db.clone(), w.git.clone(), w.storage.clone()).with_browser(w.browser.clone());
    // The production limits: a merge request looked at a moment ago is not
    // due again for twenty seconds — only a wake that names it decides it.
    let auto = forge_review::AutoMerger::new(review, merger);
    let mut watch: AutoMergeWatch = auto.watch(Duration::from_secs(30));
    assert!(watch.waits_on_wake());
    let first = watch.wait().await;
    assert_eq!(first, AutoMergeWake::Sweep, "a new watch sweeps first");
    watch.act(first).await.expect("sweep");
    assert_eq!(
        w.auto_state(&a).await.map(|s| s.0),
        Some("waiting".to_owned()),
        "no forge/risk run yet"
    );
    assert!(w.live_entry(&a).await.is_none());
    // What the sweep itself announced (a mergeability answer) is consumed:
    // from here on the watch waits for its next sweep, 30 s away.
    while let Ok(wake) = tokio::time::timeout(Duration::from_millis(500), watch.wait()).await {
        assert_ne!(wake, AutoMergeWake::Sweep, "the sweep is 30 s away");
        watch.act(wake).await.expect("act");
    }
    assert!(w.live_entry(&a).await.is_none());

    // The check completes; the sequencer (`forge worker`) numbers its event.
    let mut seen = hub.subscribe(Channel::Events);
    let began = Instant::now();
    w.risk(&a, CheckConclusion::Success, "No risk found").await;
    sequencer::run_once(&w.t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("sequence");
    assert!(
        matches!(seen.wait(SOON).await, Woken::Notified(_)),
        "a completed check run is an event, and its number is announced"
    );
    let mut wakes = Vec::new();
    while w.live_entry(&a).await.is_none() {
        assert!(wakes.len() < 10, "never queued: {wakes:?}");
        let wake = tokio::time::timeout(WELL_BEFORE, watch.wait())
            .await
            .expect("a wake well before the sweep");
        wakes.push(wake.clone()); // clone: kept for the assertions
        watch.act(wake).await.expect("act");
    }
    let took = began.elapsed();
    assert!(took < WELL_BEFORE, "queued after {took:?}");
    assert!(
        wakes.contains(&AutoMergeWake::Events),
        "decided from the event: {wakes:?}"
    );
    assert!(
        !wakes
            .iter()
            .any(|w| matches!(w, AutoMergeWake::Sweep | AutoMergeWake::Poll)),
        "not by the sweep: {wakes:?}"
    );
    hub.stop(SOON).await.expect("stop");
}

/// Branch `name` one commit ahead of the base with `files`, and its merge
/// request into `target` (checked as the worker does), by dave.
async fn mr_to(
    w: &mut World,
    name: &str,
    target: &str,
    files: &[(&str, &str)],
) -> StoredMergeRequest {
    let mut all = w.base_files.to_vec();
    all.extend_from_slice(files);
    let head = commit(&w.path, &all, Some(&w.base));
    plumb(
        &w.path,
        &["update-ref", &format!("refs/heads/{name}"), &head],
        None,
    );
    w.browser.invalidate(w.repo);
    let proof = open_proof(&w.t, &session(w.dev), w.project, (name, target), false)
        .await
        .expect("open proof");
    let mr = w
        .review
        .open(
            &request(&format!("req-open-{name}")),
            &proof,
            ready(w.repo),
            &NewFields {
                title: format!("Change {name}"),
                description: String::new(),
            },
        )
        .await
        .expect("open");
    w.checked(mr.id).await;
    w.review
        .merge_request(w.project, mr.iid)
        .await
        .expect("read")
        .expect("mr")
}

/// `merge_queue.workers = 2`: while one turn is inside the landing of
/// `main`'s queue, the other builds the batch of `release`'s queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_queues_progress_concurrently() {
    let mut w = world_with(DOC_BASE).await;
    plumb(
        &w.path,
        &["update-ref", "refs/heads/release", &w.base],
        None,
    );
    w.browser.invalidate(w.repo);
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let b = mr_to(&mut w, "fb", "release", &[("fb.txt", "x\n")]).await;
    let (hub, db) = hub_on(&w).await;

    // The first landing (main's) stops after its fence until `go`.
    let entered = Arc::new(Notify::new());
    let go = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let gave_up = Arc::new(AtomicBool::new(false));
    let hook: forge_review::AfterFence = {
        let (entered, go) = (Arc::clone(&entered), Arc::clone(&go));
        let (calls, gave_up) = (Arc::clone(&calls), Arc::clone(&gave_up));
        Arc::new(move || {
            let (entered, go) = (Arc::clone(&entered), Arc::clone(&go));
            let (calls, gave_up) = (Arc::clone(&calls), Arc::clone(&gave_up));
            Box::pin(async move {
                if calls.fetch_add(1, Ordering::SeqCst) > 0 {
                    return;
                }
                entered.notify_one();
                let waited = tokio::time::timeout(Duration::from_secs(40), go.notified()).await;
                if waited.is_err() {
                    gave_up.store(true, Ordering::SeqCst);
                }
            })
        })
    };
    let queue = queue_on(&w, &db, "w", nets()).with_after_fence(hook);
    let tasks: Vec<_> = queue.turns(2).into_iter().map(run).collect();

    w.enqueue(&a).await.expect("a into main's queue");
    tokio::time::timeout(SOON, entered.notified())
        .await
        .expect("main's landing began");
    w.enqueue(&b).await.expect("b into release's queue");
    let release = w
        .review
        .merge_queue(w.project, &branch("release"), 50)
        .await
        .expect("view")
        .queue
        .expect("release's queue")
        .id;
    assert_ne!(release, w.queue_id().await, "two queues");
    until!(
        Duration::from_secs(20),
        "release's batch while main's landing is held",
        qdb::live_batch(&w.t.db, release)
            .await
            .expect("batch")
            .is_some()
    );
    assert!(
        !gave_up.load(Ordering::SeqCst),
        "main's landing is still held"
    );
    assert_eq!(
        w.state_of(&a).await,
        forge_core::MergeRequestState::Open,
        "main's landing has not finished"
    );
    go.notify_one();
    until!(
        Duration::from_secs(60),
        "both queues landed",
        w.state_of(&a).await == forge_core::MergeRequestState::Merged
            && w.state_of(&b).await == forge_core::MergeRequestState::Merged
    );
    assert!(w.main_has("fa.txt"));
    assert!(!w.main_has("fb.txt"), "b went to release");
    stop(tasks, &hub).await;
}

/// The claim's lease is what keeps one queue to one turn: every turn of
/// every process may wake for it, exactly one steps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_queue_is_never_stepped_by_two_turns_at_once() {
    let mut w = world_with(DOC_BASE).await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let b = w.mr("fb", &[("fb.txt", "x\n")]).await;
    let c = w.mr("fc", &[("fc.txt", "x\n")]).await;
    let (hub, db) = hub_on(&w).await;

    // Inside every landing: wake the other turn, try to claim the queue as
    // a third worker, and stay long enough for both to try.
    let inside = Arc::new(AtomicUsize::new(0));
    let most = Arc::new(AtomicUsize::new(0));
    let landings = Arc::new(AtomicUsize::new(0));
    let intruded = Arc::new(AtomicBool::new(false));
    let hook: forge_review::AfterFence = {
        let (inside, most) = (Arc::clone(&inside), Arc::clone(&most));
        let (landings, intruded) = (Arc::clone(&landings), Arc::clone(&intruded));
        let (hub, db) = (hub.clone(), db.clone()); // clone: handles
        Arc::new(move || {
            let (inside, most) = (Arc::clone(&inside), Arc::clone(&most));
            let (landings, intruded) = (Arc::clone(&landings), Arc::clone(&intruded));
            let (hub, db) = (hub.clone(), db.clone()); // clone: handles
            Box::pin(async move {
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                landings.fetch_add(1, Ordering::SeqCst);
                hub.nudge(Channel::MergeQueue, "0");
                match qdb::claim(&db, "intruder", Duration::from_secs(300)).await {
                    Ok(None) => {}
                    Ok(Some(_)) | Err(_) => intruded.store(true, Ordering::SeqCst),
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                inside.fetch_sub(1, Ordering::SeqCst);
            })
        })
    };
    let queue = queue_on(&w, &db, "w", nets()).with_after_fence(hook);
    let mut first = queue.turn(0);
    let mut second = queue.turn(1);
    assert_eq!((first.worker(), second.worker()), ("w-0", "w-1"));

    // Both turns claim the one due queue at the same moment: one gets it.
    w.enqueue(&a).await.expect("enqueue");
    let (x, y) = tokio::join!(first.tick(), second.tick());
    let stepped = [x.expect("tick"), y.expect("tick")];
    assert_eq!(
        stepped.iter().filter(|r| r.is_some()).count(),
        1,
        "one turn stepped the queue: {stepped:?}"
    );

    // Plan 18 S7: `a` landed in that very tick (a document queue), so `b`
    // and `c` are checked again against the new `main` before they enqueue.
    w.checked(b.id).await;
    w.checked(c.id).await;

    // Then both run, as `merge_queue.workers = 2` does.
    let tasks = vec![run(first), run(second)];
    w.enqueue(&b).await.expect("enqueue");
    w.enqueue(&c).await.expect("enqueue");
    until!(
        Duration::from_secs(90),
        "all three landed",
        w.state_of(&a).await == forge_core::MergeRequestState::Merged
            && w.state_of(&b).await == forge_core::MergeRequestState::Merged
            && w.state_of(&c).await == forge_core::MergeRequestState::Merged
    );
    assert!(landings.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        most.load(Ordering::SeqCst),
        1,
        "never two landings of the queue at once"
    );
    assert!(
        !intruded.load(Ordering::SeqCst),
        "a queue under a lease is nobody else's"
    );
    stop(tasks, &hub).await;
}

/// The in-process path: `MergeQueue::wake` ends the idle wait of the turns
/// of this process without the database.
#[tokio::test]
async fn a_wake_from_this_process_ends_the_idle_wait() {
    let w = world().await;
    let (hub, db) = hub_on(&w).await;
    let queue = queue_on(&w, &db, "w", nets());
    let mut turn = queue.turn(0);
    assert!(turn.waits_on_wake());
    settled(&mut turn).await;
    let began = Instant::now();
    let roused = roused_by!(turn, queue.wake(42));
    assert_eq!(roused, Roused::Wake);
    assert!(began.elapsed() < Duration::from_secs(2), "at once");
    hub.stop(SOON).await.expect("stop");
}

#[tokio::test]
async fn a_resume_and_a_dequeue_wake_an_idle_turn() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let b = w.mr("fb", &[("fb.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    w.enqueue(&b).await.expect("enqueue");
    let pause = project_proof(&w, &session(w.dev), Action::MergeQueuePause).await;
    w.review
        .set_queue_paused(&request("req-pause"), &pause, &branch("main"), true)
        .await
        .expect("pause");
    let (hub, db) = hub_on(&w).await;
    let mut turn = queue_on(&w, &db, "w", nets()).turn(0);
    // The paused queue is looked at every idle period (30 s).
    settled(&mut turn).await;
    let queue = w.queue_id().await;
    assert!(qdb::live_batch(&w.t.db, queue)
        .await
        .expect("batch")
        .is_none());

    let resume = project_proof(&w, &session(w.owner), Action::MergeQueueManage).await;
    let roused = roused_by!(
        turn,
        w.review
            .set_queue_paused(&request("req-resume"), &resume, &branch("main"), false)
            .await
            .expect("resume")
    );
    assert_eq!(roused, Roused::Wake, "woken by the resume");
    let built = turn.tick().await.expect("tick").expect("due at once");
    assert_eq!(built.outcome, "built");

    // The batch waits for its pipeline (60 s); an entry leaves.
    settled(&mut turn).await;
    let proof = w
        .mr_proof(&session(w.dev), Action::MergeQueueDequeue, &a)
        .await
        .expect("proof");
    let roused = roused_by!(
        turn,
        w.review
            .dequeue(&request("req-deq"), &proof)
            .await
            .expect("dequeue")
    );
    assert_eq!(roused, Roused::Wake, "woken by the dequeue");
    // The batch lost an entry: the queue is due at once.
    turn.tick().await.expect("tick").expect("due at once");
    hub.stop(SOON).await.expect("stop");
}

/// No hub at all (a process that does not listen): the turn's idle wait is
/// the short poll, whatever the safety-net period says.
#[tokio::test]
async fn without_a_listener_a_turn_polls_by_itself() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let mut turn = w.worker("w").with_limits(nets()).turn(0);
    assert!(!turn.waits_on_wake());
    assert!(turn.tick().await.expect("tick").is_none());
    let began = Instant::now();
    let (roused, ()) = tokio::join!(turn.idle(), async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        w.enqueue(&a).await.expect("enqueue");
    });
    assert_eq!(roused.expect("idle"), Roused::Poll);
    let waited = began.elapsed();
    assert!(
        waited >= Duration::from_millis(1500)
            && waited < QueueTurn::FALLBACK_POLL + Duration::from_secs(3),
        "the short poll, not the 30 s period: {waited:?}"
    );
    let built = turn.tick().await.expect("tick").expect("the enqueue");
    assert_eq!(built.outcome, "built");
}

/// A hub whose listener is gone: the wait that sees the loss ends at once,
/// the next ones are the short poll, and work found that way is no missed
/// wake (`FG-DB-0015` already said the listener is down).
#[tokio::test]
async fn a_turn_whose_listener_went_down_polls_by_itself_and_reports_no_missed_wake() {
    let logs = Captured::default();
    let _guard = logs.install();
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let (hub, db) = hub_on(&w).await;
    let mut turn = queue_on(&w, &db, "w", nets()).turn(0);
    assert!(turn.waits_on_wake());
    settled(&mut turn).await;
    hub.stop(SOON).await.expect("stop");
    assert!(!hub.is_live(Channel::MergeQueue));

    let began = Instant::now();
    let lost = turn.idle().await.expect("idle");
    assert_eq!(lost, Roused::Poll, "the loss ends the wait");
    assert!(began.elapsed() < Duration::from_secs(1));

    // Ready for a minute, and nobody could be told.
    w.enqueue(&a).await.expect("enqueue");
    let queue = w.queue_id().await;
    w.t.exec_superuser(&format!(
        "UPDATE forge.merge_queues SET next_run_at = now() - interval '60 seconds' \
          WHERE id = {queue}"
    ))
    .await;
    // The queue is due now: the wait is the short "due" one, not 30 s.
    let began = Instant::now();
    let roused = turn.idle().await.expect("idle");
    assert_ne!(roused, Roused::SafetyNet);
    assert!(began.elapsed() < QueueTurn::FALLBACK_POLL + Duration::from_secs(2));
    let built = turn.tick().await.expect("tick").expect("the enqueue");
    assert_eq!(built.outcome, "built");

    // Nothing due by itself for a minute: the short poll again and again.
    while turn.tick().await.expect("tick").is_some() {}
    let began = Instant::now();
    let roused = turn.idle().await.expect("idle");
    let waited = began.elapsed();
    assert_eq!(roused, Roused::Poll);
    assert!(
        waited >= Duration::from_millis(1500)
            && waited < QueueTurn::FALLBACK_POLL + Duration::from_secs(2),
        "the short poll: {waited:?}"
    );
    let missed = missed_wakes(&logs);
    assert!(missed.is_empty(), "no listener, no missed wake: {missed:?}");
}

/// The auto-merge worker without a hub polls as before plan 18.
#[tokio::test]
async fn without_a_listener_auto_merge_polls_by_itself() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.risk(&a, CheckConclusion::Success, "No risk found").await;
    let mut watch = w.auto().watch(Duration::from_secs(30));
    assert!(!watch.waits_on_wake());
    let began = Instant::now();
    let wake = watch.wait().await;
    let waited = began.elapsed();
    assert_eq!(wake, AutoMergeWake::Poll);
    assert!(
        waited >= Duration::from_secs(4)
            && waited < AutoMergeWatch::FALLBACK_POLL + Duration::from_secs(3),
        "the short poll, not the 30 s sweep: {waited:?}"
    );
    watch.act(wake).await.expect("tick");
    assert!(w.live_entry(&a).await.is_some(), "decided by the poll");
}

/// Review of plan 18 S3 (1): a queue one turn released due now is
/// announced, so another turn in its whole wait (it saw the queue not
/// due, or leased) claims it without waiting out the period — the
/// releasing turn may go on to another queue.
#[tokio::test]
async fn a_queue_released_due_now_wakes_a_turn_in_its_whole_wait() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let queue = w.queue_id().await;
    w.t.exec_superuser(&format!(
        "UPDATE forge.merge_queues SET next_run_at = now() + interval '10 minutes' \
          WHERE id = {queue}"
    ))
    .await;
    let (hub, db) = hub_on(&w).await;
    let q = queue_on(&w, &db, "w", nets());
    let mut first = q.turn(0);
    let mut other = q.turn(1);
    settled(&mut first).await;
    settled(&mut other).await;
    let roused = roused_by!(other, {
        // Due without a word (as if its lease ended), and the first turn
        // takes it: the batch is built and the queue released due now.
        w.t.exec_superuser(&format!(
            "UPDATE forge.merge_queues SET next_run_at = now() WHERE id = {queue}"
        ))
        .await;
        let built = first.tick().await.expect("tick").expect("due");
        assert_eq!(built.outcome, "built");
    });
    assert_eq!(roused, Roused::Wake, "the release announced the queue");
    hub.stop(SOON).await.expect("stop");
}

/// Review of plan 18 S3 (6): the train's pipeline-create job dies by its
/// lease (the worker went away on its last attempt): no pipeline will
/// come, and the reap tells the queue at once — as `fail` does.
#[tokio::test]
async fn a_train_create_job_that_dies_by_its_lease_wakes_the_queue() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let queue = w.queue_id().await;
    let (hub, db) = hub_on(&w).await;
    let mut turn = queue_on(&w, &db, "w", nets()).turn(0);
    settled(&mut turn).await;
    assert!(waits_for_its_pipeline(&w, queue).await);
    w.t.exec_superuser(
        "UPDATE forge.jobs SET state = 'running', worker_id = 'w-gone', \
           attempts = max_attempts, lease_until = now() - interval '1 second' \
         WHERE kind = 'ci.pipeline_create' AND payload ->> 'source' = 'merge_queue'",
    )
    .await;
    let roused = roused_by!(turn, {
        let reaped = forge_db::jobs::reap(&w.t.db).await.expect("reap");
        assert!(reaped.iter().any(|j| j.dead), "{reaped:?}");
    });
    assert_eq!(roused, Roused::Wake, "the reap announced the queue");
    hub.stop(SOON).await.expect("stop");
}

/// Review of plan 18 S3 (9): a sender's stamp during another turn's long
/// step does not make the queue "ready" from the stamp — it was leased
/// until the release, so a claim after a whole safety wait reports no
/// missed wake for it.
#[tokio::test]
async fn a_queue_stamped_during_a_long_step_is_ready_from_its_release() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.enqueue(&a).await.expect("enqueue");
    let queue = w.queue_id().await;
    let lease = Duration::from_secs(300);
    let claimed = qdb::claim(&w.t.db, "w-long", lease)
        .await
        .expect("claim")
        .expect("due");
    // Stamped due by a sender; the step has run for ten seconds since.
    w.t.exec_superuser(&format!(
        "UPDATE forge.merge_queues SET next_run_at = now() - interval '10 seconds' \
          WHERE id = {queue}"
    ))
    .await;
    let _released = qdb::release(&w.t.db, &claimed, Duration::ZERO)
        .await
        .expect("release");
    let again = qdb::claim(&w.t.db, "w-next", lease)
        .await
        .expect("claim")
        .expect("due again");
    assert!(
        again.ready_for < Duration::from_secs(2),
        "ready since the release, not the stamp: {:?}",
        again.ready_for
    );
}

/// Review of plan 18 S3 (4): before a sweep succeeded the watch has no
/// cursor; told of events, it reads none of the history (the sweep, due
/// again soon, covers it) and starts after it.
#[tokio::test]
async fn auto_merge_reads_no_event_history_before_its_first_sweep() {
    let mut w = world().await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.risk(&a, CheckConclusion::Success, "No risk found").await;
    sequencer::run_once(&w.t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("sequence");
    let mut watch = w.auto().watch(Duration::from_secs(30));
    let report = watch.act(AutoMergeWake::Events).await.expect("events");
    assert_eq!(report.looked, 0, "nothing read from cursor 0: {report:?}");
    assert!(w.live_entry(&a).await.is_none());
    // The cursor is the high-water mark now: nothing newer, nothing read.
    let report = watch.act(AutoMergeWake::Events).await.expect("events");
    assert_eq!(report.looked, 0, "{report:?}");
}
