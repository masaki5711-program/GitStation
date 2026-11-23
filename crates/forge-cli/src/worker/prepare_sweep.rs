//! The `prepare_sweep` task of `forge worker` (plan 18 S6 review, HIGH):
//! every 60 s it answers `error` for the checks a dead
//! `merge_request.prepare` job owed — `forge/risk` and the spec checks
//! (`forge/spec-lint`, `forge/spec-trace`, `forge/doc-lint`,
//! `forge/ci-lint`) of its merge request's current head, whichever the
//! head still lacks (`forge_db::review::unanswered_dead_prepares`). A
//! version the prepare job recorded says `prepared`, so the `risk` and
//! `spec-index` subscribers asked for no job of their own; without this
//! sweep a prepare job that died after the version would leave those
//! checks unanswered for good.
//!
//! As `risk_sweep`: the dead jobs are walked by id with a cursor, at most
//! [`BATCH`] per run, wrapping after the last; a full batch runs again at
//! once. Each answer carries the dead job's code and its request id
//! (`record_error` / `record_errors`, which log a failure to record); an
//! answered head is not listed again. A failed run (the database) is
//! returned and logged once per failure streak by the supervisor.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use forge_checks::risk::record_error;
use forge_checks::spec_checks::record_errors;
use forge_core::RequestId;
use forge_db::checks::risk::RiskRequest;
use forge_db::checks::spec_index::SpecIndexRequest;
use forge_db::review::unanswered_dead_prepares;
use forge_db::Db;
use forge_errors::ForgeError;

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "prepare_sweep";
/// The task's period.
pub(crate) const PERIOD: Duration = Duration::from_secs(60);
/// Dead jobs looked at per run at most.
const BATCH: i64 = 50;

/// Where the next run starts (the last job id seen; `0`: the start).
static CURSOR: AtomicI64 = AtomicI64::new(0);

/// One run (module docs).
pub(crate) async fn run_prepare_sweep(db: &Db) -> Result<Next, ForgeError> {
    sweep(db, &CURSOR, BATCH).await
}

/// One run with its own cursor and batch size (tests).
pub(crate) async fn sweep(db: &Db, cursor: &AtomicI64, batch: i64) -> Result<Next, ForgeError> {
    let after = cursor.load(Ordering::Relaxed);
    let jobs = unanswered_dead_prepares(db, after, batch).await?;
    let mut answered = 0usize;
    for job in &jobs {
        let request_id = match RequestId::parse(&job.origin_request_id) {
            Some(id) => id,
            None => {
                let fresh = RequestId::generate();
                tracing::warn!(
                    target: "forge_cli",
                    task = NAME,
                    event = "forge.worker.prepare_sweep_request_id_replaced",
                    job_id = job.job_id,
                    request_id = %fresh,
                    "a dead prepare job's request id is unreadable; its answers carry a new one"
                );
                fresh
            }
        };
        if job.risk_missing {
            let request = RiskRequest {
                project: job.project,
                head: job.head.clone(), // clone: the request owns it
            };
            if record_error(db, &request, &request_id, &job.last_error_code).await {
                answered += 1;
            }
        }
        if job.spec_missing {
            let request = SpecIndexRequest {
                project: job.project,
                iid: job.iid,
                head: job.head.clone(), // clone: the request owns it
            };
            if record_errors(db, &request, &request_id, &job.last_error_code).await {
                answered += 1;
            }
        }
    }
    if !jobs.is_empty() {
        tracing::info!(
            target: "forge_cli",
            task = NAME,
            event = "forge.worker.prepare_swept",
            found = jobs.len(),
            answered,
            "the checks of dead prepare jobs answered with error"
        );
    }
    let full = i64::try_from(jobs.len()).unwrap_or(i64::MAX) >= batch;
    let next_cursor = match (full, jobs.last()) {
        (true, Some(last)) => last.job_id,
        _ => 0,
    };
    cursor.store(next_cursor, Ordering::Relaxed);
    Ok(if full {
        Next::Immediately
    } else {
        Next::AfterPeriod
    })
}
