#![allow(clippy::expect_used, clippy::panic, clippy::print_stdout)] // a bench may fail loudly
//! Plan 18 S0: what one claim costs while about 1,000 jobs are queued —
//! the statement `JobRunner` runs for every job (the priority tiers, the
//! ordering keys of the jobs running now, `SKIP LOCKED`), against a
//! migrated test database (ignored: run on demand, alone):
//!
//! ```text
//! cargo nextest run -p forge-db --run-ignored only -E 'test(claim_bench_1000_queued_rows)' --no-capture
//! ```
//!
//! The queue holds what a burst of 250 merge requests leaves behind: per
//! merge request a version refresh, a mergeability check, a conflict scan
//! and a merge-request pipeline. Nothing is asserted about the time.

use std::time::{Duration, Instant};

use forge_core::RequestId;
use serde_json::json;

use super::tests::kind;
use super::*;
use crate::test_support::*;

const QUEUED: i64 = 1_000;
const CLAIMS: usize = 100;
/// The ordering keys a worker with eight busy slots passes over.
const BUSY: i64 = 8;

fn job_of(k: &str, payload: serde_json::Value) -> NewJob {
    NewJob {
        kind: kind(k),
        payload,
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(Duration::from_secs(30)).expect("timeout"),
        dedup_key: None,
        origin_request_id: RequestId::parse("req-claim-bench").expect("id"),
        origin_event_seq: None,
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The `p`-th percentile (nearest rank) of sorted `values`.
fn pct(values: &[Duration], p: usize) -> Duration {
    let rank = (values.len() * p).div_ceil(100).max(1);
    values[rank - 1]
}

#[tokio::test]
#[ignore = "bench: run on demand (module docs)"]
async fn claim_bench_1000_queued_rows() {
    let t = FreshDb::migrated().await;
    let kinds = [
        "merge_request.refresh",
        "merge_request.mergeability",
        "merge_request.conflict_scan",
        "ci.pipeline_create",
    ];
    for mr in 1..=QUEUED / 4 {
        for k in kinds {
            let payload = if k == "ci.pipeline_create" {
                json!({ "project_id": 1, "head_sha": format!("{mr:040x}"), "source": "merge_request" })
            } else {
                json!({ "merge_request_id": mr, "seq": 1 })
            };
            enqueue(&t.db, &job_of(k, payload)).await.expect("enqueue");
        }
    }
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM forge.jobs WHERE state = 'queued'")
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count");
    assert_eq!(queued, QUEUED);

    let w = WorkerId::generate();
    let kinds: Vec<JobKind> = kinds.iter().map(|k| kind(k)).collect();
    // The merge requests whose jobs "run now" (never claimed below).
    let busy: Vec<String> = (1..=BUSY)
        .map(|mr| format!("merge_request.mergeability:mr:{mr}"))
        .collect();
    // One warm-up claim (the pool's connection, the statement's plan).
    claim_excluding(&t.db, &w, &kinds, &busy)
        .await
        .expect("claim")
        .expect("a ready job");
    let mut took = Vec::with_capacity(CLAIMS);
    for _ in 0..CLAIMS {
        let started = Instant::now();
        let job = claim_excluding(&t.db, &w, &kinds, &busy)
            .await
            .expect("claim");
        took.push(started.elapsed());
        assert!(job.is_some(), "a ready job");
    }
    took.sort_unstable();
    let total: Duration = took.iter().sum();
    println!(
        "BENCH jobs.claim queued≈{QUEUED} claims={CLAIMS} busy_keys={BUSY}  \
         p50 {:.2} ms  p95 {:.2} ms  max {:.2} ms  mean {:.2} ms",
        ms(pct(&took, 50)),
        ms(pct(&took, 95)),
        ms(took[took.len() - 1]),
        ms(total) / CLAIMS as f64,
    );
    t.drop_now().await;
}
