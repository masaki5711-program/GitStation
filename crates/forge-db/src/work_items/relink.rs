//! Detecting the work items of existing merge requests again after a
//! binding (ADR-0016 Amendment 2026-11-12): the `work_items.relink` job.
//!
//! Unbinding ends the project's open links (`unbound`); a new binding that
//! resolves work items — bind, re-bind to the same or another kanflow
//! project, or a shared binding becoming the resolving one — used to leave
//! the existing merge requests unlinked until their next title change or
//! push. The binding command now enqueues this job in its own transaction
//! (`ChangeTx::enqueue_job`, deduplicated by `work_items_relink:<binding>`),
//! and the job asks the ordered subscriber `work-item-sync` — the only
//! writer of detected links (trigger `merge_request_work_items_audited`,
//! constraint `merge_request_work_items_derived`) — to detect each merge
//! request again:
//!
//! - the **open** merge requests (at most [`MAX_RELINK_OPEN`], oldest
//!   first) and the merge requests **merged** in the last
//!   [`RELINK_MERGED_DAYS`] days (at most [`MAX_RELINK_MERGED`], newest
//!   first; merged at = the applied merge attempt's end, else the row's
//!   `updated_at`), so kanflow's merge history works after a re-bind.
//!   Closed merge requests are not detected again. A cut list is logged at
//!   warn (`forge.work_items.relink_capped`), never silently.
//! - one `merge_request.relink_requested` event (admin class: internal)
//!   per merge request, written in changes of at most [`RELINK_BATCH`]
//!   events with one audit row each (`work_item.relink_requested` on the
//!   project, actor `system`). Each event is its own root, so one merge
//!   request's links stay within `MAX_DERIVED_PER_ROOT`.
//! - `work-item-sync` handles each like any `merge_request.*` event (the
//!   same rules and caps; a work item a person unlinked stays unlinked) and
//!   publishes one `merge_request.work_items_changed` (`reason=relink`)
//!   when the links changed.
//!
//! At least once: an event already written for (merge request, binding)
//! is skipped (`merge_request_relink_requested:<mr>:<binding>`), so a
//! retried job writes only what is missing. A binding that is gone or no
//! longer resolves work items when the job runs ends it without writing
//! (`binding_gone`).
//!
//! Tests: `feed::tests::relink` (with the feed and the development status).

use std::time::Duration;

use forge_core::ids::ProjectId;
use forge_core::{DedupKey, DedupPart, EventType, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Committed, DomainEvent, Outcome,
};
use crate::jobs::{self, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::{query_error, Db};

/// The kind of the job.
pub const RELINK_JOB: &str = "work_items.relink";
/// The most open merge requests detected again per binding.
pub const MAX_RELINK_OPEN: i64 = 500;
/// The most merged merge requests detected again per binding.
pub const MAX_RELINK_MERGED: i64 = 500;
/// Merged merge requests younger than this are detected again.
pub const RELINK_MERGED_DAYS: i32 = 90;
/// Events per change (`change::MAX_EVENTS`).
pub const RELINK_BATCH: usize = 100;
/// The job's budget: up to ten changes of 100 events.
pub const RELINK_TIMEOUT: Duration = Duration::from_secs(300);

const ACTION: &str = "work_item.relink_requested";
const EVENT_DEDUP: &str = "merge_request_relink_requested";

/// The job's payload: the project and the binding (its `generation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelinkRequest {
    pub project: ProjectId,
    pub binding: i64,
}

/// How much one run selects (the constants; smaller in tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelinkLimits {
    pub open: i64,
    pub merged: i64,
    pub batch: usize,
}

impl Default for RelinkLimits {
    fn default() -> Self {
        Self {
            open: MAX_RELINK_OPEN,
            merged: MAX_RELINK_MERGED,
            batch: RELINK_BATCH,
        }
    }
}

/// What one run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Relinked {
    /// The binding was gone or no longer resolves work items: nothing written.
    pub binding_gone: bool,
    /// Merge requests selected (open, merged).
    pub open: usize,
    pub merged: usize,
    /// More were there than selected.
    pub open_capped: bool,
    pub merged_capped: bool,
    /// `merge_request.relink_requested` events written by this run.
    pub requested: usize,
}

fn invalid_job(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", RELINK_JOB)
}

/// The job detecting the merge requests of `request` again.
pub fn relink_job(
    request: &RelinkRequest,
    origin_request_id: RequestId,
) -> Result<NewJob, ForgeError> {
    let key = format!("work_items_relink:{}", request.binding);
    Ok(NewJob {
        kind: JobKind::parse(RELINK_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({
            "project_id": request.project.get(),
            "binding_id": request.binding,
        }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(RELINK_TIMEOUT).ok_or_else(|| invalid_job("job_timeout"))?,
        dedup_key: Some(jobs::DedupKey::parse(&key).ok_or_else(|| invalid_job("dedup_key"))?),
        origin_request_id,
        origin_event_seq: None,
    })
}

/// The payload of a claimed job (`FG-SYS-0001 reason=invalid_job_payload`
/// for anything else: forge wrote it).
pub fn relink_payload(payload: &serde_json::Value) -> Result<RelinkRequest, ForgeError> {
    let positive = |field: &str| {
        payload
            .get(field)
            .and_then(serde_json::Value::as_i64)
            .filter(|id| *id > 0)
            .ok_or_else(|| invalid_job("invalid_job_payload"))
    };
    Ok(RelinkRequest {
        project: ProjectId::new(positive("project_id")?),
        binding: positive("binding_id")?,
    })
}

/// (id, iid) of the merge requests to detect again: open ones oldest
/// first, then merged ones newest first; each list one longer than its
/// limit when cut.
const CANDIDATES_SQL: &str = "(SELECT m.id, m.iid, 'open' AS state FROM forge.merge_requests AS m \
     WHERE m.project_id = $1 AND m.state = 'open' ORDER BY m.id LIMIT $2) \
     UNION ALL \
     (SELECT m.id, m.iid, 'merged' FROM forge.merge_requests AS m \
     WHERE m.project_id = $1 AND m.state = 'merged' \
     AND COALESCE((SELECT max(g.finished_at) FROM forge.merge_request_merges AS g \
       WHERE g.merge_request_id = m.id AND g.state = 'applied'), m.updated_at) \
       >= pg_catalog.now() - pg_catalog.make_interval(days => $4) \
     ORDER BY m.id DESC LIMIT $3)";

const BINDING_SQL: &str = "SELECT EXISTS (SELECT 1 FROM forge.integration_bindings \
     WHERE generation = $1 AND project_id = $2 AND resolves_work_items)";

/// Run the job for `request` with the default limits.
pub async fn request_relink(
    db: &Db,
    ctx: &ChangeContext,
    request: &RelinkRequest,
) -> Result<Relinked, ForgeError> {
    request_relink_with(db, ctx, request, RelinkLimits::default()).await
}

/// [`request_relink`] with `limits` (tests use small ones).
pub async fn request_relink_with(
    db: &Db,
    ctx: &ChangeContext,
    request: &RelinkRequest,
    limits: RelinkLimits,
) -> Result<Relinked, ForgeError> {
    let project = request.project;
    let (bound, rows) = db
        .within("work_items.relink.candidates", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("work_items.relink.acquire", e))?;
            let bound: bool = sqlx::query_scalar(BINDING_SQL)
                .bind(request.binding)
                .bind(project.get())
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| query_error("work_items.relink.binding", e))?;
            if !bound {
                return Ok((false, Vec::new()));
            }
            let rows: Vec<(i64, i64, String)> = sqlx::query_as(CANDIDATES_SQL)
                .bind(project.get())
                .bind(limits.open.saturating_add(1))
                .bind(limits.merged.saturating_add(1))
                .bind(RELINK_MERGED_DAYS)
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| query_error("work_items.relink.candidates", e))?;
            Ok((true, rows))
        })
        .await?;
    if !bound {
        tracing::info!(
            target: "forge_db",
            event = "forge.work_items.relink_skipped",
            request_id = %ctx.request_id,
            project_id = project.get(),
            binding_id = request.binding,
            reason = "binding_gone",
            "the binding is gone or no longer resolves work items; nothing to detect"
        );
        return Ok(Relinked {
            binding_gone: true,
            ..Relinked::default()
        });
    }
    let mut out = Relinked::default();
    let cap = |n: i64| usize::try_from(n).unwrap_or(usize::MAX);
    let of_state = |state: &str, limit: i64| -> (Vec<(i64, i64)>, bool) {
        let all: Vec<(i64, i64)> = rows
            .iter()
            .filter(|r| r.2 == state)
            .map(|r| (r.0, r.1))
            .collect();
        let capped = all.len() > cap(limit);
        (all.into_iter().take(cap(limit)).collect(), capped)
    };
    let (open, open_capped) = of_state("open", limits.open);
    let (merged, merged_capped) = of_state("merged", limits.merged);
    (out.open, out.open_capped) = (open.len(), open_capped);
    (out.merged, out.merged_capped) = (merged.len(), merged_capped);
    let chosen: Vec<(i64, i64)> = open.into_iter().chain(merged).collect();
    if out.open_capped || out.merged_capped {
        tracing::warn!(
            target: "forge_db",
            event = "forge.work_items.relink_capped",
            request_id = %ctx.request_id,
            project_id = project.get(),
            binding_id = request.binding,
            open = out.open,
            open_capped = out.open_capped,
            merged = out.merged,
            merged_capped = out.merged_capped,
            "a project has more merge requests than are detected again after a binding; the rest keep their links until their next change"
        );
    }
    for batch in chosen.chunks(limits.batch.clamp(1, RELINK_BATCH)) {
        out.requested += request_batch(db, ctx, request, batch).await?;
    }
    tracing::info!(
        target: "forge_db",
        event = "forge.work_items.relink_requested",
        request_id = %ctx.request_id,
        project_id = project.get(),
        binding_id = request.binding,
        open = out.open,
        merged = out.merged,
        requested = out.requested,
        "merge requests are detected again after a binding"
    );
    Ok(out)
}

fn event_key(merge_request: i64, binding: i64) -> Result<DedupKey, ForgeError> {
    DedupKey::new(
        EVENT_DEDUP,
        &[DedupPart::Id(merge_request), DedupPart::Id(binding)],
    )
}

/// One change: the batch's merge requests that have no event for this
/// binding yet, one event each and one audit row. A concurrent run of the
/// same job (a lease taken over) makes the change `Deduped`: read again
/// once and write what is still missing.
async fn request_batch(
    db: &Db,
    ctx: &ChangeContext,
    request: &RelinkRequest,
    batch: &[(i64, i64)],
) -> Result<usize, ForgeError> {
    let project = request.project;
    let mut keys: Vec<String> = Vec::with_capacity(batch.len());
    for (id, _) in batch {
        keys.push(event_key(*id, request.binding)?.as_str().to_owned());
    }
    for _ in 0..2 {
        let mut tx = ChangeTx::begin(db, ctx).await?;
        let keys = keys.clone(); // clone: each attempt binds its own copy
                                 // `None`: the binding went away meanwhile (nothing more to ask).
        let written: Result<Option<Vec<String>>, ForgeError> = tx
            .step(|c| async move {
                // The project first, as every writer of its links.
                sqlx::query("SELECT 1 FROM forge.projects WHERE id = $1 FOR SHARE")
                    .bind(project.get())
                    .execute(&mut *c)
                    .await
                    .map_err(|e| query_error("work_items.relink.project_lock", e))?;
                let bound: bool = sqlx::query_scalar(BINDING_SQL)
                    .bind(request.binding)
                    .bind(project.get())
                    .fetch_one(&mut *c)
                    .await
                    .map_err(|e| query_error("work_items.relink.binding", e))?;
                if !bound {
                    return Ok(None);
                }
                sqlx::query_scalar("SELECT dedup_key FROM forge.events WHERE dedup_key = ANY($1)")
                    .bind(&keys)
                    .fetch_all(&mut *c)
                    .await
                    .map(Some)
                    .map_err(|e| query_error("work_items.relink.written", e))
            })
            .await;
        let written = match written {
            Ok(Some(w)) => w,
            Ok(None) => {
                tx.rollback(ACTION).await;
                return Ok(0);
            }
            Err(e) => {
                tx.rollback(ACTION).await;
                return Err(e);
            }
        };
        let mut events = Vec::new();
        for (id, iid) in batch {
            let key = event_key(*id, request.binding)?;
            if written.iter().any(|w| w == key.as_str()) {
                continue;
            }
            events.push(DomainEvent {
                event_type: EventType::MergeRequestRelinkRequested,
                schema_version: 1,
                aggregate_type: "merge_request",
                aggregate_id: *id,
                project_id: Some(project),
                payload: serde_json::json!({
                    "merge_request_id": id,
                    "iid": iid,
                    "binding_id": request.binding,
                }),
                dedup_key: Some(key),
            });
        }
        if events.is_empty() {
            tx.rollback(ACTION).await;
            return Ok(0);
        }
        let count = events.len();
        let record = ChangeRecord {
            audit: AuditEntry {
                action: ACTION,
                outcome: Outcome::Succeeded,
                resource_type: "project",
                resource_id: Some(project.get()),
                project_id: Some(project),
                error_code: None,
                details: serde_json::json!({
                    "binding_id": request.binding,
                    "merge_requests": count,
                }),
            },
            affected: Vec::new(),
            events,
        };
        match tx.commit(record).await? {
            Committed::Recorded { .. } => return Ok(count),
            Committed::Deduped => continue,
        }
    }
    Err(ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "relink_deduped_twice")
        .with_context("project_id", project.get())
        .with_context("binding_id", request.binding))
}
