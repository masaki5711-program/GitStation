//! The `check.spec_index` job and the `spec-index` subscriber (M3 X4,
//! ADR-0023 Amendment X4): `forge/spec-lint` and `forge/spec-trace` of a
//! merge request version's head. The job is run by `forge worker` through
//! forge-checks (`spec_checks::SpecChecks`), which writes both runs with
//! [`super::record_forge_check_run`].
//!
//! When a job is asked for:
//! - `merge_request.pushed` (every version, the first of an open included):
//!   the version's head;
//! - `work_item.linked` / `work_item.unlinked` (spec-trace covers the linked
//!   work items) and a retarget (`merge_request.updated` with
//!   `target_branch`: the merge base moves): the merge request's **current**
//!   head. A merge request without a version yet is passed over: its first
//!   `merge_request.pushed` evaluates it with the state of that time.
//!
//! The dedup key is `spec_index:<project>:<iid>:<head>:<state>`, `state`
//! the md5 of the target branch and the open links' work item ids (review
//! X4, MEDIUM d): what the runs depend on besides the commits. It is held by
//! a queued, running or succeeded job, so the same head in the same state
//! is evaluated once, and the same head in another state (a link changed,
//! another target) again. **Coalesced** (review X4, MEDIUM e): while a job
//! of the same (project, merge request, head) is still queued nothing more
//! is asked — it reads the links and target when it runs — so a flood of
//! link changes is one evaluation.
//!
//! A dead job does not hold its key (ADR-0016 §7); its head's runs say
//! `error` (the handler's last attempt, or the worker's sweep:
//! [`unanswered_dead_jobs`]).

use forge_core::{EventType, ObjectId, ProjectId, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use crate::jobs::{self, enqueue_on, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};
use crate::{query_error, read_query_error, Db};

/// The kind of the job.
pub const SPEC_INDEX_JOB: &str = "check.spec_index";
/// The subscriber's name (`forge.event_subscribers`).
pub const SPEC_INDEX_SUBSCRIBER: &str = "spec-index";
/// One evaluation: two indexes (the head and the merge base: a tree
/// listing, the spec files, one `git grep` each), the merge request and its
/// links, and two writes. forge-checks ends its own evaluation well inside
/// it (its budget plus two bounded writes).
pub const SPEC_INDEX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The job's payload: the merge request and the commit to evaluate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecIndexRequest {
    pub project: ProjectId,
    /// The merge request's number in its project.
    pub iid: i64,
    pub head: ObjectId,
}

fn invalid_job(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", SPEC_INDEX_JOB)
}

/// The job evaluating `request` in `state` (the md5 of the target and the
/// open links: module docs; `None` for a job asked for by hand).
pub fn spec_index_job(
    request: &SpecIndexRequest,
    state: Option<&str>,
    origin_request_id: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let mut key = format!(
        "spec_index:{}:{}:{}",
        request.project.get(),
        request.iid,
        request.head.as_str()
    );
    if let Some(state) = state {
        key.push(':');
        key.push_str(state);
    }
    Ok(NewJob {
        kind: JobKind::parse(SPEC_INDEX_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({
            "project_id": request.project.get(),
            "iid": request.iid,
            "head_sha": request.head.as_str(),
        }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(SPEC_INDEX_TIMEOUT).ok_or_else(|| invalid_job("job_timeout"))?,
        dedup_key: Some(jobs::DedupKey::parse(&key).ok_or_else(|| invalid_job("dedup_key"))?),
        origin_request_id,
        origin_event_seq,
    })
}

/// The payload of a claimed job (`FG-SYS-0001 reason=invalid_job_payload`
/// for anything else: forge wrote it).
pub fn spec_index_payload(payload: &serde_json::Value) -> Result<SpecIndexRequest, ForgeError> {
    let positive = |field: &str| {
        payload
            .get(field)
            .and_then(serde_json::Value::as_i64)
            .filter(|n| *n > 0)
            .ok_or_else(|| invalid_job("invalid_job_payload"))
    };
    let project = positive("project_id")?;
    let iid = positive("iid")?;
    let head = payload
        .get("head_sha")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| ObjectId::parse(raw).ok())
        .ok_or_else(|| invalid_job("invalid_job_payload"))?;
    Ok(SpecIndexRequest {
        project: ProjectId::new(project),
        iid,
        head,
    })
}

/// A dead `check.spec_index` job whose head lacks one of its two runs (the
/// reaper ended it — a crashed worker, the runner's own timeout — so its
/// handler never answered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnansweredJob {
    pub job_id: i64,
    pub request: SpecIndexRequest,
    /// The job's request (the answer and its logs carry it).
    pub origin_request_id: String,
    /// The code the job died with.
    pub last_error_code: String,
}

/// How far back the sweep looks for dead jobs.
pub const UNANSWERED_WINDOW_DAYS: i32 = 7;

/// At most `limit` [`UnansweredJob`]s, the oldest first: dead within
/// [`UNANSWERED_WINDOW_DAYS`], of a project that still exists, whose head
/// has no `forge/spec-lint` or no `forge/spec-trace` run. The partial index
/// `jobs_dead_kind` (X2) serves the lookup. A payload this build cannot
/// read is passed over (logged at warn: forge wrote it).
pub async fn unanswered_dead_jobs(db: &Db, limit: i64) -> Result<Vec<UnansweredJob>, ForgeError> {
    const QUERY: &str = "checks.spec_index_unanswered";
    let names: Vec<&str> = vec![
        forge_core::checks::SPEC_LINT_CHECK,
        forge_core::checks::SPEC_TRACE_CHECK,
    ];
    let rows: Vec<(i64, serde_json::Value, String, Option<String>)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT j.id, j.payload, j.origin_request_id, j.last_error_code \
                 FROM forge.jobs j JOIN forge.projects p ON p.id = CASE \
                 WHEN j.payload->>'project_id' ~ '^[0-9]{1,18}$' \
                 THEN (j.payload->>'project_id')::bigint END \
                 WHERE j.kind = $1 AND j.state = 'dead' \
                 AND j.finished_at > now() - make_interval(days => $2) \
                 AND (SELECT count(*) FROM forge.check_runs r WHERE r.project_id = p.id \
                 AND r.head_sha = j.payload->>'head_sha' AND r.name = ANY($3)) < 2 \
                 ORDER BY j.finished_at, j.id LIMIT $4",
            )
            .bind(SPEC_INDEX_JOB)
            .bind(UNANSWERED_WINDOW_DAYS)
            .bind(&names)
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (job_id, payload, origin_request_id, code) in rows {
        match spec_index_payload(&payload) {
            Ok(request) => out.push(UnansweredJob {
                job_id,
                request,
                origin_request_id,
                // `jobs_dead_has_error`: a dead job always has one.
                last_error_code: code.unwrap_or_else(|| ErrorCode::JobDeadLettered.id().to_owned()),
            }),
            Err(e) => tracing::warn!(
                target: "forge_db",
                event = "forge.spec_index.unanswered_unreadable",
                error_code = e.code().id(),
                job_id,
                "a dead spec index job's payload cannot be read; passed over"
            ),
        }
    }
    Ok(out)
}

/// The `spec-index` subscriber (`independent`: an event it cannot read is
/// quarantined and counted, never retried forever).
#[derive(Debug, Default, Clone, Copy)]
pub struct SpecIndexRequests;

fn bad_payload(event: &SourceEvent, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_event_payload")
        .with_context("event_type", event.event_type.clone()) // clone: the context owns it
        .with_context("field", field)
        .with_context("seq", event.seq)
}

fn iid_of(event: &SourceEvent) -> Result<i64, ForgeError> {
    event
        .payload
        .get("iid")
        .and_then(serde_json::Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(|| bad_payload(event, "iid"))
}

/// The merge request's current head (`None`: no version yet) and state
/// digest (module docs), read in the subscriber's transaction. `None`: the
/// merge request is gone.
async fn current(
    tx: &mut DerivedTx<'_>,
    project: ProjectId,
    iid: i64,
) -> Result<Option<(Option<String>, String)>, ForgeError> {
    sqlx::query_as(
        "SELECT m.head_sha, pg_catalog.md5(m.target_branch || '|' || coalesce((\
         SELECT pg_catalog.string_agg(l.work_item_id::text, ',' ORDER BY l.work_item_id) \
         FROM forge.merge_request_work_items l \
         WHERE l.merge_request_id = m.id AND l.unlinked_at IS NULL), '')) \
         FROM forge.merge_requests m WHERE m.project_id = $1 AND m.iid = $2",
    )
    .bind(project.get())
    .bind(iid)
    .fetch_optional(tx.conn())
    .await
    .map_err(|e| query_error("checks.spec_index_state", e))
}

impl SpecIndexRequests {
    /// Ask for `head` of `iid` in its current state, unless a job of that
    /// head is still queued (module docs).
    async fn ask(
        event: &SourceEvent,
        tx: &mut DerivedTx<'_>,
        (project, iid): (ProjectId, i64),
        head: Option<ObjectId>,
    ) -> Result<(), ForgeError> {
        let Some((current_head, state)) = current(tx, project, iid).await? else {
            // The merge request is gone: nothing to evaluate.
            return Ok(());
        };
        let Some(head) = head.or_else(|| current_head.and_then(|h| ObjectId::parse(&h).ok()))
        else {
            // No version yet: its first version is evaluated then.
            return Ok(());
        };
        let queued: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.jobs WHERE kind = $1 AND state = 'queued' \
             AND payload->>'project_id' = $2 AND payload->>'iid' = $3 \
             AND payload->>'head_sha' = $4)",
        )
        .bind(SPEC_INDEX_JOB)
        .bind(project.get().to_string())
        .bind(iid.to_string())
        .bind(head.as_str())
        .fetch_one(tx.conn())
        .await
        .map_err(|e| query_error("checks.spec_index_queued", e))?;
        if queued {
            return Ok(());
        }
        let origin = event.request_id().unwrap_or_else(RequestId::generate);
        let job = spec_index_job(
            &SpecIndexRequest { project, iid, head },
            Some(&state),
            origin,
            Some(event.seq),
        )?;
        // Deduped: a retried step, or this head in this state evaluated before.
        let _enqueued = enqueue_on(tx.conn(), &job).await?;
        Ok(())
    }

    async fn on_version(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        let project = event
            .project_id
            .ok_or_else(|| bad_payload(event, "project_id"))?;
        let iid = iid_of(event)?;
        let head = event
            .payload
            .get("head_sha")
            .and_then(serde_json::Value::as_str)
            .and_then(|raw| ObjectId::parse(raw).ok())
            .ok_or_else(|| bad_payload(event, "head_sha"))?;
        Self::ask(event, tx, (project, iid), Some(head)).await
    }

    /// A link changed, or the merge request was retargeted: its current
    /// head again.
    async fn on_change(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        let project = event
            .project_id
            .ok_or_else(|| bad_payload(event, "project_id"))?;
        let iid = iid_of(event)?;
        Self::ask(event, tx, (project, iid), None).await
    }

    /// `merge_request.updated`: only a retarget moves the merge base.
    async fn on_update(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        let retargeted = event
            .payload
            .get("target_branch")
            .is_some_and(|t| !t.is_null());
        if !retargeted {
            return Ok(());
        }
        Self::on_change(event, tx).await
    }
}

impl Subscriber for SpecIndexRequests {
    fn name(&self) -> &'static str {
        SPEC_INDEX_SUBSCRIBER
    }

    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            let Some(kind) = EventType::parse(&event.event_type) else {
                // A type this build does not know (a newer writer).
                tracing::warn!(
                    target: "forge_db",
                    event = "forge.subscribers.unknown_event_type",
                    subscriber = SPEC_INDEX_SUBSCRIBER,
                    request_id = %event.request_id,
                    source_seq = event.seq,
                    event_type = %event.event_type,
                    "an event type this build does not know was passed over"
                );
                return Ok(());
            };
            match kind {
                EventType::MergeRequestPushed => Self::on_version(event, tx).await,
                EventType::WorkItemLinked | EventType::WorkItemUnlinked => {
                    Self::on_change(event, tx).await
                }
                // Review X4 (HIGH-1): a retarget moves the merge base.
                EventType::MergeRequestUpdated => Self::on_update(event, tx).await,
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
                | EventType::WebhookDisabled => Ok(()),
            }
        })
    }
}
