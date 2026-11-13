//! Scheduled pipelines in `forge worker` (M4 C12, plan 14 §5 C12):
//!
//! - the `ci_schedules` task: every [`PERIOD`] it claims the due schedules
//!   (`forge_db::ci_schedules::claim_due`: the next time from the
//!   database's clock and one `ci.schedule_fire` job per due time, in one
//!   transaction). A full batch runs again at once. A failed run is
//!   returned and logged once per failure streak by the supervisor with
//!   `error_code` and `task=ci_schedules`;
//! - the `ci.schedule_fire` job handler: one due time
//!   ([`forge_pipelines::Pipelines::fire_schedule`]). Retried like
//!   `ci.pipeline_create` (git, storage, the database, a repository still
//!   being provisioned); on the last attempt, or a failure that can never
//!   pass, the schedule records `failed` with the code first so its page
//!   says why; a malformed payload is dead at once.

use std::sync::Arc;
use std::time::Duration;

use forge_core::RequestId;
use forge_db::ci_schedules::{claim_due, CLAIM_BATCH};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_pipelines::schedules::{fire_payload, FireResult, SCHEDULE_FIRE_JOB};
use forge_pipelines::Pipelines;

use super::provision::Storage;
use super::supervise::Next;
use crate::ci_marks::ConfidentialMeter;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "ci_schedules";
/// How often it looks for due schedules (a rule fires at most every 5
/// minutes; a run starts within this much of its time).
pub(crate) const PERIOD: Duration = Duration::from_secs(30);

/// One run of the task.
pub(crate) async fn run_ci_schedules(db: &Db) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let claimed = claim_due(db, &request_id).await?;
    for fired in &claimed {
        tracing::info!(
            target: "forge_cli::worker",
            event = "forge.ci.schedule_due",
            request_id = %request_id,
            schedule_id = fired.schedule,
            project_id = fired.project.get(),
            due = fired.due,
            "a pipeline schedule is due; its run is queued"
        );
    }
    if i64::try_from(claimed.len()).unwrap_or(i64::MAX) >= CLAIM_BATCH {
        return Ok(Next::Immediately);
    }
    Ok(Next::AfterPeriod)
}

/// The handler of `ci.schedule_fire`.
pub(crate) struct ScheduleFireHandler {
    kind: JobKind,
    pipelines: Arc<Pipelines>,
}

impl ScheduleFireHandler {
    pub(crate) fn new(db: &Db, storage: &Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(SCHEDULE_FIRE_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", SCHEDULE_FIRE_JOB)
        })?;
        let browse = Browser::new(
            storage.git.clone(),  // clone: the browser owns its handles
            storage.root.clone(), // clone: as git
            BrowseLimits::default(),
        );
        Ok(Self {
            kind,
            pipelines: Arc::new(Pipelines::new(
                db.clone(),     // clone: pool handles
                browse.clone(), // clone: the meter reads with the same browser
                Arc::new(ConfidentialMeter::new(browse)),
            )),
        })
    }
}

/// Whether a failed run may succeed later (as `ci.pipeline_create`).
fn retryable(e: &ForgeError) -> bool {
    super::review::retryable(e)
        || matches!(
            e.code(),
            ErrorCode::RequestTimeout | ErrorCode::PipelineCreateFailed
        )
}

impl JobHandler for ScheduleFireHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let pipelines = Arc::clone(&self.pipelines);
        Box::pin(async move {
            let request = fire_payload(&job.payload).map_err(JobFailure::permanent)?;
            let request_id = job.request_id();
            let e = match pipelines.fire_schedule(&request, &request_id).await {
                Ok(FireResult::Skipped(reason)) => {
                    tracing::info!(
                        target: "forge_cli::worker",
                        event = "forge.ci.schedule_skipped",
                        request_id = %request_id,
                        schedule_id = request.schedule,
                        reason,
                        "the scheduled run had nothing to do"
                    );
                    return Ok(());
                }
                Ok(FireResult::Started(_) | FireResult::Recorded(_)) => return Ok(()),
                Err(e) => e,
            };
            let last = job.lease.attempt >= job.max_attempts;
            if retryable(&e) && !last {
                return Err(JobFailure::retry(e));
            }
            if let Err(write) = pipelines
                .record_schedule_failure(&request, &request_id, &e)
                .await
            {
                return Err(JobFailure::retry(write));
            }
            Err(if retryable(&e) {
                JobFailure::retry(e)
            } else {
                JobFailure::permanent(e)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// With no schedule the task waits for its period.
    #[tokio::test]
    async fn a_schedules_run_with_nothing_due_waits_for_the_period() {
        let t = FreshDb::migrated().await;
        assert_eq!(
            run_ci_schedules(&t.db).await.expect("run"),
            Next::AfterPeriod
        );
        t.drop_now().await;
    }
}
