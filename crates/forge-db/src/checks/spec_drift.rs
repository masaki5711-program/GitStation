//! The `check.spec_drift` job and the `spec-drift` subscriber (M3 X2,
//! ADR-0023 Amendment X2): every merge request version
//! (`merge_request.pushed`, which also announces the first version of an
//! open) asks for one evaluation of `forge/spec-drift` on its head commit.
//! The job is run by `forge worker` through forge-checks, which writes the
//! run with [`super::record_forge_check_run`].
//!
//! One job per (project, head): the dedup key `spec_drift:<project>:<head>`
//! is held by a queued, running or succeeded job, so a head that comes back
//! (a force push to an earlier version, a reopen) is not evaluated twice; a
//! dead job does not hold it (ADR-0016 §7), and its head's run says `error`.

use forge_core::{EventType, ObjectId, ProjectId, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use crate::jobs::{self, enqueue_on, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};

/// The kind of the job.
pub const SPEC_DRIFT_JOB: &str = "check.spec_drift";
/// The subscriber's name (`forge.event_subscribers`).
pub const SPEC_DRIFT_SUBSCRIBER: &str = "spec-drift";
/// One evaluation: a file read, at most 32 references (a lookup and a ref
/// list each, every git call bounded by the browse limits) and one write.
pub const SPEC_DRIFT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The job's payload: the commit to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecDriftRequest {
    pub project: ProjectId,
    pub head: ObjectId,
}

fn invalid_job(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", SPEC_DRIFT_JOB)
}

/// The job evaluating `request`.
pub fn spec_drift_job(
    request: &SpecDriftRequest,
    origin_request_id: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let key = format!(
        "spec_drift:{}:{}",
        request.project.get(),
        request.head.as_str()
    );
    Ok(NewJob {
        kind: JobKind::parse(SPEC_DRIFT_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({
            "project_id": request.project.get(),
            "head_sha": request.head.as_str(),
        }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(SPEC_DRIFT_TIMEOUT).ok_or_else(|| invalid_job("job_timeout"))?,
        dedup_key: Some(jobs::DedupKey::parse(&key).ok_or_else(|| invalid_job("dedup_key"))?),
        origin_request_id,
        origin_event_seq,
    })
}

/// The payload of a claimed job (`FG-SYS-0001 reason=invalid_job_payload`
/// for anything else: forge wrote it).
pub fn spec_drift_payload(payload: &serde_json::Value) -> Result<SpecDriftRequest, ForgeError> {
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
    Ok(SpecDriftRequest {
        project: ProjectId::new(project),
        head,
    })
}

/// The `spec-drift` subscriber (`independent`: an event it cannot read is
/// quarantined and counted, never retried forever).
#[derive(Debug, Default, Clone, Copy)]
pub struct SpecDriftRequests;

fn bad_payload(event: &SourceEvent, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_event_payload")
        .with_context("event_type", event.event_type.clone()) // clone: the context owns it
        .with_context("field", field)
        .with_context("seq", event.seq)
}

impl SpecDriftRequests {
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
        let origin = event.request_id().unwrap_or_else(RequestId::generate);
        let job = spec_drift_job(&SpecDriftRequest { project, head }, origin, Some(event.seq))?;
        // Deduped: a retried step, or a head evaluated before.
        let _enqueued = enqueue_on(tx.conn(), &job).await?;
        Ok(())
    }
}

impl Subscriber for SpecDriftRequests {
    fn name(&self) -> &'static str {
        SPEC_DRIFT_SUBSCRIBER
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
                    subscriber = SPEC_DRIFT_SUBSCRIBER,
                    request_id = %event.request_id,
                    source_seq = event.seq,
                    event_type = %event.event_type,
                    "an event type this build does not know was passed over"
                );
                return Ok(());
            };
            match kind {
                EventType::MergeRequestPushed => Self::on_version(event, tx).await,
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
                | EventType::MergeRequestUpdated
                | EventType::MergeRequestDraftChanged
                | EventType::MergeRequestClosed
                | EventType::MergeRequestReopened
                | EventType::MergeRequestConflictDetected
                | EventType::MergeRequestConflictResolved
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
                | EventType::WebhookDisabled => Ok(()),
            }
        })
    }
}
