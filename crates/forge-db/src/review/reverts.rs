//! One-step revert (M4 C10, plan 04 M-4, ADR-0024 Amendment C10): the
//! database side of a revert merge request and of the draft the system
//! opens when a target branch's pipeline fails right after a merge.
//!
//! - [`applied_merge_at`]: the applied merge attempt of a project whose
//!   merge produced commit `sha` (a merge commit, a squash commit or the
//!   fast-forwarded head), the newest when several did.
//! - [`push_pipeline_status`]: the status of the newest push pipeline of a
//!   branch at a commit (the target branch's own CI after the merge).
//! - [`RevertDrafts`] (subscriber `revert-drafts`, `independent`): every
//!   `check.completed` of `ci/pipeline` that concluded `failure` on a
//!   commit an applied merge of the project produced enqueues one
//!   `merge_request.revert_draft` job (dedup
//!   `mr_revert_draft:<project_id>:<sha>`: once per commit, however often
//!   the check is reported). A subscriber never calls git (ADR-0016 §7):
//!   `forge serve` runs the job (forge-review's `Reverter`), next to the
//!   browse cache its branch write invalidates.
//! - [`record_revert_drafted`]: the `merge_request.revert_drafted` audit
//!   row and event of what the job did (actor `system`), deduplicated by
//!   `merge_request_revert_drafted:<merge_request_id>:<merge_id>`.
//!
//! No table of its own: the job's dedup key makes the draft once per
//! failing commit, the event's dedup key records it once per merge, and the
//! revert branch (`refs/heads/revert/mr-<iid>`) links the revert to what it
//! reverts.

use std::time::Duration;

use forge_core::checks::CI_PIPELINE_CHECK;
use forge_core::ids::{MergeRequestId, ProjectId};
use forge_core::{
    AuditDetails, DedupKey as EventDedupKey, DedupPart, EventType, MergeRequestChange, ObjectId,
    RefName, RequestId, UserId, UserKind,
};
use forge_errors::{ErrorCode, ForgeError};

use super::merges::{columns, MergeRow, StoredMerge};
use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Committed, DomainEvent, Outcome,
};
use crate::jobs::{enqueue_on, DedupKey, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};
use crate::{query_error, read_query_error, Db};

/// The job that drafts a revert after a failed pipeline.
pub const REVERT_DRAFT_JOB: &str = "merge_request.revert_draft";
/// The subscriber's name (`forge.event_subscribers`).
pub const REVERT_DRAFTS_SUBSCRIBER: &str = "revert-drafts";
/// The bound of one draft (git: three trees, three commits, one merge-tree,
/// one commit, one ref; the database: a few short transactions).
pub const REVERT_DRAFT_TIMEOUT: Duration = Duration::from_secs(120);

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", REVERT_DRAFT_JOB)
}

/// What a `merge_request.revert_draft` job asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertDraftRequest {
    pub project: ProjectId,
    /// The commit whose `ci/pipeline` failed.
    pub sha: ObjectId,
}

/// The job for `request` (module docs).
pub fn revert_draft_job(
    request: &RevertDraftRequest,
    origin_request_id: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let key = format!(
        "mr_revert_draft:{}:{}",
        request.project.get(),
        request.sha.as_str()
    );
    Ok(NewJob {
        kind: JobKind::parse(REVERT_DRAFT_JOB).ok_or_else(|| internal("invalid_job_kind"))?,
        payload: serde_json::json!({
            "project_id": request.project.get(),
            "sha": request.sha.as_str(),
        }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(REVERT_DRAFT_TIMEOUT).ok_or_else(|| internal("job_timeout"))?,
        dedup_key: Some(DedupKey::parse(&key).ok_or_else(|| internal("dedup_key"))?),
        origin_request_id,
        origin_event_seq,
    })
}

/// The request of a claimed job (`FG-SYS-0001 reason=invalid_job_payload`
/// for anything else: forge wrote it).
pub fn revert_draft_payload(payload: &serde_json::Value) -> Result<RevertDraftRequest, ForgeError> {
    let bad = || internal("invalid_job_payload");
    let project = payload
        .get("project_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(bad)?;
    let sha = payload
        .get("sha")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| ObjectId::parse(raw).ok())
        .ok_or_else(bad)?;
    Ok(RevertDraftRequest {
        project: ProjectId::new(project),
        sha,
    })
}

/// The newest applied merge attempt of `project` that moved its target to
/// `sha`. `None`: no merge produced that commit.
pub async fn applied_merge_at(
    db: &Db,
    project: ProjectId,
    sha: &ObjectId,
) -> Result<Option<StoredMerge>, ForgeError> {
    const QUERY: &str = "review.revert_applied_merge";
    let row: Option<MergeRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_request_merges WHERE project_id = $1 \
                 AND state = 'applied' AND new_sha = $2 ORDER BY id DESC LIMIT 1",
                columns()
            ))
            .bind(project.get())
            .bind(sha.as_str())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(MergeRow::parse).transpose()
}

/// The status of the newest push pipeline of branch `git_ref` at `sha` in
/// `project` (`pending`, `running`, `succeeded`, `failed`, …); `None`: no
/// push pipeline ran there.
pub async fn push_pipeline_status(
    db: &Db,
    project: ProjectId,
    git_ref: &RefName,
    sha: &ObjectId,
) -> Result<Option<String>, ForgeError> {
    const QUERY: &str = "review.revert_push_pipeline";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT status FROM forge.ci_pipelines WHERE project_id = $1 AND source = 'push' \
             AND ref = $2 AND sha = $3 ORDER BY id DESC LIMIT 1",
        )
        .bind(project.get())
        .bind(git_ref.as_str())
        .bind(sha.as_str())
        .fetch_optional(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// The iid of merge request `mr` (`FG-REPO-0005 resource=merge_request`
/// when it is gone).
pub async fn merge_request_iid(db: &Db, mr: MergeRequestId) -> Result<i64, ForgeError> {
    super::merges::iid_of(db, mr).await
}

/// `user`'s kind and account flags as they are now (`kind`, `active`,
/// `is_admin` — the standing flag, `AdminSource::standing_admin`: a flag
/// the identity provider's role granted does not count outside a session,
/// ADR-0021 amendment 2026-11-24); `None`: no such user. The system's draft decides as the
/// reverted merge request's author with them.
pub async fn account_facts(
    db: &Db,
    user: UserId,
) -> Result<Option<(UserKind, bool, bool)>, ForgeError> {
    const QUERY: &str = "review.revert_author";
    let row: Option<(String, bool, bool)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT kind, state = 'active', is_admin AND admin_source = 'cli' \
                 FROM forge.users WHERE id = $1",
            )
                .bind(user.get())
                .fetch_optional(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(|(kind, active, is_admin)| {
        UserKind::parse(&kind)
            .map(|k| (k, active, is_admin))
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "stored_user_kind_invalid")
                    .with_context("user_id", user.get())
            })
    })
    .transpose()
}

/// Record what the system's revert draft did (`change` is a
/// [`MergeRequestChange::RevertDrafted`]) on `project`: one audit row and
/// the event. `false`: already recorded for this merge (nothing written).
pub async fn record_revert_drafted(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    change: MergeRequestChange,
) -> Result<bool, ForgeError> {
    let MergeRequestChange::RevertDrafted {
        merge_request,
        merge_id,
        ..
    } = &change
    else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "not_a_revert_draft")
        );
    };
    let dedup = EventDedupKey::new(
        change.dedup_prefix(),
        &[DedupPart::Id(merge_request.get()), DedupPart::Id(*merge_id)],
    )?;
    let aggregate: MergeRequestId = *merge_request;
    let details = AuditDetails::MergeRequest(change);
    let event_type: EventType = details.event_type().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "audit_only_event")
    })?;
    let value = details.to_value();
    let record = ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: value.clone(), // clone: the same JSON is the payload
        },
        affected: Vec::new(),
        events: vec![DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: "merge_request",
            aggregate_id: aggregate.get(),
            project_id: Some(project),
            payload: value,
            dedup_key: Some(dedup),
        }],
    };
    let tx = ChangeTx::begin(db, ctx).await?;
    Ok(match tx.commit(record).await? {
        Committed::Recorded { .. } => true,
        Committed::Deduped => false,
    })
}

/// The subscriber (module docs).
#[derive(Debug, Default, Clone, Copy)]
pub struct RevertDrafts;

fn bad_payload(event: &SourceEvent, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_event_payload")
        .with_context("subscriber", REVERT_DRAFTS_SUBSCRIBER)
        .with_context("event_type", event.event_type.clone()) // clone: the context owns it
        .with_context("field", field)
        .with_context("seq", event.seq)
}

impl RevertDrafts {
    async fn on_check(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        let payload = &event.payload;
        let name = payload
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| bad_payload(event, "name"))?;
        let failed = payload
            .get("conclusion")
            .and_then(serde_json::Value::as_str)
            == Some("failure");
        if name != CI_PIPELINE_CHECK || !failed {
            return Ok(());
        }
        let project = event
            .project_id
            .ok_or_else(|| bad_payload(event, "project_id"))?;
        let sha = payload
            .get("head_sha")
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| ObjectId::parse(raw).ok())
            .ok_or_else(|| bad_payload(event, "head_sha"))?;
        // Most failures are of a merge request's merge result or a feature
        // branch: only a commit a merge produced is a candidate.
        const QUERY: &str = "review.revert_candidate";
        let merged: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.merge_request_merges WHERE project_id = $1 \
             AND state = 'applied' AND new_sha = $2)",
        )
        .bind(project.get())
        .bind(sha.as_str())
        .fetch_one(tx.conn())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        if !merged {
            return Ok(());
        }
        let origin = event.request_id().unwrap_or_else(RequestId::generate);
        let job = revert_draft_job(
            &RevertDraftRequest { project, sha },
            origin,
            Some(event.seq),
        )?;
        // Deduped: the same commit's failure reported again.
        let _enqueued = enqueue_on(tx.conn(), &job).await?;
        Ok(())
    }
}

impl Subscriber for RevertDrafts {
    fn name(&self) -> &'static str {
        REVERT_DRAFTS_SUBSCRIBER
    }

    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            match EventType::parse(&event.event_type) {
                Some(EventType::CheckCompleted) => Self::on_check(event, tx).await,
                // Every other type: nothing to draft.
                Some(_) => Ok(()),
                None => {
                    // A type this build does not know (a newer writer). The
                    // subscriber is independent, so it goes on, never silently.
                    tracing::warn!(
                        target: "forge_db",
                        event = "forge.subscribers.unknown_event_type",
                        subscriber = REVERT_DRAFTS_SUBSCRIBER,
                        request_id = %event.request_id,
                        source_seq = event.seq,
                        event_type = %event.event_type,
                        "an event type this build does not know was passed over"
                    );
                    Ok(())
                }
            }
        })
    }
}

#[cfg(test)]
#[path = "reverts_tests.rs"]
mod tests;
