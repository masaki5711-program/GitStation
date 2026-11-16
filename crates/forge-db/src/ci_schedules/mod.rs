//! Pipeline schedules (M4 C12, plan 14 §5 C12; migration 20261219100000):
//! the typed commands and reads of `forge.ci_pipeline_schedules`.
//!
//! - [`create_schedule_decided`] / [`update_schedule_decided`] / [`delete_schedule_decided`]: a
//!   Maintainer's change, decided again on the locked project and caller
//!   and audited on the project in the same transaction
//!   (`pipeline_schedule.created` / `.updated` / `.deleted`). Whoever saves
//!   a schedule becomes its **owner**: it runs with their rights.
//! - [`claim_due`] (the worker): the due schedules, `SKIP LOCKED`; each gets
//!   its next time from its rule (the database's clock) and one
//!   `ci.schedule_fire` job, deduplicated per (schedule, due time), in one
//!   transaction — a due time is fired at most once.
//! - [`fire_schedule_decided`] (the job): decide `PipelineCreate` for the owner on
//!   the locked project and account, write the pipeline (`source =
//!   schedule`, never protected) and the schedule's last run, with one
//!   `pipeline.scheduled` audit row (actor `system`).
//! - [`record_outcome`] (the job): a due run that started nothing (no
//!   definition, no branch, the owner refused, a failure) on the
//!   schedule's unaudited columns.

use forge_core::ci_schedule::Rule;
use forge_core::pipeline::PipelineChange;
use forge_core::{AuditDetails, ProjectId, RefName, RequestId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::jobs::{enqueue_on, DedupKey, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::pipelines::{decided_facts, insert_pipeline, NewPipeline, Source};
use crate::projects::ProjectUpdateFacts;
use crate::{query_error, read_query_error, Db};

/// The audit resource of a schedule's change (tables.toml).
pub const RESOURCE: &str = "project";
/// The most schedules one project may have (`FG-CI-0027`).
pub const MAX_PER_PROJECT: i64 = 20;
/// The kind of the job that runs one due time of one schedule.
pub const SCHEDULE_FIRE_JOB: &str = "ci.schedule_fire";
/// One run: the project and branch lookups, the planning (as
/// `ci.pipeline_create`) and one write.
pub const SCHEDULE_FIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
/// The most due schedules one claim takes.
pub const CLAIM_BATCH: i64 = 20;
/// At most this many scheduled pipelines per project in an hour (M4 C12
/// review): a due run past it starts nothing (`capped`, FG-CI-0028).
pub const MAX_PROJECT_PER_HOUR: i64 = 12;
/// At most this many scheduled pipelines on the instance in an hour.
pub const MAX_GLOBAL_PER_HOUR: i64 = 120;
/// When a stored rule has no next time (never for a rule that passed
/// [`Rule::parse`] and fired once), it is looked at again after a day.
const FALLBACK_SECS: i64 = 86_400;

const TS: &str = "YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"";

/// What a person saves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleInput {
    pub description: String,
    /// `refs/heads/…`.
    pub git_ref: RefName,
    /// Checked with [`Rule::parse`] by the caller.
    pub cron: String,
    pub utc_offset_minutes: i32,
    pub active: bool,
}

/// What its last due run did (`last_outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastOutcome {
    /// A pipeline was started (`last_pipeline_id`).
    Created,
    /// No `.forge-ci.yml` at the branch's tip.
    NoDefinition,
    /// The branch does not exist.
    RefMissing,
    /// The owner may not run pipelines now (`FG-CI-0026`).
    Refused,
    /// Planning failed for good (`last_error_code`).
    Failed,
    /// The previous scheduled pipeline is still pending or running: this
    /// due time started nothing (M4 C12 review).
    Busy,
    /// The hourly cap of scheduled pipelines was reached (`FG-CI-0028`).
    Capped,
}

impl LastOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::NoDefinition => "no_definition",
            Self::RefMissing => "ref_missing",
            Self::Refused => "refused",
            Self::Failed => "failed",
            Self::Busy => "busy",
            Self::Capped => "capped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "created" => Some(Self::Created),
            "no_definition" => Some(Self::NoDefinition),
            "ref_missing" => Some(Self::RefMissing),
            "refused" => Some(Self::Refused),
            "failed" => Some(Self::Failed),
            "busy" => Some(Self::Busy),
            "capped" => Some(Self::Capped),
            _ => None,
        }
    }
}

/// A schedule as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSchedule {
    pub id: i64,
    pub project: ProjectId,
    pub description: String,
    pub git_ref: String,
    pub cron: String,
    pub utc_offset_minutes: i32,
    pub active: bool,
    pub owner: UserId,
    pub created_by: UserId,
    /// RFC 3339 (UTC); `None` when inactive.
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    /// Unix seconds of `last_run_at` (the due time last handled).
    pub last_run_epoch: Option<i64>,
    pub last_pipeline_id: Option<i64>,
    pub last_outcome: Option<LastOutcome>,
    pub last_error_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, sqlx::FromRow)]
struct ScheduleRow {
    id: i64,
    project_id: i64,
    description: String,
    git_ref: String,
    cron: String,
    utc_offset_minutes: i32,
    active: bool,
    owner_id: i64,
    created_by: i64,
    next_run_at: Option<String>,
    last_run_at: Option<String>,
    last_run_epoch: Option<i64>,
    last_pipeline_id: Option<i64>,
    last_outcome: Option<String>,
    last_error_code: Option<String>,
    created_at: String,
    updated_at: String,
}

fn columns() -> String {
    format!(
        "id, project_id, description, ref AS git_ref, cron, utc_offset_minutes, active, \
         owner_id, created_by, \
         to_char(next_run_at AT TIME ZONE 'UTC', '{TS}') AS next_run_at, \
         to_char(last_run_at AT TIME ZONE 'UTC', '{TS}') AS last_run_at, \
         extract(epoch FROM last_run_at)::bigint AS last_run_epoch, \
         last_pipeline_id, last_outcome, last_error_code, \
         to_char(created_at AT TIME ZONE 'UTC', '{TS}') AS created_at, \
         to_char(updated_at AT TIME ZONE 'UTC', '{TS}') AS updated_at"
    )
}

fn schedule_of(r: ScheduleRow) -> Result<StoredSchedule, ForgeError> {
    let last_outcome = match r.last_outcome.as_deref() {
        None => None,
        Some(s) => Some(LastOutcome::parse(s).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "stored_outcome_invalid")
                .with_context("schedule_id", r.id)
        })?),
    };
    Ok(StoredSchedule {
        id: r.id,
        project: ProjectId::new(r.project_id),
        description: r.description,
        git_ref: r.git_ref,
        cron: r.cron,
        utc_offset_minutes: r.utc_offset_minutes,
        active: r.active,
        owner: UserId::new(r.owner_id),
        created_by: UserId::new(r.created_by),
        next_run_at: r.next_run_at,
        last_run_at: r.last_run_at,
        last_run_epoch: r.last_run_epoch,
        last_pipeline_id: r.last_pipeline_id,
        last_outcome,
        last_error_code: r.last_error_code,
        created_at: r.created_at,
        updated_at: r.updated_at,
    })
}

fn record(change: PipelineChange, project: ProjectId) -> ChangeRecord {
    let details = AuditDetails::Pipeline(change);
    ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

fn not_found(schedule: i64) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "pipeline_schedule")
        .with_context("reason", "absent")
        .with_context("schedule_id", schedule)
}

/// The next time of `cron` after `now` (Unix seconds), or the fallback.
fn next_time(cron: &str, offset: i32, now: i64) -> i64 {
    Rule::parse(cron)
        .ok()
        .and_then(|r| r.next_after(now, offset))
        .unwrap_or(now + FALLBACK_SECS)
}

/// The database's clock, as Unix seconds.
async fn db_now(c: &mut sqlx::PgConnection, query: &'static str) -> Result<i64, ForgeError> {
    sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
        .fetch_one(c)
        .await
        .map_err(|e| query_error(query, e))
}

/// The schedules of `project`, by id (the read pool).
pub async fn list(db: &Db, project: ProjectId) -> Result<Vec<StoredSchedule>, ForgeError> {
    const QUERY: &str = "ci_schedules.list";
    let rows: Vec<ScheduleRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ci_pipeline_schedules WHERE project_id = $1 \
                 ORDER BY id LIMIT $2",
                columns()
            ))
            .bind(project.get())
            .bind(MAX_PER_PROJECT)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(schedule_of).collect()
}

/// One schedule of `project` (the read pool); `None` without it.
pub async fn get(
    db: &Db,
    project: ProjectId,
    schedule: i64,
) -> Result<Option<StoredSchedule>, ForgeError> {
    const QUERY: &str = "ci_schedules.get";
    let row: Option<ScheduleRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ci_pipeline_schedules WHERE project_id = $1 AND id = $2",
                columns()
            ))
            .bind(project.get())
            .bind(schedule)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(schedule_of).transpose()
}

/// One schedule by id, on the write pool (the fire job reads what it is
/// about to write against); `None` once deleted.
pub async fn by_id(db: &Db, schedule: i64) -> Result<Option<StoredSchedule>, ForgeError> {
    const QUERY: &str = "ci_schedules.by_id";
    let row: Option<ScheduleRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ci_pipeline_schedules WHERE id = $1",
                columns()
            ))
            .bind(schedule)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(schedule_of).transpose()
}

/// Create a schedule of `project` owned by `caller`, decided again on the
/// locked project and caller (`decide`), with one
/// `pipeline_schedule.created` row. Returns its id. Errors (nothing
/// written): `decide`'s, `FG-CI-0027` (the project has
/// [`MAX_PER_PROJECT`]), `FG-DB-*`.
pub async fn create_schedule_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    input: &ScheduleInput,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(i64, P), ForgeError> {
    const QUERY: &str = "ci_schedules.create";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let id = tx
            .step(|c| async move {
                let count: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM forge.ci_pipeline_schedules WHERE project_id = $1",
                )
                .bind(project.get())
                .fetch_one(&mut *c)
                .await
                .map_err(|e| query_error(QUERY, e))?;
                if count >= MAX_PER_PROJECT {
                    return Err(ForgeError::new(ErrorCode::ScheduleLimitReached)
                        .with_context("project_id", project.get())
                        .with_context("schedules", count)
                        .with_context("max", MAX_PER_PROJECT));
                }
                let now = db_now(&mut *c, QUERY).await?;
                let next = input
                    .active
                    .then(|| next_time(&input.cron, input.utc_offset_minutes, now));
                sqlx::query_scalar(
                    "INSERT INTO forge.ci_pipeline_schedules (project_id, description, ref, \
                       cron, utc_offset_minutes, active, owner_id, created_by, next_run_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $7, to_timestamp($8)) RETURNING id",
                )
                .bind(project.get())
                .bind(&input.description)
                .bind(input.git_ref.as_str())
                .bind(&input.cron)
                .bind(input.utc_offset_minutes)
                .bind(input.active)
                .bind(caller.get())
                .bind(next)
                .fetch_one(&mut *c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let change = saved(project, id, true, input, caller);
        Ok(Settle::Commit(record(change, project), (id, decided)))
    }
    .await;
    tx.settle("pipeline_schedule.created", outcome).await
}

fn saved(
    project: ProjectId,
    schedule: i64,
    created: bool,
    input: &ScheduleInput,
    owner: UserId,
) -> PipelineChange {
    PipelineChange::ScheduleSaved {
        project,
        schedule,
        created,
        git_ref: input.git_ref.as_str().to_owned(),
        cron: input.cron.clone(), // clone: the audit row owns it
        utc_offset_minutes: input.utc_offset_minutes,
        active: input.active,
        owner: owner.get(),
    }
}

/// Replace a schedule's settings and compute its next time again. `caller`
/// becomes its owner when the branch, the rule or the offset changes, or
/// when the schedule was stopped because its owner was refused; otherwise
/// (a description or pause/resume edit) the owner stays (M4 C12 review). One `pipeline_schedule.updated` row. Errors
/// (nothing written): `decide`'s, `FG-REPO-0005
/// resource=pipeline_schedule`, `FG-DB-*`.
pub async fn update_schedule_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, schedule): (ProjectId, i64),
    caller: UserId,
    input: &ScheduleInput,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    const QUERY: &str = "ci_schedules.update";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let owner: Option<i64> = tx
            .step(|c| async move {
                let now = db_now(&mut *c, QUERY).await?;
                let next = input
                    .active
                    .then(|| next_time(&input.cron, input.utc_offset_minutes, now));
                // M4 C12 review: the caller becomes the owner only when what
                // runs or when changes (branch, rule, offset) or when taking
                // over a schedule its owner may no longer run; a description
                // or pause/resume edit keeps the owner.
                sqlx::query_scalar(
                    "UPDATE forge.ci_pipeline_schedules SET description = $3, ref = $4, \
                       cron = $5, utc_offset_minutes = $6, active = $7, \
                       owner_id = CASE WHEN ref IS DISTINCT FROM $4 OR cron IS DISTINCT FROM $5 \
                         OR utc_offset_minutes IS DISTINCT FROM $6 \
                         OR last_outcome IS NOT DISTINCT FROM 'refused' \
                         THEN $8 ELSE owner_id END, \
                       next_run_at = to_timestamp($9), updated_at = now() \
                     WHERE project_id = $1 AND id = $2 RETURNING owner_id",
                )
                .bind(project.get())
                .bind(schedule)
                .bind(&input.description)
                .bind(input.git_ref.as_str())
                .bind(&input.cron)
                .bind(input.utc_offset_minutes)
                .bind(input.active)
                .bind(caller.get())
                .bind(next)
                .fetch_optional(&mut *c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let owner = UserId::new(owner.ok_or_else(|| not_found(schedule))?);
        let change = saved(project, schedule, false, input, owner);
        Ok(Settle::Commit(record(change, project), decided))
    }
    .await;
    tx.settle("pipeline_schedule.updated", outcome).await
}

/// Delete a schedule (`pipeline_schedule.deleted`). Errors as
/// [`update_schedule_decided`].
pub async fn delete_schedule_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, schedule): (ProjectId, i64),
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    const QUERY: &str = "ci_schedules.delete";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let deleted = tx
            .step(|c| async move {
                sqlx::query(
                    "DELETE FROM forge.ci_pipeline_schedules WHERE project_id = $1 AND id = $2",
                )
                .bind(project.get())
                .bind(schedule)
                .execute(c)
                .await
                .map_err(|e| query_error(QUERY, e))
                .map(|r| r.rows_affected())
            })
            .await?;
        if deleted == 0 {
            return Err(not_found(schedule));
        }
        let change = PipelineChange::ScheduleDeleted { project, schedule };
        Ok(Settle::Commit(record(change, project), decided))
    }
    .await;
    tx.settle("pipeline_schedule.deleted", outcome).await
}

/// One due time handed to a `ci.schedule_fire` job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FireRequest {
    pub schedule: i64,
    pub project: ProjectId,
    /// The due time (Unix seconds): the run's identity.
    pub due: i64,
}

fn invalid_job(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", SCHEDULE_FIRE_JOB)
}

/// The job of one due time (dedup `ci_schedule:<id>:<due>`).
pub fn fire_job(request: &FireRequest, origin: RequestId) -> Result<NewJob, ForgeError> {
    let key = format!("ci_schedule:{}:{}", request.schedule, request.due);
    Ok(NewJob {
        kind: JobKind::parse(SCHEDULE_FIRE_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({
            "schedule_id": request.schedule,
            "project_id": request.project.get(),
            "due": request.due,
        }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(SCHEDULE_FIRE_TIMEOUT)
            .ok_or_else(|| invalid_job("job_timeout"))?,
        dedup_key: Some(DedupKey::parse(&key).ok_or_else(|| invalid_job("dedup_key"))?),
        origin_request_id: origin,
        origin_event_seq: None,
    })
}

/// The payload of a claimed `ci.schedule_fire` job (`FG-SYS-0001
/// reason=invalid_job_payload` otherwise: forge wrote it).
pub fn fire_payload(payload: &serde_json::Value) -> Result<FireRequest, ForgeError> {
    let field = |k: &str| {
        payload
            .get(k)
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| invalid_job("invalid_job_payload"))
    };
    let (schedule, project, due) = (field("schedule_id")?, field("project_id")?, field("due")?);
    if schedule <= 0 || project <= 0 {
        return Err(invalid_job("invalid_job_payload"));
    }
    Ok(FireRequest {
        schedule,
        project: ProjectId::new(project),
        due,
    })
}

/// Claim the due schedules (worker): at most [`CLAIM_BATCH`], `SKIP
/// LOCKED`, in one transaction each gets its next time (from the database's
/// `now()`, so missed times while forge was down are run once, not each)
/// and one `ci.schedule_fire` job for the due time it had. Returns what
/// was enqueued (an already enqueued due time is not repeated).
pub async fn claim_due(db: &Db, origin: &RequestId) -> Result<Vec<FireRequest>, ForgeError> {
    const QUERY: &str = "ci_schedules.claim_due";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let due: Vec<(i64, i64, String, i32, i64, i64)> = sqlx::query_as(
            "SELECT id, project_id, cron, utc_offset_minutes, \
               extract(epoch FROM next_run_at)::bigint, extract(epoch FROM now())::bigint \
             FROM forge.ci_pipeline_schedules WHERE active AND next_run_at <= now() \
             ORDER BY next_run_at LIMIT $1 FOR UPDATE SKIP LOCKED",
        )
        .bind(CLAIM_BATCH)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let mut fired = Vec::with_capacity(due.len());
        for (id, project, cron, offset, at, now) in due {
            let next = next_time(&cron, offset, now);
            sqlx::query(
                "UPDATE forge.ci_pipeline_schedules SET next_run_at = to_timestamp($2) \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(next)
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            let request = FireRequest {
                schedule: id,
                project: ProjectId::new(project),
                due: at,
            };
            // Deduplicated: a due time already enqueued is not repeated.
            let _enqueued = enqueue_on(&mut tx, &fire_job(&request, origin.clone())?).await?; // clone: each job owns its origin
            fired.push(request);
        }
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(fired)
    })
    .await
}

/// A due run that started nothing: its outcome on the schedule's unaudited
/// columns (`last_run_at` = the due time). A schedule deleted meanwhile is
/// left alone.
pub async fn record_outcome(
    db: &Db,
    request: &FireRequest,
    outcome: LastOutcome,
    error_code: Option<ErrorCode>,
) -> Result<(), ForgeError> {
    const QUERY: &str = "ci_schedules.record_outcome";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.ci_pipeline_schedules SET last_run_at = to_timestamp($2), \
               last_outcome = $3, last_error_code = $4, \
               last_pipeline_id = CASE WHEN $3 IN ('busy', 'capped') THEN last_pipeline_id END \
             WHERE id = $1",
        )
        .bind(request.schedule)
        .bind(request.due)
        .bind(outcome.as_str())
        .bind(error_code.map(|c| c.id()))
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))
        .map(drop)
    })
    .await
}

/// What [`fire_schedule_decided`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fired {
    /// The pipeline written.
    Created(i64),
    /// The schedule is gone, inactive, owned by another person or ran this
    /// due time already: nothing written.
    Skipped(&'static str),
    /// Its previous scheduled pipeline is still pending or running: nothing
    /// written (the caller records `busy`).
    Busy,
    /// The hourly cap (`project` or `instance`) with the count found:
    /// nothing written (the caller records `capped`, FG-CI-0028).
    Capped(&'static str, i64),
}

/// Start the pipeline of one due time: lock the project and the owner's
/// account (`decide`: forge-pipelines decides `PipelineCreate` for the
/// owner as a person), lock the schedule (still active, still owned by
/// `owner`, this due time not handled yet), write `new` and the
/// schedule's last run, with one `pipeline.scheduled` row (`ctx`: actor
/// `system`). Errors (nothing written): `decide`'s, `FG-DB-*`.
pub async fn fire_schedule_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    request: &FireRequest,
    owner: UserId,
    new: &NewPipeline,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(Fired, Option<P>), ForgeError> {
    const QUERY: &str = "ci_schedules.fire";
    if new.request.source != Source::Schedule || new.protected {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "not_a_scheduled_run")
            .with_context("source", new.request.source.as_str()));
    }
    let project = request.project;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = decided_facts(&mut tx, project, owner).await?;
        let schedule = request.schedule;
        let state: Option<(bool, i64, Option<i64>)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT active, owner_id, extract(epoch FROM last_run_at)::bigint \
                     FROM forge.ci_pipeline_schedules WHERE project_id = $1 AND id = $2 \
                     FOR UPDATE",
                )
                .bind(project.get())
                .bind(schedule)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let skip = match state {
            None => Some("schedule_gone"),
            Some((false, _, _)) => Some("inactive"),
            Some((_, o, _)) if o != owner.get() => Some("owner_changed"),
            Some((_, _, Some(last))) if last >= request.due => Some("already_ran"),
            Some(_) => None,
        };
        if let Some(reason) = skip {
            return Ok(Settle::Discard((Fired::Skipped(reason), None)));
        }
        // M4 C12 review: one scheduled pipeline at a time per schedule, and
        // an hourly cap per project and on the instance.
        let (busy, project_hour, global_hour): (bool, i64, i64) = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT EXISTS (SELECT 1 FROM forge.ci_pipeline_schedules s \
                       JOIN forge.ci_pipelines p ON p.id = s.last_pipeline_id \
                       WHERE s.id = $1 AND p.status IN ('pending', 'running')), \
                     (SELECT count(*) FROM forge.ci_pipelines WHERE source = 'schedule' \
                       AND project_id = $2 AND created_at > now() - interval '1 hour'), \
                     (SELECT count(*) FROM forge.ci_pipelines WHERE source = 'schedule' \
                       AND created_at > now() - interval '1 hour')",
                )
                .bind(schedule)
                .bind(project.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let limited = if busy {
            Some(Fired::Busy)
        } else if project_hour >= MAX_PROJECT_PER_HOUR {
            Some(Fired::Capped("project", project_hour))
        } else if global_hour >= MAX_GLOBAL_PER_HOUR {
            Some(Fired::Capped("instance", global_hour))
        } else {
            None
        };
        if let Some(fired) = limited {
            return Ok(Settle::Discard((fired, None)));
        }
        let decided = decide(&facts)?;
        let pipeline = tx
            .step(|c| async move {
                let id = insert_pipeline(&mut *c, new, QUERY).await?.ok_or_else(|| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "scheduled_run_deduplicated")
                })?;
                sqlx::query(
                    "UPDATE forge.ci_pipeline_schedules SET last_run_at = to_timestamp($2), \
                       last_pipeline_id = $3, last_outcome = 'created', last_error_code = NULL \
                     WHERE id = $1",
                )
                .bind(schedule)
                .bind(request.due)
                .bind(id)
                .execute(&mut *c)
                .await
                .map_err(|e| query_error(QUERY, e))?;
                Ok(id)
            })
            .await?;
        let change = PipelineChange::Scheduled {
            project,
            pipeline,
            schedule,
            owner: owner.get(),
            git_ref: new.request.git_ref.as_str().to_owned(),
            sha: new.request.sha.as_str().to_owned(),
            status: crate::pipelines::planned_status(new).as_str().to_owned(),
        };
        Ok(Settle::Commit(
            record(change, project),
            (Fired::Created(pipeline), Some(decided)),
        ))
    }
    .await;
    tx.settle("pipeline.scheduled", outcome).await
}

/// Stop a schedule whose owner was refused at a due run (M4 C12 review):
/// `active = false`, no next time, the run's outcome `refused` with
/// `FG-CI-0026`, and one `pipeline_schedule.deactivated` row (`ctx`: actor
/// `system`) with the refusal's `cause_code`. `false`: the schedule was
/// deleted, paused or taken over meanwhile (nothing written).
pub async fn deactivate_refused(
    db: &Db,
    ctx: &ChangeContext,
    request: &FireRequest,
    owner: UserId,
    cause_code: &str,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "ci_schedules.deactivate_refused";
    let project = request.project;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let schedule = request.schedule;
        let due = request.due;
        let changed = tx
            .step(|c| async move {
                sqlx::query(
                    "UPDATE forge.ci_pipeline_schedules SET active = false, next_run_at = NULL, \
                       last_run_at = to_timestamp($4), last_outcome = 'refused', \
                       last_error_code = $5, last_pipeline_id = NULL, updated_at = now() \
                     WHERE project_id = $1 AND id = $2 AND owner_id = $3 AND active",
                )
                .bind(project.get())
                .bind(schedule)
                .bind(owner.get())
                .bind(due)
                .bind(ErrorCode::ScheduleOwnerNotAllowed.id())
                .execute(c)
                .await
                .map_err(|e| query_error(QUERY, e))
                .map(|r| r.rows_affected())
            })
            .await?;
        if changed == 0 {
            return Ok(Settle::Discard(false));
        }
        let change = PipelineChange::ScheduleDeactivated {
            project,
            schedule,
            owner: owner.get(),
            cause_code: cause_code.to_owned(),
        };
        Ok(Settle::Commit(record(change, project), true))
    }
    .await;
    tx.settle("pipeline_schedule.deactivated", outcome).await
}

#[cfg(test)]
mod tests;
