//! The `check.risk` job and the `risk` subscriber (M3 X7, ADR-0023
//! Amendment X7): every merge request version (`merge_request.pushed`,
//! which also announces the first version of an open) asks for one
//! evaluation of `forge/risk` on its head commit. The job is run by
//! `forge worker` through forge-checks, which writes the run with
//! [`super::record_forge_check_run`].
//!
//! One job per (project, head, trigger): the dedup key is
//! `risk:<project>:<head>:mr<merge request>` for a version (`merge_request.pushed`),
//! and `…:mr<id>:e<event seq>` for a retarget (`merge_request.updated` with a
//! new target) or a reopen (review X7: the same head in another merge request,
//! or into another target, is assessed again — a succeeded job of one
//! merge request never stands for another's). A queued, running or
//! succeeded job holds its key, so a version that comes back is not
//! evaluated twice; a dead job does not hold it (ADR-0016 §7), and its head's
//! run says `error` (the worker's `risk_sweep`, [`unanswered_dead_jobs`]).
//!
//! What the head is compared with is the target branch of each open merge
//! request that has it as a version ([`risk_targets`]), read when the job
//! runs, not stored in the job: a retarget before the job runs is seen.

use forge_core::{EventType, ObjectId, ProjectId, RefName, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use crate::jobs::{self, enqueue_on, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};
use crate::{read_query_error, Db};

/// The kind of the job.
pub const RISK_JOB: &str = "check.risk";
/// The subscriber's name (`forge.event_subscribers`).
pub const RISK_SUBSCRIBER: &str = "risk";
/// One evaluation: the evaluation's own budget (45 s), one write (10 s,
/// and 10 s again for the `error` of the last attempt) and the lookups.
pub const RISK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
/// The most target branches one head is assessed against.
pub const MAX_TARGETS: usize = 4;

/// The job's payload: the commit to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskRequest {
    pub project: ProjectId,
    pub head: ObjectId,
}

fn invalid_job(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", RISK_JOB)
}

/// The job evaluating `request` (`trigger`: what asked, part of the dedup key).
pub fn risk_job(
    request: &RiskRequest,
    trigger: &str,
    origin_request_id: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let key = format!(
        "risk:{}:{}:{trigger}",
        request.project.get(),
        request.head.as_str()
    );
    Ok(NewJob {
        kind: JobKind::parse(RISK_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({
            "project_id": request.project.get(),
            "head_sha": request.head.as_str(),
        }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(RISK_TIMEOUT).ok_or_else(|| invalid_job("job_timeout"))?,
        dedup_key: Some(jobs::DedupKey::parse(&key).ok_or_else(|| invalid_job("dedup_key"))?),
        origin_request_id,
        origin_event_seq,
    })
}

/// The payload of a claimed job (`FG-SYS-0001 reason=invalid_job_payload`
/// for anything else: forge wrote it).
pub fn risk_payload(payload: &serde_json::Value) -> Result<RiskRequest, ForgeError> {
    let project = payload
        .get("project_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(|| invalid_job("invalid_job_payload"))?;
    let head = payload
        .get("head_sha")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| ObjectId::parse(raw).ok())
        .ok_or_else(|| invalid_job("invalid_job_payload"))?;
    Ok(RiskRequest {
        project: ProjectId::new(project),
        head,
    })
}

/// The target branches a head is assessed against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskTargets {
    /// At most [`MAX_TARGETS`], by name.
    pub targets: Vec<RefName>,
    /// More targets exist than are listed.
    pub more: bool,
}

/// The distinct target branches of the **open** merge requests of
/// `project` that have `head` as one of their versions.
pub async fn risk_targets(
    db: &Db,
    project: ProjectId,
    head: &ObjectId,
) -> Result<RiskTargets, ForgeError> {
    const QUERY: &str = "checks.risk_targets";
    let limit = i64::try_from(MAX_TARGETS)
        .unwrap_or(i64::MAX)
        .saturating_add(1);
    let rows: Vec<String> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT DISTINCT m.target_branch FROM forge.merge_requests m \
                 JOIN forge.merge_request_versions v ON v.merge_request_id = m.id \
                 WHERE m.project_id = $1 AND v.head_sha = $2 AND m.state = 'open' \
                 ORDER BY m.target_branch LIMIT $3",
            )
            .bind(project.get())
            .bind(head.as_str())
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let more = rows.len() > MAX_TARGETS;
    let mut targets = Vec::with_capacity(rows.len().min(MAX_TARGETS));
    for raw in rows.into_iter().take(MAX_TARGETS) {
        // The table's CHECK keeps it a branch ref; a name this build cannot
        // parse is data that should not exist.
        let name = RefName::parse(&raw).map_err(|_| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_target_branch")
                .with_context("project_id", project.get())
        })?;
        targets.push(name);
    }
    Ok(RiskTargets { targets, more })
}

/// A dead `check.risk` job whose head has no `forge/risk` run (the reaper
/// ended it — a crashed worker, the runner's own timeout — so its handler
/// never answered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnansweredJob {
    pub job_id: i64,
    pub request: RiskRequest,
    /// The job's request (the answer and its logs carry it).
    pub origin_request_id: String,
    /// The code the job died with.
    pub last_error_code: String,
}

/// How far back the sweep looks for dead jobs.
pub const UNANSWERED_WINDOW_DAYS: i32 = 7;

/// At most `limit` [`UnansweredJob`]s with an id above `after_job_id`, by id
/// (the sweep walks the ids with a cursor, so a job whose answer keeps
/// failing cannot starve the others): dead within [`UNANSWERED_WINDOW_DAYS`],
/// of a project that exists and is not soft-deleted (its run can never be
/// written), with no `forge/risk` run on their head. A payload this build cannot read is
/// passed over (logged at warn: forge wrote it).
pub async fn unanswered_dead_jobs(
    db: &Db,
    after_job_id: i64,
    limit: i64,
) -> Result<Vec<UnansweredJob>, ForgeError> {
    const QUERY: &str = "checks.risk_unanswered";
    let rows: Vec<(i64, serde_json::Value, String, Option<String>)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT j.id, j.payload, j.origin_request_id, j.last_error_code \
                 FROM forge.jobs j JOIN forge.projects p ON p.id = CASE \
                 WHEN j.payload->>'project_id' ~ '^[0-9]{1,18}$' \
                 THEN (j.payload->>'project_id')::bigint END \
                 WHERE j.kind = $1 AND j.state = 'dead' AND j.id > $5 \
                 AND p.deleted_at IS NULL \
                 AND j.finished_at > now() - make_interval(days => $2) \
                 AND NOT EXISTS (SELECT 1 FROM forge.check_runs r WHERE r.project_id = p.id \
                 AND r.head_sha = j.payload->>'head_sha' AND r.name = $3) \
                 ORDER BY j.id LIMIT $4",
            )
            .bind(RISK_JOB)
            .bind(UNANSWERED_WINDOW_DAYS)
            .bind(forge_core::checks::RISK_CHECK)
            .bind(limit)
            .bind(after_job_id)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (job_id, payload, origin_request_id, code) in rows {
        match risk_payload(&payload) {
            Ok(request) => out.push(UnansweredJob {
                job_id,
                request,
                origin_request_id,
                // `jobs_dead_has_error`: a dead job always has one.
                last_error_code: code.unwrap_or_else(|| ErrorCode::JobDeadLettered.id().to_owned()),
            }),
            Err(e) => tracing::warn!(
                target: "forge_db",
                event = "forge.risk.unanswered_unreadable",
                error_code = e.code().id(),
                job_id,
                "a dead risk job's payload cannot be read; passed over"
            ),
        }
    }
    Ok(out)
}

/// The `risk` subscriber (`independent`: an event it cannot read is
/// quarantined and counted, never retried forever).
#[derive(Debug, Default, Clone, Copy)]
pub struct RiskRequests;

fn bad_payload(event: &SourceEvent, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_event_payload")
        .with_context("event_type", event.event_type.clone()) // clone: the context owns it
        .with_context("field", field)
        .with_context("seq", event.seq)
}

impl RiskRequests {
    async fn on_version(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        let project = event
            .project_id
            .ok_or_else(|| bad_payload(event, "project_id"))?;
        let head = event
            .payload
            .get("head_sha")
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| ObjectId::parse(raw).ok())
            .ok_or_else(|| bad_payload(event, "head_sha"))?;
        let mr = event
            .payload
            .get("merge_request_id")
            .and_then(serde_json::Value::as_i64)
            .filter(|id| *id > 0)
            .ok_or_else(|| bad_payload(event, "merge_request_id"))?;
        let origin = event.request_id().unwrap_or_else(RequestId::generate);
        let job = risk_job(
            &RiskRequest { project, head },
            &format!("mr{mr}"),
            origin,
            Some(event.seq),
        )?;
        // Deduped: a retried step, or a version of this merge request
        // evaluated before.
        let _enqueued = enqueue_on(tx.conn(), &job).await?;
        Ok(())
    }

    /// A retarget or a reopen: the merge request's current head is
    /// assessed against its (new) target. Nothing to ask while it has no
    /// head yet (its first version asks).
    async fn on_target_change(
        event: &SourceEvent,
        tx: &mut DerivedTx<'_>,
    ) -> Result<(), ForgeError> {
        let project = event
            .project_id
            .ok_or_else(|| bad_payload(event, "project_id"))?;
        let mr = event
            .payload
            .get("merge_request_id")
            .and_then(serde_json::Value::as_i64)
            .filter(|id| *id > 0)
            .ok_or_else(|| bad_payload(event, "merge_request_id"))?;
        let head: Option<Option<String>> = sqlx::query_scalar(
            "SELECT head_sha FROM forge.merge_requests WHERE id = $1 AND project_id = $2",
        )
        .bind(mr)
        .bind(project.get())
        .fetch_optional(tx.conn())
        .await
        .map_err(|e| crate::query_error("checks.risk_head", e))?;
        let Some(head) = head.flatten().and_then(|raw| ObjectId::parse(&raw).ok()) else {
            return Ok(());
        };
        let origin = event.request_id().unwrap_or_else(RequestId::generate);
        let job = risk_job(
            &RiskRequest { project, head },
            &format!("mr{mr}:e{}", event.seq),
            origin,
            Some(event.seq),
        )?;
        let _enqueued = enqueue_on(tx.conn(), &job).await?;
        Ok(())
    }
}

impl Subscriber for RiskRequests {
    fn name(&self) -> &'static str {
        RISK_SUBSCRIBER
    }

    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            let Some(kind) = EventType::parse(&event.event_type) else {
                // A type this build does not know (a newer writer): it is
                // no merge request version this build could read.
                tracing::warn!(
                    target: "forge_db",
                    event = "forge.subscribers.unknown_event_type",
                    subscriber = RISK_SUBSCRIBER,
                    request_id = %event.request_id,
                    source_seq = event.seq,
                    event_type = %event.event_type,
                    "an event type this build does not know was passed over"
                );
                return Ok(());
            };
            match kind {
                EventType::MergeRequestPushed => Self::on_version(event, tx).await,
                EventType::MergeRequestReopened => Self::on_target_change(event, tx).await,
                // Only a retarget (the payload names the new target).
                EventType::MergeRequestUpdated
                    if event
                        .payload
                        .get("target_branch")
                        .is_some_and(|t| !t.is_null()) =>
                {
                    Self::on_target_change(event, tx).await
                }
                EventType::MergeRequestUpdated => Ok(()),
                EventType::ProjectCreated
                | EventType::RepositoryCreated
                | EventType::RepositoryPushed
                | EventType::TokenCreated
                | EventType::TokenRevoked
                | EventType::MemberAdded
                | EventType::MemberRoleChanged
                | EventType::MemberRemoved
                | EventType::ProtectedRefCreated
                | EventType::ProtectedRefUpdated
                | EventType::ProtectedRefDeleted
                | EventType::ProjectVisibilityChanged
                | EventType::ProjectDeleted
                | EventType::ProjectRestored
                | EventType::ProjectPurged
                | EventType::ProjectMoved
                | EventType::GroupMoved
                | EventType::UserRenamed
                | EventType::MergeRequestOpened
                | EventType::MergeRequestDraftChanged
                | EventType::MergeRequestClosed
                | EventType::MergeRequestConflictDetected
                | EventType::MergeRequestConflictResolved
                | EventType::MergeRequestConflictPredicted
                | EventType::MergeRequestCommented
                | EventType::MergeRequestMerged
                | EventType::MergeRequestSourceBranchDeleted
                | EventType::MergeRequestApproved
                | EventType::MergeRequestUnapproved
                | EventType::CodeownersEnforcementChanged
                | EventType::CodeownersEnforcementRestored
                | EventType::CheckCompleted
                | EventType::WorkItemLinked
                | EventType::WorkItemUnlinked
                | EventType::MergeRequestWorkItemsChanged
                | EventType::MergeRequestRelinkRequested
                | EventType::WebhookDisabled => Ok(()),
            }
        })
    }
}

#[cfg(test)]
#[path = "risk_tests.rs"]
mod tests;
