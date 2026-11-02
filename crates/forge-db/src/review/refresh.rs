//! The `merge-request-refresh` subscriber (M3 R1b, plan 11 §11.3): on
//! `repository.pushed` it ENQUEUES the version job (`merge_request.refresh`)
//! for every open merge request whose source branch the push moved; the
//! job reads the branch with git and writes the version (a subscriber
//! never calls git: external calls belong in jobs, ADR-0016 §7).
//!
//! - When the event lists every ref (`refs_truncated = false`), the merge
//!   requests are those whose `source_branch` is a listed ref that was
//!   created, fast-forwarded or forced (a deleted branch adds no version).
//! - When the list was cut (`refs_truncated = true`, more than
//!   `MAX_LISTED_REFS` refs), EVERY open merge request of the project is
//!   re-checked: nothing is missed silently (a job whose branch did not
//!   move writes nothing).
//!
//! Each job's dedup key is `mr_refresh:<merge_request_id>:push:<seq>`: a
//! retried step never doubles a job, and each push gets its own. The pusher
//! (`actor_user_id` of a `user` event) becomes the version's `pushed_by`.
//! `independent`: an event that keeps failing is quarantined (doctor's
//! `subscribers.dead_letters` alerts) instead of holding up every later
//! push.

use forge_core::ids::{MergeRequestId, UserId};
use forge_core::{EventType, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use super::versions::{refresh_job, RefreshRequest, RefreshTrigger};
use crate::jobs::enqueue_on;
use crate::query_error;
use crate::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};

/// The subscriber's name (its row in forge.event_subscribers).
pub const REFRESH_SUBSCRIBER: &str = "merge-request-refresh";

/// The subscriber (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct MergeRequestRefresh;

fn bad_payload(event: &SourceEvent, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_event_payload")
        .with_context("event_type", event.event_type.clone()) // clone: the context owns it
        .with_context("field", field)
        .with_context("seq", event.seq)
}

/// The source branches a push moved (`None`: the list was cut, every open
/// merge request is re-checked).
pub(crate) fn moved_branches(event: &SourceEvent) -> Result<Option<Vec<String>>, ForgeError> {
    let payload = &event.payload;
    let truncated = payload
        .get("refs_truncated")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| bad_payload(event, "refs_truncated"))?;
    if truncated {
        return Ok(None);
    }
    let refs = payload
        .get("refs")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| bad_payload(event, "refs"))?;
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
        if name.starts_with("refs/heads/") && kind != "delete" {
            moved.push(name.to_owned());
        }
    }
    Ok(Some(moved))
}

impl MergeRequestRefresh {
    async fn on_push(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        let project = event
            .project_id
            .ok_or_else(|| bad_payload(event, "project_id"))?;
        let moved = moved_branches(event)?;
        if moved.as_ref().is_some_and(Vec::is_empty) {
            return Ok(());
        }
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM forge.merge_requests WHERE project_id = $1 AND state = 'open' \
             AND ($2::text[] IS NULL OR source_branch = ANY($2)) ORDER BY id",
        )
        .bind(project.get())
        .bind(moved)
        .fetch_all(tx.conn())
        .await
        .map_err(|e| query_error("review.refresh_targets", e))?;
        let pushed_by = (event.actor_kind == "user")
            .then_some(event.actor_user_id)
            .flatten()
            .map(UserId::new);
        let origin = event.request_id().unwrap_or_else(RequestId::generate);
        for id in ids {
            let job = refresh_job(
                RefreshRequest {
                    merge_request: MergeRequestId::new(id),
                    pushed_by,
                },
                RefreshTrigger::Pushed { seq: event.seq },
                origin.clone(), // clone: each job keeps the origin request
                Some(event.seq),
            )?;
            // Deduped: a retried step already enqueued it.
            let _enqueued = enqueue_on(tx.conn(), &job).await?;
        }
        Ok(())
    }
}

impl Subscriber for MergeRequestRefresh {
    fn name(&self) -> &'static str {
        REFRESH_SUBSCRIBER
    }

    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            let Some(kind) = EventType::parse(&event.event_type) else {
                // A type this build does not know (a newer writer): not ours.
                return Ok(());
            };
            match kind {
                EventType::RepositoryPushed => Self::on_push(event, tx).await,
                // Target pushes feed the mergeability job (R2), not versions.
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
                | EventType::MergeRequestPushed => Ok(()),
            }
        })
    }
}
