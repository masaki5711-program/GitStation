//! The merge queue's rows (M4 C8, plan 14 §4.1, ADR-0030; migration
//! 20261213100000): `forge.merge_queues`, `forge.merge_queue_batches`,
//! `forge.merge_queue_entries`. The git work and every decision are
//! forge-review's (`forge_review::merges::queue`); this module reads facts
//! and moves rows with conditional updates, so two workers (or a worker and
//! a person) never both move one row.
//!
//! - **A person's commands** (audited on the project, `ChangeTx`): the
//!   enqueue (`merges::enqueue_mr_decided`: it needs the merge gate),
//!   [`dequeue_mr_decided`], [`set_paused_decided`].
//! - **Reads** for the routes: [`queue_view`], [`entry_of`].
//! - **The worker** ([`claim`] … [`release`]): a queue is claimed with
//!   `FOR UPDATE SKIP LOCKED` and a lease, its step runs without a
//!   transaction held (git), each write is its own short transaction.

use std::time::Duration;

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::merge_queue::{BatchState, EntryState, Isolation, MergeQueueChange, MAX_BATCH};
use forge_core::{MergeMethod, ObjectId, RefName, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use super::merges::{queue_on, queue_record, CredentialRef};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::jobs::{enqueue_on, NewJob};
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::review::commands::{absent, facts, MergeRequestFacts};
use crate::{query_error, read_query_error, Db};

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

/// `FG-CI-0025 reason=<reason>` (ADR-0030 §2, §10): the queue's own state
/// refuses the change; nothing was written.
pub fn refused(reason: &'static str, mr: MergeRequestId) -> ForgeError {
    ForgeError::new(ErrorCode::MergeQueueEntryRefused)
        .with_context("reason", reason)
        .with_context("merge_request_id", mr.get())
}

fn unreadable(reason: &'static str, id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("merge_queue_row", id)
}

fn oid(raw: &str, id: i64) -> Result<ObjectId, ForgeError> {
    ObjectId::parse(raw).map_err(|_| unreadable("stored_merge_queue_sha_invalid", id))
}

/// One queue as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredQueue {
    pub id: i64,
    pub project_id: ProjectId,
    pub target_branch: RefName,
    pub paused: bool,
    pub paused_by: Option<UserId>,
    pub paused_at: Option<String>,
    pub batch_limit: usize,
}

#[derive(sqlx::FromRow)]
struct QueueRow {
    id: i64,
    project_id: i64,
    target_branch: String,
    paused: bool,
    paused_by: Option<i64>,
    paused_at: Option<String>,
    batch_limit: i32,
}

fn queue_columns() -> String {
    format!(
        "id, project_id, target_branch, paused, paused_by, \
         to_char(paused_at AT TIME ZONE 'UTC', {TS}) AS paused_at, batch_limit"
    )
}

impl QueueRow {
    fn parse(self) -> Result<StoredQueue, ForgeError> {
        let id = self.id;
        Ok(StoredQueue {
            id,
            project_id: ProjectId::new(self.project_id),
            target_branch: RefName::parse(&self.target_branch)
                .ok()
                .filter(|r| r.branch().is_some())
                .ok_or_else(|| unreadable("stored_queue_target_invalid", id))?,
            paused: self.paused,
            paused_by: self.paused_by.map(UserId::new),
            paused_at: self.paused_at,
            batch_limit: usize::try_from(self.batch_limit)
                .ok()
                .filter(|l| (1..=MAX_BATCH).contains(l))
                .ok_or_else(|| unreadable("stored_queue_limit_invalid", id))?,
        })
    }
}

/// One entry as stored (with its merge request's iid).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEntry {
    pub id: i64,
    pub queue_id: i64,
    pub project_id: ProjectId,
    pub merge_request: MergeRequestId,
    pub iid: i64,
    pub position: i64,
    pub state: EntryState,
    pub head_sha: ObjectId,
    pub method: MergeMethod,
    pub message: String,
    pub enqueued_by: UserId,
    pub credential: CredentialRef,
    pub request_id: RequestId,
    pub isolate: Option<Isolation>,
    pub batch_id: Option<i64>,
    pub train_sha: Option<ObjectId>,
    pub merge_id: Option<i64>,
    pub reaps: i32,
    pub reason: Option<String>,
    pub error_code: Option<String>,
    pub created_at: String,
    pub finished_at: Option<String>,
}

#[derive(sqlx::FromRow)]
pub(crate) struct EntryRow {
    id: i64,
    queue_id: i64,
    project_id: i64,
    merge_request_id: i64,
    iid: i64,
    position: i64,
    state: String,
    head_sha: String,
    method: String,
    message: String,
    enqueued_by: i64,
    credential_kind: String,
    credential_id: i64,
    request_id: String,
    isolate: Option<String>,
    batch_id: Option<i64>,
    train_sha: Option<String>,
    merge_id: Option<i64>,
    reaps: i32,
    reason: Option<String>,
    error_code: Option<String>,
    created_at: String,
    finished_at: Option<String>,
}

/// The columns of [`EntryRow`] (`e` the entry, `m` its merge request).
pub(crate) fn entry_columns() -> String {
    format!(
        "e.id, e.queue_id, e.project_id, e.merge_request_id, m.iid, e.position, e.state, \
         e.head_sha, e.method, e.message, e.enqueued_by, e.credential_kind, e.credential_id, \
         e.request_id, e.isolate, e.batch_id, e.train_sha, e.merge_id, e.reaps, e.reason, \
         e.error_code, to_char(e.created_at AT TIME ZONE 'UTC', {TS}) AS created_at, \
         to_char(e.finished_at AT TIME ZONE 'UTC', {TS}) AS finished_at"
    )
}

impl EntryRow {
    pub(crate) fn parse(self) -> Result<StoredEntry, ForgeError> {
        let id = self.id;
        let bad = |reason| unreadable(reason, id);
        Ok(StoredEntry {
            id,
            queue_id: self.queue_id,
            project_id: ProjectId::new(self.project_id),
            merge_request: MergeRequestId::new(self.merge_request_id),
            iid: self.iid,
            position: self.position,
            state: EntryState::parse(&self.state)
                .ok_or_else(|| bad("stored_entry_state_invalid"))?,
            head_sha: oid(&self.head_sha, id)?,
            method: MergeMethod::parse(&self.method)
                .ok_or_else(|| bad("stored_entry_method_invalid"))?,
            message: self.message,
            enqueued_by: UserId::new(self.enqueued_by),
            credential: match self.credential_kind.as_str() {
                "session" => CredentialRef::Session(self.credential_id),
                "token" => CredentialRef::Token(self.credential_id),
                _ => return Err(bad("stored_entry_credential_invalid")),
            },
            request_id: RequestId::parse(&self.request_id)
                .ok_or_else(|| bad("stored_entry_request_id_invalid"))?,
            isolate: self
                .isolate
                .map(|i| Isolation::parse(&i).ok_or_else(|| bad("stored_entry_isolate_invalid")))
                .transpose()?,
            batch_id: self.batch_id,
            train_sha: self.train_sha.map(|t| oid(&t, id)).transpose()?,
            merge_id: self.merge_id,
            reaps: self.reaps,
            reason: self.reason,
            error_code: self.error_code,
            created_at: self.created_at,
            finished_at: self.finished_at,
        })
    }
}

/// One batch as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBatch {
    pub id: i64,
    pub queue_id: i64,
    pub project_id: ProjectId,
    pub state: BatchState,
    pub base_sha: ObjectId,
    pub train_sha: ObjectId,
    pub size: usize,
    pub request_id: RequestId,
    pub reason: Option<String>,
    pub error_code: Option<String>,
    pub deadline_at: String,
    /// `now() > deadline_at` when it was read.
    pub past_deadline: bool,
    pub created_at: String,
}

#[derive(sqlx::FromRow)]
struct BatchRow {
    id: i64,
    queue_id: i64,
    project_id: i64,
    state: String,
    base_sha: String,
    train_sha: String,
    size: i32,
    request_id: String,
    reason: Option<String>,
    error_code: Option<String>,
    deadline_at: String,
    past_deadline: bool,
    created_at: String,
}

fn batch_columns() -> String {
    format!(
        "id, queue_id, project_id, state, base_sha, train_sha, size, request_id, reason, \
         error_code, to_char(deadline_at AT TIME ZONE 'UTC', {TS}) AS deadline_at, \
         now() > deadline_at AS past_deadline, \
         to_char(created_at AT TIME ZONE 'UTC', {TS}) AS created_at"
    )
}

impl BatchRow {
    fn parse(self) -> Result<StoredBatch, ForgeError> {
        let id = self.id;
        Ok(StoredBatch {
            id,
            queue_id: self.queue_id,
            project_id: ProjectId::new(self.project_id),
            state: BatchState::parse(&self.state)
                .ok_or_else(|| unreadable("stored_batch_state_invalid", id))?,
            base_sha: oid(&self.base_sha, id)?,
            train_sha: oid(&self.train_sha, id)?,
            size: usize::try_from(self.size)
                .map_err(|_| unreadable("stored_batch_size_invalid", id))?,
            request_id: RequestId::parse(&self.request_id)
                .ok_or_else(|| unreadable("stored_batch_request_id_invalid", id))?,
            reason: self.reason,
            error_code: self.error_code,
            deadline_at: self.deadline_at,
            past_deadline: self.past_deadline,
            created_at: self.created_at,
        })
    }
}

// ── a person's commands ──

/// What a pause or resume did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseOutcome {
    /// Written, with its audit row.
    Changed,
    /// The queue was already so: nothing written.
    Unchanged,
}

/// Take merge request `iid` of `project` out of its queue for `viewer`
/// (ADR-0030 §10): `decide` runs on the locked facts (forge-review
/// reconfirms `authorize()`); no live entry is `FG-CI-0025
/// reason=not_queued`; else the entry is `removed` (`reason=dequeued`)
/// with one `merge_queue.dequeued` audit row. An entry being tested leaves
/// its batch: the worker throws the batch away at its next step.
pub async fn dequeue_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    viewer: UserId,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(StoredEntry, P), ForgeError> {
    const ACTION: &str = "merge_queue.dequeued";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let mr = facts.merge_request.as_ref().ok_or_else(absent)?;
        let mr_id = mr.id;
        let live = tx
            .step(|c| async move { super::merges::live_entry_on(c, mr_id).await })
            .await?;
        let Some(entry) = live else {
            return Err(refused("not_queued", mr_id));
        };
        // Review HIGH 1: an entry whose batch is landing (its attempt may
        // already have moved the target) is the landing's, never a person's.
        let landing = match (entry.merge_id, entry.batch_id) {
            (Some(_), _) => true,
            (None, Some(batch)) => tx
                .step(|c| async move {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT state = 'landing' FROM forge.merge_queue_batches                          WHERE id = $1 FOR SHARE",
                    )
                    .bind(batch)
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("merge_queue.dequeue_batch", e))
                })
                .await?
                .unwrap_or(false),
            (None, None) => false,
        };
        if landing {
            return Err(refused("landing", mr_id).with_context("entry_id", entry.id));
        }
        let entry_id = entry.id;
        let queue = entry.queue_id;
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.merge_queue_entries SET state = 'removed', reason = 'dequeued', \
                 removed_by = $2, finished_at = now(), updated_at = now() WHERE id = $1",
            )
            .bind(entry_id)
            .bind(viewer.get())
            .execute(&mut *c)
            .await
            .map_err(|e| query_error("merge_queue.dequeue", e))?;
            sqlx::query("UPDATE forge.merge_queues SET next_run_at = now() WHERE id = $1")
                .bind(queue)
                .execute(c)
                .await
                .map(drop)
                .map_err(|e| query_error("merge_queue.wake", e))
        })
        .await?;
        let removed = StoredEntry {
            state: EntryState::Removed,
            reason: Some("dequeued".to_owned()),
            ..entry
        };
        let change = MergeQueueChange::Dequeued {
            project,
            merge_request: mr_id,
            iid: mr.iid,
            entry: entry_id,
            target_branch: mr.target_branch.as_str().to_owned(),
        };
        Ok(Settle::Commit(queue_record(change), (removed, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Pause (`paused = true`) or resume the queue of `target` in `project`
/// for `caller` (ADR-0030 §9), decided again on the locked project
/// (`decide`). The queue is created when missing (a pause before any
/// entry stops the first one too). One `merge_queue.paused` / `.resumed`
/// audit row; [`PauseOutcome::Unchanged`] writes nothing.
pub async fn set_paused_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, target): (ProjectId, &RefName),
    caller: UserId,
    paused: bool,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(PauseOutcome, StoredQueue, P), ForgeError> {
    let action = if paused {
        "merge_queue.paused"
    } else {
        "merge_queue.resumed"
    };
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock_shared(&mut tx, project).await?;
        let decided = decide(&locked_access_facts(&mut tx, project, caller, &locked).await?)?;
        let name = target.clone(); // clone: the step owns its name
        let queue = tx
            .step(|c| async move { queue_on(c, project, &name).await })
            .await?;
        let changed: Option<QueueRow> = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "UPDATE forge.merge_queues SET paused = $2, \
                     paused_by = CASE WHEN $2 THEN $3 END, \
                     paused_at = CASE WHEN $2 THEN now() END, \
                     next_run_at = now(), updated_at = now() \
                     WHERE id = $1 AND paused <> $2 RETURNING {}",
                    queue_columns()
                ))
                .bind(queue)
                .bind(paused)
                .bind(caller.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("merge_queue.pause", e))
            })
            .await?;
        match changed {
            Some(row) => {
                let stored = row.parse()?;
                let target_branch = target.as_str().to_owned();
                let change = if paused {
                    MergeQueueChange::Paused {
                        project,
                        target_branch,
                    }
                } else {
                    MergeQueueChange::Resumed {
                        project,
                        target_branch,
                    }
                };
                Ok(Settle::Commit(
                    queue_record(change),
                    (PauseOutcome::Changed, stored, decided),
                ))
            }
            None => {
                let row: QueueRow = tx
                    .step(|c| async move {
                        sqlx::query_as(&format!(
                            "SELECT {} FROM forge.merge_queues WHERE id = $1",
                            queue_columns()
                        ))
                        .bind(queue)
                        .fetch_one(c)
                        .await
                        .map_err(|e| query_error("merge_queue.pause_read", e))
                    })
                    .await?;
                Ok(Settle::Discard((
                    PauseOutcome::Unchanged,
                    row.parse()?,
                    decided,
                )))
            }
        }
    }
    .await;
    tx.settle(action, outcome).await
}

// ── reads (read pool) ──

/// A queue with its live entries in order, its live batch and its most
/// recent finished entries (newest first, at most `recent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueView {
    /// `None`: nothing was ever queued for the target (and it was never
    /// paused).
    pub queue: Option<StoredQueue>,
    pub live: Vec<StoredEntry>,
    pub batch: Option<StoredBatch>,
    pub recent: Vec<StoredEntry>,
}

/// The queue of `target` in `project` (module docs).
pub async fn queue_view(
    db: &Db,
    project: ProjectId,
    target: &RefName,
    recent: i64,
) -> Result<QueueView, ForgeError> {
    const QUERY: &str = "merge_queue.view";
    db.within(QUERY, async {
        let pool = db.pool().executor();
        let queue: Option<QueueRow> = sqlx::query_as(&format!(
            "SELECT {} FROM forge.merge_queues WHERE project_id = $1 AND target_branch = $2",
            queue_columns()
        ))
        .bind(project.get())
        .bind(target.as_str())
        .fetch_optional(pool)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let Some(queue) = queue.map(QueueRow::parse).transpose()? else {
            return Ok(QueueView {
                queue: None,
                live: Vec::new(),
                batch: None,
                recent: Vec::new(),
            });
        };
        let live: Vec<EntryRow> = sqlx::query_as(&format!(
            "SELECT {} FROM forge.merge_queue_entries e \
             JOIN forge.merge_requests m ON m.id = e.merge_request_id \
             WHERE e.queue_id = $1 AND e.state IN ('queued', 'testing') ORDER BY e.position",
            entry_columns()
        ))
        .bind(queue.id)
        .fetch_all(pool)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let done: Vec<EntryRow> = sqlx::query_as(&format!(
            "SELECT {} FROM forge.merge_queue_entries e \
             JOIN forge.merge_requests m ON m.id = e.merge_request_id \
             WHERE e.queue_id = $1 AND e.state IN ('merged', 'removed') \
             ORDER BY e.finished_at DESC, e.id DESC LIMIT $2",
            entry_columns()
        ))
        .bind(queue.id)
        .bind(recent)
        .fetch_all(pool)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let batch: Option<BatchRow> = sqlx::query_as(&format!(
            "SELECT {} FROM forge.merge_queue_batches WHERE queue_id = $1 \
             AND state IN ('testing', 'landing')",
            batch_columns()
        ))
        .bind(queue.id)
        .fetch_optional(pool)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(QueueView {
            queue: Some(queue),
            live: live
                .into_iter()
                .map(EntryRow::parse)
                .collect::<Result<_, _>>()?,
            batch: batch.map(BatchRow::parse).transpose()?,
            recent: done
                .into_iter()
                .map(EntryRow::parse)
                .collect::<Result<_, _>>()?,
        })
    })
    .await
}

/// The latest entry of merge request `mr` (the live one when there is
/// one) and its place among the live entries of its queue (1-based;
/// `None` when not live).
pub async fn entry_of(
    db: &Db,
    mr: MergeRequestId,
) -> Result<Option<(StoredEntry, Option<i64>)>, ForgeError> {
    const QUERY: &str = "merge_queue.entry_of";
    db.within(QUERY, async {
        let pool = db.pool().executor();
        let row: Option<EntryRow> = sqlx::query_as(&format!(
            "SELECT {} FROM forge.merge_queue_entries e \
             JOIN forge.merge_requests m ON m.id = e.merge_request_id \
             WHERE e.merge_request_id = $1 \
             ORDER BY (e.state IN ('queued', 'testing')) DESC, e.id DESC LIMIT 1",
            entry_columns()
        ))
        .bind(mr.get())
        .fetch_optional(pool)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let Some(entry) = row.map(EntryRow::parse).transpose()? else {
            return Ok(None);
        };
        if !entry.state.is_live() {
            return Ok(Some((entry, None)));
        }
        let ahead: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM forge.merge_queue_entries WHERE queue_id = $1 \
             AND state IN ('queued', 'testing') AND position < $2",
        )
        .bind(entry.queue_id)
        .bind(entry.position)
        .fetch_one(pool)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(Some((entry, Some(ahead + 1))))
    })
    .await
}

// ── the worker ──

/// A queue the worker holds the lease of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    pub queue: StoredQueue,
    pub worker: String,
}

/// Claim one queue that is due, unleased (or whose lease expired) and has
/// work (a live entry or batch): `FOR UPDATE SKIP LOCKED`, then the lease
/// is written and the row released at once (module docs). `None`: no
/// queue to work now.
pub async fn claim(db: &Db, worker: &str, lease: Duration) -> Result<Option<Claimed>, ForgeError> {
    const QUERY: &str = "merge_queue.claim";
    let row: Option<QueueRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "UPDATE forge.merge_queues SET lease_owner = $1, \
                   lease_until = now() + make_interval(secs => $2), updated_at = now() \
                 WHERE id = (SELECT q.id FROM forge.merge_queues q \
                   WHERE q.next_run_at <= now() \
                     AND (q.lease_until IS NULL OR q.lease_until < now()) \
                     AND (EXISTS (SELECT 1 FROM forge.merge_queue_entries e \
                            WHERE e.queue_id = q.id AND e.state IN ('queued', 'testing')) \
                          OR EXISTS (SELECT 1 FROM forge.merge_queue_batches b \
                            WHERE b.queue_id = q.id AND b.state IN ('testing', 'landing'))) \
                   ORDER BY q.next_run_at, q.id LIMIT 1 FOR UPDATE SKIP LOCKED) \
                 RETURNING {}",
                queue_columns()
            ))
            .bind(worker)
            .bind(lease.as_secs_f64())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(|r| {
        Ok(Claimed {
            queue: r.parse()?,
            worker: worker.to_owned(),
        })
    })
    .transpose()
}

/// Release `claimed`'s lease and ask for the next step after `next_in`.
/// `false`: the lease was no longer this worker's (it expired and another
/// worker took the queue).
pub async fn release(db: &Db, claimed: &Claimed, next_in: Duration) -> Result<bool, ForgeError> {
    const QUERY: &str = "merge_queue.release";
    db.within(QUERY, async {
        let done = sqlx::query(
            "UPDATE forge.merge_queues SET lease_owner = NULL, lease_until = NULL, \
               next_run_at = now() + make_interval(secs => $3), updated_at = now() \
             WHERE id = $1 AND lease_owner = $2",
        )
        .bind(claimed.queue.id)
        .bind(&claimed.worker)
        .bind(next_in.as_secs_f64())
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(done.rows_affected() == 1)
    })
    .await
}

/// The queue row as it is now (the worker reads `paused` again before it
/// lands).
pub async fn queue_now(db: &Db, queue: i64) -> Result<Option<StoredQueue>, ForgeError> {
    const QUERY: &str = "merge_queue.queue_now";
    let row: Option<QueueRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_queues WHERE id = $1",
                queue_columns()
            ))
            .bind(queue)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(QueueRow::parse).transpose()
}

/// The live batch of `queue` (write pool).
pub async fn live_batch(db: &Db, queue: i64) -> Result<Option<StoredBatch>, ForgeError> {
    const QUERY: &str = "merge_queue.live_batch";
    let row: Option<BatchRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_queue_batches WHERE queue_id = $1 \
                 AND state IN ('testing', 'landing')",
                batch_columns()
            ))
            .bind(queue)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(BatchRow::parse).transpose()
}

/// Batch `id` (write pool; tests and diagnosis).
pub async fn batch(db: &Db, id: i64) -> Result<Option<StoredBatch>, ForgeError> {
    const QUERY: &str = "merge_queue.batch";
    let row: Option<BatchRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_queue_batches WHERE id = $1",
                batch_columns()
            ))
            .bind(id)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(BatchRow::parse).transpose()
}

/// Every entry of `queue` in `states`, in queue order (write pool).
pub async fn entries(
    db: &Db,
    queue: i64,
    states: &[EntryState],
) -> Result<Vec<StoredEntry>, ForgeError> {
    const QUERY: &str = "merge_queue.entries";
    let states: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
    let rows: Vec<EntryRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_queue_entries e \
                 JOIN forge.merge_requests m ON m.id = e.merge_request_id \
                 WHERE e.queue_id = $1 AND e.state = ANY($2) ORDER BY e.position",
                entry_columns()
            ))
            .bind(queue)
            .bind(&states)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(EntryRow::parse).collect()
}

/// The entries of batch `batch` (every state), in queue order.
pub async fn batch_entries(db: &Db, batch: i64) -> Result<Vec<StoredEntry>, ForgeError> {
    const QUERY: &str = "merge_queue.batch_entries";
    let rows: Vec<EntryRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_queue_entries e \
                 JOIN forge.merge_requests m ON m.id = e.merge_request_id \
                 WHERE e.batch_id = $1 ORDER BY e.position",
                entry_columns()
            ))
            .bind(batch)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(EntryRow::parse).collect()
}

/// Remove a queued entry for a reason the worker found before batching it
/// (`head_changed`, `merge_request_not_open`, `conflict`, …) with its
/// code. `false`: it was not queued any more.
pub async fn remove_queued(
    db: &Db,
    entry: i64,
    reason: &str,
    code: ErrorCode,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "merge_queue.remove_queued";
    db.within(QUERY, async {
        let done = sqlx::query(
            "UPDATE forge.merge_queue_entries SET state = 'removed', reason = $2, \
             error_code = $3, finished_at = now(), updated_at = now() \
             WHERE id = $1 AND state = 'queued'",
        )
        .bind(entry)
        .bind(reason)
        .bind(code.id())
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(done.rows_affected() == 1)
    })
    .await
}

/// One entry of a new batch: its commit in the train and why it runs
/// alone (if it does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchMember {
    pub entry: i64,
    pub train_sha: ObjectId,
    pub isolate: Option<Isolation>,
}

/// A batch to write.
#[derive(Debug, Clone)]
pub struct NewBatch {
    pub queue: i64,
    pub project: ProjectId,
    pub base: ObjectId,
    pub train: ObjectId,
    pub members: Vec<BatchMember>,
    pub request_id: RequestId,
    pub deadline: Duration,
    /// The train's `ci.pipeline_create` job (deduplicated on its sha).
    pub pipeline: NewJob,
}

/// Write `new` (state `testing`), move its entries to `testing` and
/// enqueue the train's pipeline, in one transaction. `None`: the queue has
/// a live batch already, or a member is no longer `queued` (dequeued
/// meanwhile) — nothing written, the worker builds again.
pub async fn create_batch(db: &Db, new: &NewBatch) -> Result<Option<i64>, ForgeError> {
    const QUERY: &str = "merge_queue.create_batch";
    if new.members.is_empty() || new.members.len() > MAX_BATCH {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "merge_queue_batch_size")
            .with_context("size", new.members.len()));
    }
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let _locked: i64 =
            sqlx::query_scalar("SELECT id FROM forge.merge_queues WHERE id = $1 FOR UPDATE")
                .bind(new.queue)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| query_error(QUERY, e))?;
        let ids: Vec<i64> = new.members.iter().map(|m| m.entry).collect();
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM forge.merge_queue_entries WHERE id = ANY($1) \
             AND queue_id = $2 AND state = 'queued'",
        )
        .bind(&ids)
        .bind(new.queue)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        if usize::try_from(queued).ok() != Some(ids.len()) {
            return Ok(None);
        }
        let size = i32::try_from(new.members.len()).unwrap_or(i32::MAX);
        let inserted: Option<i64> = sqlx::query_scalar(
            "INSERT INTO forge.merge_queue_batches (queue_id, project_id, state, base_sha, \
             train_sha, size, request_id, deadline_at) \
             SELECT $1, $2, 'testing', $3, $4, $5, $6, now() + make_interval(secs => $7) \
             WHERE NOT EXISTS (SELECT 1 FROM forge.merge_queue_batches \
               WHERE queue_id = $1 AND state IN ('testing', 'landing')) \
             RETURNING id",
        )
        .bind(new.queue)
        .bind(new.project.get())
        .bind(new.base.as_str())
        .bind(new.train.as_str())
        .bind(size)
        .bind(new.request_id.as_str())
        .bind(new.deadline.as_secs_f64())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some(batch) = inserted else {
            return Ok(None);
        };
        for m in &new.members {
            sqlx::query(
                "UPDATE forge.merge_queue_entries SET state = 'testing', batch_id = $2, \
                 train_sha = $3, isolate = $4, updated_at = now() \
                 WHERE id = $1 AND state = 'queued'",
            )
            .bind(m.entry)
            .bind(batch)
            .bind(m.train_sha.as_str())
            .bind(m.isolate.map(Isolation::as_str))
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
        }
        enqueue_on(&mut tx, &new.pipeline).await?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Some(batch))
    })
    .await
}

/// The train pipeline of a batch as the verdict reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrainPipeline {
    /// No pipeline row yet and its create job is still queued / running.
    Planning,
    /// The create job finished without a row (no `.forge-ci.yml`), or no
    /// job is left.
    None,
    /// The newest pipeline on the train.
    Found {
        id: i64,
        status: crate::pipelines::PipelineStatus,
    },
}

/// The pipeline of the train `sha` in `project` (`source = merge_queue`).
pub async fn train_pipeline(
    db: &Db,
    project: ProjectId,
    sha: &ObjectId,
) -> Result<TrainPipeline, ForgeError> {
    const QUERY: &str = "merge_queue.train_pipeline";
    let (found, planning): (Option<(i64, String)>, bool) = db
        .within(QUERY, async {
            let found: Option<(i64, String)> = sqlx::query_as(
                "SELECT id, status FROM forge.ci_pipelines WHERE project_id = $1 \
                 AND source = 'merge_queue' AND sha = $2 ORDER BY id DESC LIMIT 1",
            )
            .bind(project.get())
            .bind(sha.as_str())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))?;
            let planning: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM forge.jobs WHERE kind = $1 \
                 AND state IN ('queued', 'running') \
                 AND payload ->> 'source' = 'merge_queue' AND payload ->> 'sha' = $2 \
                 AND (payload ->> 'project_id')::bigint = $3)",
            )
            .bind(crate::pipelines::PIPELINE_CREATE_JOB)
            .bind(sha.as_str())
            .bind(project.get())
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))?;
            Ok((found, planning))
        })
        .await?;
    match found {
        Some((id, status)) => Ok(TrainPipeline::Found {
            id,
            status: crate::pipelines::PipelineStatus::parse(&status)
                .ok_or_else(|| unreadable("stored_pipeline_status_invalid", id))?,
        }),
        None if planning => Ok(TrainPipeline::Planning),
        None => Ok(TrainPipeline::None),
    }
}

/// How a batch ends without landing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchEnd {
    /// The state the batch must be in (review HIGH 2: only the poll ends a
    /// `testing` batch; only the landing and the recovery, under the merge
    /// lock, end a `landing` one).
    pub from: BatchState,
    /// `failed` or `abandoned`.
    pub state: BatchState,
    pub reason: &'static str,
    pub error_code: Option<ErrorCode>,
    /// Entries removed for good, with their reason and code.
    pub removed: Vec<(i64, String, ErrorCode)>,
    /// The batch passed its deadline: every other entry gets one more reap,
    /// and an entry at [`forge_core::merge_queue::MAX_REAPS`] is removed
    /// (`FG-CI-0024 reason=stalled`).
    pub reaped: bool,
    /// The queue's next batch bound (`None`: unchanged).
    pub batch_limit: Option<usize>,
}

/// End live batch `batch` as `end` says, from `end.from` only: its other
/// `testing` entries go back to `queued` in their order (their merge
/// attempt, if any, is the caller's to end first). `false`: the batch was
/// not in `end.from` any more (another worker moved it).
pub async fn end_batch(db: &Db, batch: i64, end: &BatchEnd) -> Result<bool, ForgeError> {
    const QUERY: &str = "merge_queue.end_batch";
    if !matches!(end.state, BatchState::Failed | BatchState::Abandoned)
        || !end.from.is_live()
    {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "merge_queue_batch_end_not_final")
            .with_context("state", end.state.as_str()));
    }
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let queue: Option<i64> = sqlx::query_scalar(
            "UPDATE forge.merge_queue_batches SET state = $2, reason = $3, error_code = $4, \
             finished_at = now(), updated_at = now() \
             WHERE id = $1 AND state = $5 RETURNING queue_id",
        )
        .bind(batch)
        .bind(end.state.as_str())
        .bind(end.reason)
        .bind(end.error_code.map(|c| c.id()))
        .bind(end.from.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some(queue) = queue else {
            return Ok(false);
        };
        for (entry, reason, code) in &end.removed {
            sqlx::query(
                "UPDATE forge.merge_queue_entries SET state = 'removed', reason = $2, \
                 error_code = $3, finished_at = now(), updated_at = now() \
                 WHERE id = $1 AND state = 'testing'",
            )
            .bind(entry)
            .bind(reason)
            .bind(code.id())
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
        }
        if end.reaped {
            sqlx::query(
                "UPDATE forge.merge_queue_entries SET state = 'removed', reason = 'stalled', \
                 error_code = $2, reaps = $3, batch_id = NULL, train_sha = NULL, \
                 merge_id = NULL, finished_at = now(), updated_at = now() \
                 WHERE batch_id = $1 AND state = 'testing' AND reaps + 1 >= $3",
            )
            .bind(batch)
            .bind(ErrorCode::MergeQueueStalled.id())
            .bind(forge_core::merge_queue::MAX_REAPS)
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
        }
        sqlx::query(
            "UPDATE forge.merge_queue_entries SET state = 'queued', batch_id = NULL, \
             train_sha = NULL, merge_id = NULL, reaps = reaps + $2, updated_at = now() \
             WHERE batch_id = $1 AND state = 'testing'",
        )
        .bind(batch)
        .bind(i32::from(end.reaped))
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        if let Some(limit) = end.batch_limit {
            sqlx::query("UPDATE forge.merge_queues SET batch_limit = $2 WHERE id = $1")
                .bind(queue)
                .bind(i32::try_from(limit.clamp(1, MAX_BATCH)).unwrap_or(1))
                .execute(&mut *tx)
                .await
                .map_err(|e| query_error(QUERY, e))?;
        }
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(true)
    })
    .await
}

/// `testing → landing` (the landing starts under the merge lock). `false`:
/// it was not `testing`.
pub async fn begin_landing(db: &Db, batch: i64) -> Result<bool, ForgeError> {
    const QUERY: &str = "merge_queue.begin_landing";
    db.within(QUERY, async {
        let done = sqlx::query(
            "UPDATE forge.merge_queue_batches SET state = 'landing', updated_at = now() \
             WHERE id = $1 AND state = 'testing'",
        )
        .bind(batch)
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(done.rows_affected() == 1)
    })
    .await
}

/// Entry `entry` landed (its attempt is `applied`).
pub async fn entry_merged(db: &Db, entry: i64) -> Result<bool, ForgeError> {
    const QUERY: &str = "merge_queue.entry_merged";
    db.within(QUERY, async {
        let done = sqlx::query(
            "UPDATE forge.merge_queue_entries SET state = 'merged', finished_at = now(), \
             updated_at = now() WHERE id = $1 AND state = 'testing' AND merge_id IS NOT NULL",
        )
        .bind(entry)
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(done.rows_affected() == 1)
    })
    .await
}

/// `landing → landed`, and the queue's bound back to [`MAX_BATCH`].
pub async fn batch_landed(db: &Db, batch: i64) -> Result<bool, ForgeError> {
    const QUERY: &str = "merge_queue.batch_landed";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let queue: Option<i64> = sqlx::query_scalar(
            "UPDATE forge.merge_queue_batches SET state = 'landed', finished_at = now(), \
             updated_at = now() WHERE id = $1 AND state = 'landing' RETURNING queue_id",
        )
        .bind(batch)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some(queue) = queue else {
            return Ok(false);
        };
        sqlx::query("UPDATE forge.merge_queues SET batch_limit = $2 WHERE id = $1")
            .bind(queue)
            .bind(i32::try_from(MAX_BATCH).unwrap_or(4))
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(true)
    })
    .await
}

/// What doctor's `review.merge_queue` reads: live batches past their
/// deadline and queues whose lease expired more than `lease_grace` ago
/// (the worker is not running), ids newest first (at most 10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueHealth {
    pub stalled_batches: i64,
    pub stalled_ids: Vec<i64>,
    pub orphaned_queues: i64,
    pub orphaned_ids: Vec<i64>,
}

/// [`QueueHealth`] on the read pool.
pub async fn health(db: &Db, lease_grace: Duration) -> Result<QueueHealth, ForgeError> {
    const QUERY: &str = "merge_queue.health";
    let row: (i64, Vec<i64>, i64, Vec<i64>) = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT \
                   (SELECT count(*) FROM forge.merge_queue_batches \
                     WHERE state IN ('testing', 'landing') AND deadline_at < now()), \
                   ARRAY(SELECT id FROM forge.merge_queue_batches \
                     WHERE state IN ('testing', 'landing') AND deadline_at < now() \
                     ORDER BY id DESC LIMIT 10), \
                   (SELECT count(*) FROM forge.merge_queues \
                     WHERE lease_until < now() - make_interval(secs => $1)), \
                   ARRAY(SELECT id FROM forge.merge_queues \
                     WHERE lease_until < now() - make_interval(secs => $1) \
                     ORDER BY id DESC LIMIT 10)",
            )
            .bind(lease_grace.as_secs_f64())
            .fetch_one(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(QueueHealth {
        stalled_batches: row.0,
        stalled_ids: row.1,
        orphaned_queues: row.2,
        orphaned_ids: row.3,
    })
}

/// Among `queues`, those with a live batch (`testing` / `landing`): the
/// maintenance sweep keeps their train ref and deletes every other
/// `refs/merge-queue/<id>/train` (review MEDIUM, ADR-0030 §3). Read pool.
pub async fn queues_with_live_batch(db: &Db, queues: &[i64]) -> Result<Vec<i64>, ForgeError> {
    const QUERY: &str = "merge_queue.live_queues";
    if queues.is_empty() {
        return Ok(Vec::new());
    }
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT DISTINCT queue_id FROM forge.merge_queue_batches              WHERE queue_id = ANY($1) AND state IN ('testing', 'landing') ORDER BY queue_id",
        )
        .bind(queues)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}
