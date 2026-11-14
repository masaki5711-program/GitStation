//! The merge queue's two decisions on a merge request (M4 C8, ADR-0030
//! §2, §6), made like every merge decision: under the review locks, with
//! the facts of [`MergeGate`] handed to forge-review's `decide`.
//!
//! - [`enqueue_mr_decided`]: "merge when ready" — the live entry of the
//!   merge request in its target's queue (the queue row is created by the
//!   first one) and one `merge_queue.enqueued` audit row.
//! - [`queue_merge_mr_decided`]: one step of a landing — decided again for
//!   the person who enqueued (their credential as it is now; a web session
//!   that only timed out still counts, ADR-0030 §6.2), then the merge
//!   attempt written straight as `prepared` with the train's commits (no
//!   merge job: nothing else may build another commit for it) and the
//!   entry's `merge_id`, in one derived-only commit.

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::merge_queue::{EntryState, MergeQueueChange};
use forge_core::{AuditDetails, MergeMethod, MergeRequestState, ObjectId, RefName};
use forge_errors::ForgeError;

use super::credential::CredentialState;
use super::request::{gate, live_on, MergeGate};
use super::{columns, in_progress, CredentialRef, MergeRow, StoredMerge};
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::review::commands::{absent, facts};
use crate::review::merge_queue::{entry_columns, refused, EntryRow, StoredEntry};
use crate::{query_error, Db};

/// What an enqueue asks for (forge-review checked the method and the head).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEntry {
    pub head_sha: ObjectId,
    pub method: MergeMethod,
    /// The frozen commit message (`None`: built from the merge request as
    /// locked, like a merge request's).
    pub message: Option<String>,
    pub credential: CredentialRef,
}

/// The audit row of a person's change to a queue (resource the project,
/// no event).
pub(crate) fn queue_record(change: MergeQueueChange) -> ChangeRecord {
    let project = change.project();
    let details = AuditDetails::MergeQueue(change);
    ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

/// The queue of (`project`, `target`), created when missing; its id.
pub(crate) async fn queue_on(
    conn: &mut sqlx::PgConnection,
    project: ProjectId,
    target: &RefName,
) -> Result<i64, ForgeError> {
    sqlx::query_scalar(
        "INSERT INTO forge.merge_queues (project_id, target_branch) VALUES ($1, $2) \
         ON CONFLICT (project_id, target_branch) DO UPDATE SET updated_at = now() \
         RETURNING id",
    )
    .bind(project.get())
    .bind(target.as_str())
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("merge_queue.queue", e))
}

/// The live entry of `mr`, locked (`None`: none).
pub(crate) async fn live_entry_on(
    conn: &mut sqlx::PgConnection,
    mr: MergeRequestId,
) -> Result<Option<StoredEntry>, ForgeError> {
    let row: Option<EntryRow> = sqlx::query_as(&format!(
        "SELECT {} FROM forge.merge_queue_entries e \
         JOIN forge.merge_requests m ON m.id = e.merge_request_id \
         WHERE e.merge_request_id = $1 AND e.state IN ('queued', 'testing') FOR UPDATE OF e",
        entry_columns()
    ))
    .bind(mr.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("merge_queue.live_entry", e))?;
    row.map(EntryRow::parse).transpose()
}

/// Add merge request `iid` of `project` to its target's queue for
/// `viewer` (ADR-0030 §2): under the review locks the facts are read and
/// `decide` runs (forge-review: the reconfirmed `authorize()` and
/// `evaluate_merge()` with `ci/pipeline` left to the queue); then a live
/// merge attempt is `FG-REVIEW-0011`, a live entry `FG-CI-0025
/// reason=already_queued`, a merge request that is not open `FG-CI-0025
/// reason=not_open`; else the entry and the `merge_queue.enqueued` audit
/// row are written. Returns the entry.
pub async fn enqueue_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    viewer: UserId,
    new: &NewEntry,
    decide: impl FnOnce(&MergeGate) -> Result<P, ForgeError>,
    default_message: impl FnOnce(&crate::review::StoredMergeRequest) -> String,
) -> Result<(StoredEntry, P), ForgeError> {
    const ACTION: &str = "merge_queue.enqueued";
    let auto = new.credential.is_auto_merge();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let mr = facts.merge_request.as_ref().ok_or_else(absent)?.clone(); // clone: the gate keeps the facts
        let live = tx.step(|c| async move { live_on(c, mr.id).await }).await?;
        let gate = gate(&mut tx, facts, viewer, new.credential, live).await?;
        let decided = decide(&gate)?;
        if let Some(live) = &gate.live {
            return Err(in_progress(mr.id, Some(live.id), ACTION)
                .with_context("state", live.state.as_str()));
        }
        if mr.state != MergeRequestState::Open {
            return Err(refused("not_open", mr.id).with_context("state", mr.state.as_str()));
        }
        let existing = tx
            .step(|c| async move { live_entry_on(c, mr.id).await })
            .await?;
        if let Some(entry) = existing {
            return Err(refused("already_queued", mr.id).with_context("entry_id", entry.id));
        }
        let target = mr.target_branch.clone(); // clone: the step owns its name
        let queue = tx
            .step(|c| async move { queue_on(c, project, &target).await })
            .await?;
        let message = new
            .message
            .clone() // clone: the step owns its message
            .unwrap_or_else(|| default_message(&mr));
        let new = new.clone(); // clone: moved into the step
        let request_id = ctx.request_id.as_str().to_owned();
        let mr_id = mr.id;
        let id: i64 = tx
            .step(|c| async move {
                let id = sqlx::query_scalar(
                    "INSERT INTO forge.merge_queue_entries (queue_id, project_id, \
                     merge_request_id, state, head_sha, method, message, enqueued_by, \
                     credential_kind, credential_id, request_id) \
                     VALUES ($1, $2, $3, 'queued', $4, $5, $6, $7, $8, $9, $10) RETURNING id",
                )
                .bind(queue)
                .bind(project.get())
                .bind(mr_id.get())
                .bind(new.head_sha.as_str())
                .bind(new.method.as_str())
                .bind(message)
                .bind(viewer.get())
                .bind(new.credential.kind())
                .bind(new.credential.id())
                .bind(request_id)
                .fetch_one(&mut *c)
                .await
                .map_err(|e| {
                    let unique = e
                        .as_database_error()
                        .and_then(|d| d.constraint())
                        .is_some_and(|c| c == "merge_queue_entries_one_live");
                    if unique {
                        refused("already_queued", mr_id)
                    } else {
                        query_error("merge_queue.enqueue", e)
                    }
                })?;
                sqlx::query("UPDATE forge.merge_queues SET next_run_at = now() WHERE id = $1")
                    .bind(queue)
                    .execute(c)
                    .await
                    .map_err(|e| query_error("merge_queue.wake", e))?;
                Ok(id)
            })
            .await?;
        let entry = tx
            .step(|c| async move {
                let row: EntryRow = sqlx::query_as(&format!(
                    "SELECT {} FROM forge.merge_queue_entries e \
                     JOIN forge.merge_requests m ON m.id = e.merge_request_id WHERE e.id = $1",
                    entry_columns()
                ))
                .bind(id)
                .fetch_one(c)
                .await
                .map_err(|e| query_error("merge_queue.enqueued", e))?;
                row.parse()
            })
            .await?;
        let (target_branch, head_sha) = (
            mr.target_branch.as_str().to_owned(),
            entry.head_sha.as_str().to_owned(),
        );
        // M4 C9: the auto-merge principal's entry is its own audit action
        // (actor system; `enqueued_by` is the author it lands for).
        let change = if auto {
            MergeQueueChange::AutoEnqueued {
                project,
                merge_request: mr.id,
                iid: mr.iid,
                entry: entry.id,
                target_branch,
                head_sha,
            }
        } else {
            MergeQueueChange::Enqueued {
                project,
                merge_request: mr.id,
                iid: mr.iid,
                entry: entry.id,
                target_branch,
                head_sha,
            }
        };
        Ok(Settle::Commit(queue_record(change), (entry, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// One landing step's commits (ADR-0030 §6): the attempt moves the target
/// from `expected_old` (the previous step of the train, the base for the
/// first) to `new_sha` (this step).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStep {
    pub expected_old: ObjectId,
    pub new_sha: ObjectId,
}

/// Decide entry `entry` again for the person who enqueued it and write its
/// merge attempt `prepared` with `step` (module docs). `ctx` is the
/// batch's system context. Errors (nothing written): `decide`'s refusal,
/// `FG-REVIEW-0011` (another live attempt), `FG-CI-0025
/// reason=not_testing` (the entry left the batch meanwhile), `FG-DB-*`.
pub async fn queue_merge_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    entry: &StoredEntry,
    step: &QueueStep,
    decide: impl FnOnce(&MergeGate) -> Result<P, ForgeError>,
) -> Result<(StoredMerge, P), ForgeError> {
    const ACTION: &str = "merge_queue.land_step";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, entry.project_id, Some(entry.iid), entry.enqueued_by).await?;
        let mr = facts.merge_request.as_ref().ok_or_else(absent)?.clone(); // clone: the gate keeps the facts
        let live = tx.step(|c| async move { live_on(c, mr.id).await }).await?;
        let mut gate = gate(&mut tx, facts, entry.enqueued_by, entry.credential, live).await?;
        if let (CredentialRef::Session(id), CredentialState::Session { usable: false }) =
            (entry.credential, &gate.credential.credential)
        {
            // ADR-0030 §6.2: "merge when ready" outlives a session's idle or
            // absolute expiry; a revoked session (logout, an administrator's
            // revocation) still stops it.
            let user = entry.enqueued_by;
            let alive: Option<bool> = tx
                .step(|c| async move {
                    sqlx::query_scalar(
                        "SELECT revoked_at IS NULL FROM forge.web_sessions \
                         WHERE id = $1 AND user_id = $2",
                    )
                    .bind(id)
                    .bind(user.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("merge_queue.session", e))
                })
                .await?;
            gate.credential.credential = match alive {
                Some(usable) => CredentialState::Session { usable },
                None => CredentialState::Missing,
            };
        }
        let decided = decide(&gate)?;
        if let Some(live) = &gate.live {
            return Err(in_progress(mr.id, Some(live.id), ACTION)
                .with_context("state", live.state.as_str()));
        }
        let entry_id = entry.id;
        let state: Option<(String, Option<i64>)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT state, merge_id FROM forge.merge_queue_entries WHERE id = $1 FOR UPDATE",
                )
                .bind(entry_id)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("merge_queue.land_entry", e))
            })
            .await?;
        match state {
            Some((s, None)) if s == EntryState::Testing.as_str() => {}
            other => {
                return Err(refused("not_testing", mr.id)
                    .with_context("entry_id", entry_id)
                    .with_context("state", other.map_or_else(|| "gone".to_owned(), |(s, _)| s)))
            }
        }
        let (entry, step) = (entry.clone(), step.clone()); // clone: moved into the step
        let request_id = ctx.request_id.as_str().to_owned();
        let row: MergeRow = tx
            .step(|c| async move {
                // The guard's one entry is `queued` (20261110100100): the
                // attempt is inserted so and moved to `prepared` with the
                // train's commits in the same transaction.
                let id: i64 = sqlx::query_scalar(
                    "INSERT INTO forge.merge_request_merges (merge_request_id, project_id,                      method, state, requested_by, credential_kind, credential_id, request_id,                      head_sha, target_branch, message, delete_source_branch) VALUES ($1, $2,                      $3, 'queued', $4, $5, $6, $7, $8, $9, $10, false) RETURNING id",
                )
                .bind(entry.merge_request.get())
                .bind(entry.project_id.get())
                .bind(entry.method.as_str())
                .bind(entry.enqueued_by.get())
                .bind(entry.credential.kind())
                .bind(entry.credential.id())
                .bind(request_id)
                .bind(entry.head_sha.as_str())
                .bind(mr.target_branch.as_str())
                .bind(&entry.message)
                .fetch_one(&mut *c)
                .await
                .map_err(|e| {
                    let unique = e
                        .as_database_error()
                        .and_then(|d| d.constraint())
                        .is_some_and(|c| c == "merge_request_merges_one_live");
                    if unique {
                        in_progress(entry.merge_request, None, ACTION)
                    } else {
                        query_error("merge_queue.land_attempt", e)
                    }
                })?;
                let row: MergeRow = sqlx::query_as(&format!(
                    "UPDATE forge.merge_request_merges SET state = 'prepared',                      expected_old = $2, new_sha = $3, updated_at = now()                      WHERE id = $1 AND state = 'queued' RETURNING {}",
                    columns()
                ))
                .bind(id)
                .bind(step.expected_old.as_str())
                .bind(step.new_sha.as_str())
                .fetch_one(&mut *c)
                .await
                .map_err(|e| query_error("merge_queue.land_prepare", e))?;
                sqlx::query(
                    "UPDATE forge.merge_queue_entries SET merge_id = $2, updated_at = now() \
                     WHERE id = $1",
                )
                .bind(entry.id)
                .bind(row.id)
                .execute(c)
                .await
                .map_err(|e| query_error("merge_queue.land_merge_id", e))?;
                Ok(row)
            })
            .await?;
        Ok(Settle::CommitDerived((row.parse()?, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
