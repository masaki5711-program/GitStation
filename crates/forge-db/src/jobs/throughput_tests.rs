//! Merge load test 2026-11-17 (F2): the claim's priority tiers, the
//! superseding enqueue of per-request jobs, and a runner shared by several
//! runs at once (per-kind caps, one job per ordering key, never a job run
//! twice).

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use forge_core::RequestId;
use serde_json::json;
use tokio::sync::watch;

use super::runner_tests::{runner, Script};
use super::tests::{kind, queued};
use super::*;
use crate::test_support::*;

const T: Duration = Duration::from_secs(30);

fn job_of(k: &str, payload: serde_json::Value, dedup: Option<&str>) -> NewJob {
    NewJob {
        kind: kind(k),
        payload,
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(T).expect("timeout"),
        dedup_key: dedup.map(|d| DedupKey::parse(d).expect("dedup")),
        origin_request_id: RequestId::parse("req-throughput").expect("id"),
        origin_event_seq: None,
    }
}

/// Every job of `kinds` in claim order (each claimed, never settled).
async fn claim_order(t: &FreshDb, kinds: &[&str]) -> Vec<JobId> {
    let w = WorkerId::generate();
    let kinds: Vec<JobKind> = kinds.iter().map(|k| kind(k)).collect();
    let mut order = Vec::new();
    while let Some(job) = claim(&t.db, &w, &kinds).await.expect("claim") {
        order.push(job.lease.job_id);
    }
    order
}

/// What the merge queue waits on comes first: the train pipeline, then the
/// versions and the checks of queued merge requests, then the rest (and
/// what waited 5 minutes), then a target push's fan-out of checks, then
/// the conflict scans.
#[tokio::test]
async fn the_claim_order_puts_what_the_merge_queue_waits_on_first() {
    let t = FreshDb::migrated().await;
    let scan = queued(
        &t,
        &job_of(
            "merge_request.conflict_scan",
            json!({ "merge_request_id": 1, "scan_seq": 1 }),
            None,
        ),
    )
    .await;
    let fan_out = queued(
        &t,
        &job_of(
            "merge_request.mergeability",
            json!({ "merge_request_id": 2, "check_seq": 1 }),
            None,
        ),
    )
    .await;
    let other = queued(&t, &job_of("test.other", json!({}), None)).await;
    let in_queue = queued(
        &t,
        &job_of(
            "merge_request.mergeability",
            json!({ "merge_request_id": 77, "check_seq": 1 }),
            None,
        ),
    )
    .await;
    let version = queued(
        &t,
        &job_of(
            "merge_request.refresh",
            json!({ "merge_request_id": 3 }),
            None,
        ),
    )
    .await;
    let mr_pipeline = queued(
        &t,
        &job_of(
            "ci.pipeline_create",
            json!({ "project_id": 1, "source": "merge_request", "merge_request_id": 4 }),
            None,
        ),
    )
    .await;
    let train = queued(
        &t,
        &job_of(
            "ci.pipeline_create",
            json!({ "project_id": 1, "source": "merge_queue", "merge_request_id": null }),
            None,
        ),
    )
    .await;
    let old_scan = queued(
        &t,
        &job_of(
            "merge_request.conflict_scan",
            json!({ "merge_request_id": 5, "scan_seq": 1 }),
            None,
        ),
    )
    .await;
    // A scan ready for ten minutes rises to the default tier.
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET run_after = now() - interval '10 minutes' WHERE id = {}",
        old_scan.get()
    ))
    .await;
    // Merge request 77 has a live queue entry (its references are not the
    // subject here: only the entry's state and merge request are read).
    t.exec_superuser(
        "SET session_replication_role = replica; \
         INSERT INTO forge.merge_queue_entries (queue_id, project_id, merge_request_id, state, \
           head_sha, method, message, enqueued_by, credential_kind, credential_id, request_id) \
         VALUES (1, 1, 77, 'queued', repeat('a', 40), 'merge_commit', 'm', 1, 'session', 1, \
           'req-entry'); \
         SET session_replication_role = origin;",
    )
    .await;
    let order = claim_order(
        &t,
        &[
            "merge_request.conflict_scan",
            "merge_request.mergeability",
            "merge_request.refresh",
            "ci.pipeline_create",
            "test.other",
        ],
    )
    .await;
    assert_eq!(
        order,
        [
            train,
            in_queue,
            version,
            old_scan,
            other,
            mr_pipeline,
            fan_out,
            scan
        ]
    );
    t.drop_now().await;
}

/// A malformed merge request id in a payload never fails the claim of
/// every job (the priority casts only digits).
#[tokio::test]
async fn a_malformed_payload_does_not_fail_the_claim() {
    let t = FreshDb::migrated().await;
    let bad = queued(
        &t,
        &job_of(
            "merge_request.mergeability",
            json!({ "merge_request_id": "x1", "check_seq": 1 }),
            None,
        ),
    )
    .await;
    assert_eq!(
        claim_order(&t, &["merge_request.mergeability"]).await,
        [bad]
    );
    t.drop_now().await;
}

async fn superseding(t: &FreshDb, job: &NewJob, mr: i64) -> queue::Superseding {
    let mut conn = t.db.write_pool().acquire().await.expect("conn");
    enqueue_superseding_on(&mut conn, job, mr)
        .await
        .expect("enqueue")
}

/// (id, state) of the jobs of `kind` for merge request `mr`, by id.
async fn jobs_of(t: &FreshDb, kind: &str, mr: i64) -> Vec<(i64, String)> {
    sqlx::query_as(
        "SELECT id, state FROM forge.jobs WHERE kind = $1 \
         AND payload ->> 'merge_request_id' = $2 ORDER BY id",
    )
    .bind(kind)
    .bind(mr.to_string())
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("jobs")
}

/// A newer request replaces the queued job of an older one of the same
/// merge request (keeping its place in line); a running job and other
/// merge requests are left alone.
#[tokio::test]
async fn a_newer_request_replaces_the_queued_job_of_an_older_one() {
    let t = FreshDb::migrated().await;
    const K: &str = "merge_request.mergeability";
    let one = |mr: i64, seq: i64| {
        job_of(
            K,
            json!({ "merge_request_id": mr, "check_seq": seq }),
            Some(&format!("mr_mergeability:{mr}:{seq}")),
        )
    };
    let first = superseding(&t, &one(5, 1), 5).await;
    assert_eq!(first.superseded, 0);
    let Enqueued::Queued(first_id) = first.enqueued else {
        panic!("queued");
    };
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET run_after = now() - interval '1 minute' WHERE id = {}",
        first_id.get()
    ))
    .await;
    let other = superseding(&t, &one(6, 1), 6).await;
    assert!(matches!(other.enqueued, Enqueued::Queued(_)));
    let second = superseding(&t, &one(5, 2), 5).await;
    assert_eq!(second.superseded, 1);
    let Enqueued::Queued(second_id) = second.enqueued else {
        panic!("queued");
    };
    assert_eq!(
        jobs_of(&t, K, 5).await,
        [(second_id.get(), "queued".to_owned())]
    );
    // Its place in line: the replaced job's run_after (earlier than now).
    let waited: bool = sqlx::query_scalar(
        "SELECT run_after < now() - interval '30 seconds' FROM forge.jobs WHERE id = $1",
    )
    .bind(second_id.get())
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("run_after");
    assert!(waited);
    // The same request again: deduplicated, nothing deleted.
    let again = superseding(&t, &one(5, 2), 5).await;
    assert_eq!((again.enqueued, again.superseded), (Enqueued::Deduped, 0));
    // Claimed (running): a newer request queues beside it.
    let running = claim(&t.db, &WorkerId::generate(), &[kind(K)])
        .await
        .expect("claim")
        .expect("ready");
    assert_eq!(running.lease.job_id, second_id);
    let third = superseding(&t, &one(5, 3), 5).await;
    assert_eq!(third.superseded, 0);
    let states: Vec<String> = jobs_of(&t, K, 5).await.into_iter().map(|j| j.1).collect();
    assert_eq!(states, ["running", "queued"]);
    assert_eq!(
        jobs_of(&t, K, 6).await.len(),
        1,
        "another merge request's job stays"
    );
    t.drop_now().await;
}

/// A handler that counts the runs at once, records each job and, for the
/// merge requests in `hold`, waits until `gate` opens.
struct Probe {
    now: AtomicUsize,
    max: AtomicUsize,
    ran: Mutex<Vec<JobId>>,
    gate: watch::Sender<bool>,
}

impl Probe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            now: AtomicUsize::new(0),
            max: AtomicUsize::new(0),
            ran: Mutex::new(Vec::new()),
            gate: watch::channel(false).0,
        })
    }

    fn script(self: &Arc<Self>, hold: &'static [i64]) -> Script {
        let probe = Arc::clone(self);
        Arc::new(move |job: ClaimedJob| {
            let probe = Arc::clone(&probe);
            let run: JobFuture = Box::pin(async move {
                let n = probe.now.fetch_add(1, Ordering::SeqCst) + 1;
                probe.max.fetch_max(n, Ordering::SeqCst);
                probe.ran.lock().expect("ran").push(job.lease.job_id);
                let mr = job.payload["merge_request_id"].as_i64().unwrap_or(0);
                if hold.contains(&mr) {
                    let mut gate = probe.gate.subscribe();
                    let _open = gate.wait_for(|open| *open).await;
                } else {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                probe.now.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            });
            run
        })
    }

    async fn until_running(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.now.load(Ordering::SeqCst) < n {
            assert!(Instant::now() < deadline, "never {n} running");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn of_mr(k: &str, mr: i64) -> NewJob {
    job_of(k, json!({ "merge_request_id": mr }), None)
}

/// A kind's cap holds across the runs sharing a runner: with cap 2 a third
/// run claims nothing of that kind while two run.
#[tokio::test]
async fn a_kind_never_runs_more_jobs_at_once_than_its_cap() {
    let t = FreshDb::migrated().await;
    let probe = Probe::new();
    let mut r = runner(&t, "test.capped", probe.script(&[1, 2, 3]));
    r.limit_kind(&kind("test.capped"), NonZeroUsize::new(2).expect("2"));
    let r = Arc::new(r);
    for mr in 1..=3 {
        queued(&t, &of_mr("test.capped", mr)).await;
    }
    let runs: Vec<_> = (0..2)
        .map(|_| {
            let r = Arc::clone(&r);
            tokio::spawn(async move { r.run_once().await })
        })
        .collect();
    probe.until_running(2).await;
    assert_eq!(r.running(), 2);
    assert!(r.run_once().await.expect("run").is_none(), "capped");
    probe.gate.send_replace(true);
    for run in runs {
        let ran = run.await.expect("join").expect("run");
        assert!(matches!(ran, Some(Ran::Succeeded(_))), "{ran:?}");
    }
    assert!(matches!(
        r.run_once().await.expect("run"),
        Some(Ran::Succeeded(_))
    ));
    assert_eq!(probe.max.load(Ordering::SeqCst), 2);
    assert_eq!(r.running(), 0);
    t.drop_now().await;
}

/// Two jobs of one merge request never run at once (a version job must not
/// record an older head after a newer one): the second waits while another
/// merge request's job goes ahead.
#[tokio::test]
async fn two_jobs_of_one_merge_request_never_run_at_once() {
    let t = FreshDb::migrated().await;
    let probe = Probe::new();
    let r = Arc::new(runner(&t, "test.keyed", probe.script(&[9])));
    let first = queued(&t, &of_mr("test.keyed", 9)).await;
    let second = queued(&t, &of_mr("test.keyed", 9)).await;
    let other = queued(&t, &of_mr("test.keyed", 10)).await;
    let held = {
        let r = Arc::clone(&r);
        tokio::spawn(async move { r.run_once().await })
    };
    probe.until_running(1).await;
    assert!(
        matches!(r.run_once().await.expect("run"), Some(Ran::Succeeded(id)) if id == other),
        "another merge request goes ahead"
    );
    assert!(
        r.run_once().await.expect("run").is_none(),
        "merge request 9 is busy"
    );
    probe.gate.send_replace(true);
    let ran = held.await.expect("join").expect("run");
    assert!(
        matches!(ran, Some(Ran::Succeeded(id)) if id == first),
        "{ran:?}"
    );
    assert!(matches!(r.run_once().await.expect("run"), Some(Ran::Succeeded(id)) if id == second));
    t.drop_now().await;
}

/// Several runs of one runner and a second worker take 30 jobs: each runs
/// exactly once.
#[tokio::test]
async fn parallel_runs_never_run_a_job_twice() {
    let t = FreshDb::migrated().await;
    let probe = Probe::new();
    let a = Arc::new(runner(&t, "test.many", probe.script(&[])));
    let b = Arc::new(runner(&t, "test.many", probe.script(&[])));
    let mut all = BTreeSet::new();
    for mr in 0..30 {
        all.insert(queued(&t, &of_mr("test.many", mr)).await);
    }
    let loops: Vec<_> = [&a, &a, &a, &a, &b, &b]
        .into_iter()
        .map(|r| {
            let r = Arc::clone(r);
            tokio::spawn(async move {
                while let Some(ran) = r.run_once().await.expect("run") {
                    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
                }
            })
        })
        .collect();
    for l in loops {
        l.await.expect("join");
    }
    let ran = probe.ran.lock().expect("ran").clone();
    let unique: BTreeSet<JobId> = ran.iter().copied().collect();
    assert_eq!(ran.len(), all.len(), "no job ran twice");
    assert_eq!(unique, all);
    assert!(probe.max.load(Ordering::SeqCst) > 1, "they did run at once");
    t.drop_now().await;
}

/// The Rust ordering key is the SQL one (`ORDERING_KEY_SQL`).
#[tokio::test]
async fn the_ordering_key_matches_its_sql() {
    let t = FreshDb::migrated().await;
    let cases = [
        json!({ "merge_request_id": 12, "check_seq": 3 }),
        json!({ "merge_request_id": null, "project_id": 4, "iid": 7, "head_sha": "ab" }),
        json!({ "project_id": 4, "head_sha": "ab" }),
        json!({ "project_id": 4 }),
        json!({}),
    ];
    for payload in cases {
        let sql: Option<String> = sqlx::query_scalar(&format!(
            "SELECT 'k.x:' || {} FROM (SELECT $1::jsonb AS payload) AS j",
            queue::ORDERING_KEY_SQL
        ))
        .bind(payload.to_string())
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("key");
        assert_eq!(sql, ordering_key(&kind("k.x"), &payload), "{payload}");
    }
    t.drop_now().await;
}
