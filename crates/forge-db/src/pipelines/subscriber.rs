//! The `ci-pipelines` subscriber (M4 C2, plan 14): every push to a branch
//! (`repository.pushed`) and every merge request version
//! (`merge_request.pushed`) enqueues one `ci.pipeline_create` job. The job
//! reads `.forge-ci.yml` with git and plans it (forge-pipelines, run by
//! `forge worker`): a subscriber never calls git (ADR-0016 §7).
//!
//! - A push: each listed `refs/heads/*` that was created, fast-forwarded or
//!   forced gets a job for its new commit (a deleted branch gets none), at
//!   most [`MAX_PUSH_PIPELINES`] per push. The rest — beyond the cap, or
//!   refs the event could not list (`refs_truncated`) — are logged at warn
//!   (`forge.ci.push_refs_not_planned`, with the counts): never dropped
//!   silently. The pusher (a person's or a service account's event) is
//!   `triggered_by`.
//! - A merge request version: a job for its head on its source branch,
//!   `triggered_by` the pusher (`pushed_by`; none for the first version).
//!
//! Exactly once: the job's dedup key is the pipeline's identity (project,
//! source, merge request, sha, ref), so a retried step or a redelivered
//! event enqueues nothing new, and `ci_pipelines_once` keeps a second job
//! from writing a second pipeline. `independent`: an event this build
//! cannot read is quarantined (doctor's `subscribers.dead_letters`).

use forge_core::{EventType, MergeRequestId, ObjectId, RefName, RequestId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use super::{create_job, PipelineRequest, Source};
use crate::jobs::enqueue_on;
use crate::query_error;
use crate::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};

/// The subscriber's name (`forge.event_subscribers`).
pub const PIPELINES_SUBSCRIBER: &str = "ci-pipelines";
/// The most pipelines one push asks for.
pub const MAX_PUSH_PIPELINES: usize = 20;

/// The subscriber.
#[derive(Debug, Default, Clone, Copy)]
pub struct PipelineRequests;

fn bad_payload(event: &SourceEvent, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_event_payload")
        .with_context("event_type", event.event_type.clone()) // clone: the context owns it
        .with_context("field", field)
        .with_context("seq", event.seq)
}

/// The pusher of a person's or a service account's event.
fn pusher(event: &SourceEvent) -> Option<UserId> {
    match event.actor_kind.as_str() {
        "user" | "service_account" => event.actor_user_id.filter(|id| *id > 0).map(UserId::new),
        _ => None,
    }
}

/// The branches a push created or moved, with their new commits, and how
/// many updated refs the event did not list.
fn moved_branches(event: &SourceEvent) -> Result<(Vec<(RefName, ObjectId)>, u64), ForgeError> {
    let payload = &event.payload;
    let refs = payload
        .get("refs")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| bad_payload(event, "refs"))?;
    let total = payload
        .get("total_refs")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| bad_payload(event, "total_refs"))?;
    let unlisted = total.saturating_sub(refs.len() as u64);
    let mut moved = Vec::new();
    for entry in refs {
        let name = entry
            .get("ref")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| bad_payload(event, "refs.ref"))?;
        let kind = entry
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| bad_payload(event, "refs.kind"))?;
        if !name.starts_with("refs/heads/") || kind == "delete" {
            continue;
        }
        let git_ref = RefName::parse(name).map_err(|_| bad_payload(event, "refs.ref"))?;
        let new = entry
            .get("new")
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| ObjectId::parse(raw).ok())
            .ok_or_else(|| bad_payload(event, "refs.new"))?;
        moved.push((git_ref, new));
    }
    Ok((moved, unlisted))
}

impl PipelineRequests {
    async fn on_push(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        let project = event
            .project_id
            .ok_or_else(|| bad_payload(event, "project_id"))?;
        let (moved, unlisted) = moved_branches(event)?;
        let origin = event.request_id().unwrap_or_else(RequestId::generate);
        let over = moved.len().saturating_sub(MAX_PUSH_PIPELINES);
        for (git_ref, sha) in moved.into_iter().take(MAX_PUSH_PIPELINES) {
            let request = PipelineRequest {
                project,
                git_ref,
                sha,
                source: Source::Push,
                merge_request: None,
                triggered_by: pusher(event),
            };
            // Deduped: a retried step or a redelivered event.
            let _enqueued = enqueue_on(
                tx.conn(),
                &create_job(&request, origin.clone(), Some(event.seq))?, // clone: each job owns its id
            )
            .await?;
        }
        if over > 0 || unlisted > 0 {
            tracing::warn!(
                target: "forge_db",
                event = "forge.ci.push_refs_not_planned",
                request_id = %event.request_id,
                source_seq = event.seq,
                project_id = project.get(),
                over_cap = over,
                unlisted_refs = unlisted,
                cap = MAX_PUSH_PIPELINES,
                "some branches of a push got no pipeline; run one by hand if needed"
            );
        }
        Ok(())
    }

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
        let pushed_by = match event.payload.get("pushed_by") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => Some(
                v.as_i64()
                    .filter(|id| *id > 0)
                    .map(UserId::new)
                    .ok_or_else(|| bad_payload(event, "pushed_by"))?,
            ),
        };
        let source: Option<String> = sqlx::query_scalar(
            "SELECT source_branch FROM forge.merge_requests WHERE id = $1 AND project_id = $2",
        )
        .bind(mr)
        .bind(project.get())
        .fetch_optional(tx.conn())
        .await
        .map_err(|e| query_error("pipelines.version_source", e))?;
        // A merge request purged with its project: nothing to plan.
        let Some(source) = source else {
            return Ok(());
        };
        let git_ref = RefName::parse(&source).map_err(|_| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "stored_branch_invalid")
                .with_context("merge_request_id", mr)
        })?;
        let request = PipelineRequest {
            project,
            git_ref,
            sha: head,
            source: Source::MergeRequest,
            merge_request: Some(MergeRequestId::new(mr)),
            triggered_by: pushed_by,
        };
        let origin = event.request_id().unwrap_or_else(RequestId::generate);
        let _enqueued =
            enqueue_on(tx.conn(), &create_job(&request, origin, Some(event.seq))?).await?;
        Ok(())
    }
}

impl Subscriber for PipelineRequests {
    fn name(&self) -> &'static str {
        PIPELINES_SUBSCRIBER
    }

    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            let Some(kind) = EventType::parse(&event.event_type) else {
                tracing::warn!(
                    target: "forge_db",
                    event = "forge.subscribers.unknown_event_type",
                    subscriber = PIPELINES_SUBSCRIBER,
                    request_id = %event.request_id,
                    source_seq = event.seq,
                    event_type = %event.event_type,
                    "an event type this build does not know was passed over"
                );
                return Ok(());
            };
            match kind {
                EventType::RepositoryPushed => Self::on_push(event, tx).await,
                EventType::MergeRequestPushed => Self::on_version(event, tx).await,
                EventType::ProjectCreated
                | EventType::RepositoryCreated
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
                | EventType::WebhookDisabled => Ok(()),
            }
        })
    }
}
