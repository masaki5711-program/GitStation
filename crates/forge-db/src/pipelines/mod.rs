//! CI pipelines and their jobs (M4 C2, plan 14 §4.1, migration
//! 20261204100000): the typed commands and reads of `forge.ci_pipelines`
//! and `forge.ci_jobs`, the `ci.pipeline_create` job's payload, and the
//! `ci-pipelines` subscriber ([`subscriber`]).
//!
//! A pipeline is planned by forge-pipelines (it reads `.forge-ci.yml` at the
//! commit with git, lints it with the project's protected branches and
//! plans it with forge-ci) and written here in **one transaction** with its
//! jobs ([`create`]). A push or a merge request version creates at most one
//! pipeline per (project, source, ref, sha, merge request): the unique
//! index `ci_pipelines_once` makes a redelivered event or a retried job a
//! no-op ([`Created::existing`]). Manual runs are never deduplicated.
//!
//! Cancel and retry ([`cancel`], [`retry`]) take the pipeline row's lock
//! and refuse a state they cannot change (`FG-CI-0003`), so two people
//! pressing at once get one change and one conflict.

use forge_core::pipeline::PipelineChange;
use forge_core::{AuditDetails, MergeRequestId, ObjectId, ProjectId, RefName, RequestId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use sha2::{Digest, Sha256};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::jobs::{DedupKey, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::{query_error, read_query_error, Db};

pub mod subscriber;

/// The kind of the job that plans and writes one pipeline.
pub const PIPELINE_CREATE_JOB: &str = "ci.pipeline_create";
/// One planning: the git reads (the browse timeout, 20 s each), the
/// confidential mark (20 s), the protected rules and one write.
pub const PIPELINE_CREATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
/// At most this many problems are stored on an `invalid` pipeline (the
/// count is in `problem_count`).
pub const MAX_STORED_PROBLEMS: usize = 20;
/// The most pipelines a list returns at once.
pub const MAX_PAGE: i64 = 100;

/// What started a pipeline (`ci_pipelines.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Push,
    MergeRequest,
    Schedule,
    Manual,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::MergeRequest => "merge_request",
            Self::Schedule => "schedule",
            Self::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "push" => Some(Self::Push),
            "merge_request" => Some(Self::MergeRequest),
            "schedule" => Some(Self::Schedule),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// The confidential marks at the pipeline's commit, measured when it was
/// created (ADR-0029 decision 5; `ci_pipelines.copy_mark`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMark {
    /// No confidential file at the commit.
    Open,
    /// A confidential file at the commit.
    Confidential,
    /// The measurement did not complete (fail closed for agents, C5).
    Undecided,
}

impl CopyMark {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Confidential => "confidential",
            Self::Undecided => "undecided",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "confidential" => Some(Self::Confidential),
            "undecided" => Some(Self::Undecided),
            _ => None,
        }
    }
}

/// A pipeline's state (`ci_pipelines.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Canceled,
    /// The definition selects no job for this trigger.
    Skipped,
    /// The definition (or the plan) is invalid: `FG-CI-0001`.
    Invalid,
    /// The definition could not be read: `FG-CI-0002`.
    Error,
}

impl PipelineStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Skipped => "skipped",
            Self::Invalid => "invalid",
            Self::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "canceled" => Some(Self::Canceled),
            "skipped" => Some(Self::Skipped),
            "invalid" => Some(Self::Invalid),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// A job's state (`ci_jobs.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Waits for its `needs`.
    Created,
    /// Ready for a runner (M4 C3).
    Pending,
    Running,
    Succeeded,
    Failed,
    Canceled,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "created" => Some(Self::Created),
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "canceled" => Some(Self::Canceled),
            _ => None,
        }
    }
}

/// What to plan: one commit of one branch, and why (the job's payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineRequest {
    pub project: ProjectId,
    /// `refs/heads/…`.
    pub git_ref: RefName,
    pub sha: ObjectId,
    pub source: Source,
    /// `Some` exactly for [`Source::MergeRequest`].
    pub merge_request: Option<MergeRequestId>,
    pub triggered_by: Option<UserId>,
}

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", PIPELINE_CREATE_JOB)
}

/// The dedup key of the job (and of the pipeline): the ref is hashed so the
/// key stays under [`DedupKey::MAX_LEN`] for any branch name.
pub fn create_dedup_key(request: &PipelineRequest) -> String {
    let digest = Sha256::digest(request.git_ref.as_str().as_bytes());
    let ref_hash: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!(
        "ci_pipeline:{}:{}:{}:{}:{ref_hash}",
        request.project.get(),
        request.source.as_str(),
        request.merge_request.map_or(0, MergeRequestId::get),
        request.sha.as_str(),
    )
}

/// The `ci.pipeline_create` job for `request` (push and merge request: the
/// dedup key makes a redelivered event enqueue nothing new).
pub fn create_job(
    request: &PipelineRequest,
    origin_request_id: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let key = create_dedup_key(request);
    Ok(NewJob {
        kind: JobKind::parse(PIPELINE_CREATE_JOB).ok_or_else(|| internal("invalid_job_kind"))?,
        payload: serde_json::json!({
            "project_id": request.project.get(),
            "ref": request.git_ref.as_str(),
            "sha": request.sha.as_str(),
            "source": request.source.as_str(),
            "merge_request_id": request.merge_request.map(MergeRequestId::get),
            "triggered_by": request.triggered_by.map(UserId::get),
        }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(PIPELINE_CREATE_TIMEOUT).ok_or_else(|| internal("job_timeout"))?,
        dedup_key: Some(DedupKey::parse(&key).ok_or_else(|| internal("dedup_key"))?),
        origin_request_id,
        origin_event_seq,
    })
}

/// The payload of a claimed job (`FG-SYS-0001 reason=invalid_job_payload`
/// for anything else: forge wrote it).
pub fn create_payload(payload: &serde_json::Value) -> Result<PipelineRequest, ForgeError> {
    let bad = || internal("invalid_job_payload");
    let project = payload
        .get("project_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(bad)?;
    let git_ref = payload
        .get("ref")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| RefName::parse(raw).ok())
        .filter(|r| r.as_str().starts_with("refs/heads/"))
        .ok_or_else(bad)?;
    let sha = payload
        .get("sha")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| ObjectId::parse(raw).ok())
        .ok_or_else(bad)?;
    let source = payload
        .get("source")
        .and_then(serde_json::Value::as_str)
        .and_then(Source::parse)
        .ok_or_else(bad)?;
    let positive = |key: &str| -> Result<Option<i64>, ForgeError> {
        match payload.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(v) => v.as_i64().filter(|id| *id > 0).map(Some).ok_or_else(bad),
        }
    };
    let merge_request = positive("merge_request_id")?.map(MergeRequestId::new);
    if (source == Source::MergeRequest) != merge_request.is_some() {
        return Err(bad());
    }
    Ok(PipelineRequest {
        project: ProjectId::new(project),
        git_ref,
        sha,
        source,
        merge_request,
        triggered_by: positive("triggered_by")?.map(UserId::new),
    })
}

/// One problem of an invalid definition, as stored and shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredProblem {
    pub line: i64,
    pub reason: String,
    pub subject: Option<String>,
}

/// One planned job to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCiJob {
    pub name: String,
    pub stage: String,
    pub wave: i32,
    pub needs: Vec<String>,
    pub image: String,
    pub timeout_s: i32,
    pub tags: Vec<String>,
    pub secrets: Vec<String>,
}

/// What the planning found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Planned {
    /// The jobs to run (none: `skipped`).
    Jobs(Vec<NewCiJob>),
    /// `FG-CI-0001`: at most [`MAX_STORED_PROBLEMS`] problems.
    Invalid(Vec<StoredProblem>),
    /// `FG-CI-0002`: nothing could be read.
    Error,
}

/// A pipeline to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPipeline {
    pub request: PipelineRequest,
    /// The request that planned it (the job's or the manual run's).
    pub request_id: String,
    /// sha256 (hex) of the definition's bytes (`None` only for `Error`).
    pub definition_hash: Option<String>,
    /// A push to a branch that is protected now (the only pipelines whose
    /// jobs may be given secrets; `ci_pipelines.protected`). Always `false`
    /// for every other source: the table refuses anything else.
    pub protected: bool,
    pub copy_mark: CopyMark,
    pub planned: Planned,
}

/// What [`create`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Created {
    pub pipeline_id: i64,
    /// `true`: the pipeline already existed (a redelivery); nothing written.
    pub existing: bool,
}

/// The sha256 of a definition, as stored.
pub fn definition_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The row's status, code and problems, and the jobs to write.
fn row_of(
    new: &NewPipeline,
) -> (
    PipelineStatus,
    Option<&'static str>,
    serde_json::Value,
    &[NewCiJob],
) {
    match &new.planned {
        Planned::Jobs(jobs) if jobs.is_empty() => {
            (PipelineStatus::Skipped, None, serde_json::json!([]), &[])
        }
        Planned::Jobs(jobs) => (
            PipelineStatus::Pending,
            None,
            serde_json::json!([]),
            jobs.as_slice(),
        ),
        Planned::Invalid(problems) => (
            PipelineStatus::Invalid,
            Some(ErrorCode::CiConfigInvalid.id()),
            serde_json::Value::Array(
                problems
                    .iter()
                    .take(MAX_STORED_PROBLEMS)
                    .map(|p| {
                        serde_json::json!({
                            "line": p.line, "reason": p.reason, "subject": p.subject,
                        })
                    })
                    .collect(),
            ),
            &[],
        ),
        Planned::Error => (
            PipelineStatus::Error,
            Some(ErrorCode::PipelineCreateFailed.id()),
            serde_json::json!([]),
            &[],
        ),
    }
}

/// Insert `new` and its jobs on `c` (inside the caller's transaction).
/// `None`: the push / merge request pipeline exists (nothing written).
async fn insert(
    c: &mut sqlx::PgConnection,
    new: &NewPipeline,
    query: &'static str,
) -> Result<Option<i64>, ForgeError> {
    let r = &new.request;
    let (status, error_code, problems, jobs) = row_of(new);
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, merge_request_id, \
           triggered_by, definition_hash, copy_mark, status, error_code, problems, request_id, \
           protected) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
         ON CONFLICT (project_id, source, ref, sha, coalesce(merge_request_id, 0)) \
           WHERE source IN ('push', 'merge_request') DO NOTHING \
         RETURNING id",
    )
    .bind(r.project.get())
    .bind(r.git_ref.as_str())
    .bind(r.sha.as_str())
    .bind(r.source.as_str())
    .bind(r.merge_request.map(MergeRequestId::get))
    .bind(r.triggered_by.map(UserId::get))
    .bind(new.definition_hash.as_deref())
    .bind(new.copy_mark.as_str())
    .bind(status.as_str())
    .bind(error_code)
    .bind(&problems)
    .bind(&new.request_id)
    .bind(new.protected)
    .fetch_optional(&mut *c)
    .await
    .map_err(|e| query_error(query, e))?;
    let Some(pipeline_id) = inserted else {
        return Ok(None);
    };
    for job in jobs {
        let status = if job.needs.is_empty() {
            JobStatus::Pending
        } else {
            JobStatus::Created
        };
        sqlx::query(
            "INSERT INTO forge.ci_jobs (pipeline_id, project_id, name, stage, wave, needs, \
               image, timeout_s, tags, secrets, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(pipeline_id)
        .bind(r.project.get())
        .bind(&job.name)
        .bind(&job.stage)
        .bind(job.wave)
        .bind(&job.needs)
        .bind(&job.image)
        .bind(job.timeout_s)
        .bind(&job.tags)
        .bind(&job.secrets)
        .bind(status.as_str())
        .execute(&mut *c)
        .await
        .map_err(|e| query_error(query, e))?;
    }
    Ok(Some(pipeline_id))
}

/// Write `new` and its jobs in one transaction (forge's own pipelines: a
/// push or a merge request version — derived state, no audit row).
/// Idempotent for push and merge request pipelines: one that exists is
/// returned with `existing: true` and nothing is written. Errors (nothing
/// written): `FG-DB-*` (a CHECK refused a value: forge planned something
/// the table's bounds do not admit, `FG-DB-0010`).
pub async fn create(db: &Db, new: &NewPipeline) -> Result<Created, ForgeError> {
    const QUERY: &str = "pipelines.create";
    let r = &new.request;
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        if let Some(pipeline_id) = insert(&mut tx, new, QUERY).await? {
            tx.commit().await.map_err(|e| query_error(QUERY, e))?;
            return Ok(Created {
                pipeline_id,
                existing: false,
            });
        }
        let existing: i64 = sqlx::query_scalar(
            "SELECT id FROM forge.ci_pipelines WHERE project_id = $1 AND source = $2 \
             AND ref = $3 AND sha = $4 AND coalesce(merge_request_id, 0) = $5",
        )
        .bind(r.project.get())
        .bind(r.source.as_str())
        .bind(r.git_ref.as_str())
        .bind(r.sha.as_str())
        .bind(r.merge_request.map_or(0, MergeRequestId::get))
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Created {
            pipeline_id: existing,
            existing: true,
        })
    })
    .await
}

/// The audit resource type of a person's change (the project).
const RESOURCE: &str = "project";

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

async fn decided_facts(
    tx: &mut ChangeTx,
    project: ProjectId,
    caller: UserId,
) -> Result<ProjectUpdateFacts, ForgeError> {
    let locked = lock_shared(tx, project).await?;
    locked_access_facts(tx, project, caller, &locked).await
}

/// A person's manual run (`source = manual`): `new`, its jobs and one
/// `pipeline.run_manual` audit row in one transaction, decided again on
/// the locked project and caller (`decide`: forge-pipelines reconfirms
/// `authorize()`). Errors (nothing written): `decide`'s, `FG-REPO-0005
/// resource=project`, `FG-DB-*`.
pub async fn run_manual_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    new: &NewPipeline,
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(i64, P), ForgeError> {
    const QUERY: &str = "pipelines.run_manual";
    if new.request.source != Source::Manual {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "not_a_manual_run")
            .with_context("source", new.request.source.as_str()));
    }
    let project = new.request.project;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let inserted = tx.step(|c| insert(c, new, QUERY)).await?;
        // A manual run has no unique key: an insert always happens.
        let pipeline = inserted.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "manual_run_deduplicated")
        })?;
        let change = PipelineChange::RunManual {
            project,
            pipeline,
            git_ref: new.request.git_ref.as_str().to_owned(),
            sha: new.request.sha.as_str().to_owned(),
            status: row_of(new).0.as_str().to_owned(),
        };
        Ok(Settle::Commit(record(change, project), (pipeline, decided)))
    }
    .await;
    tx.settle("pipeline.run_manual", outcome).await
}

/// One pipeline as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPipeline {
    pub id: i64,
    pub project: ProjectId,
    pub git_ref: String,
    pub sha: String,
    pub source: Source,
    pub merge_request: Option<MergeRequestId>,
    pub triggered_by: Option<UserId>,
    pub definition_hash: Option<String>,
    /// A push to a branch that was protected when the pipeline was created.
    pub protected: bool,
    pub copy_mark: CopyMark,
    pub status: PipelineStatus,
    pub error_code: Option<String>,
    pub problems: Vec<StoredProblem>,
    pub request_id: String,
    /// RFC 3339 (UTC).
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

impl StoredPipeline {
    /// May this pipeline's jobs be given secrets? THE one rule (ADR-0029,
    /// obligation of C3/C6): only a push to a branch that was protected at
    /// creation. A merge request's pipeline, a manual and a scheduled run
    /// never, whatever their jobs name in `secrets`; a retry changes neither
    /// column, so it keeps the answer. Whoever hands a job its secrets asks
    /// here and nowhere else (test `only_a_protected_push_may_release_secrets`).
    pub fn secrets_allowed(&self) -> bool {
        self.source == Source::Push && self.protected
    }
}

/// One job as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredJob {
    pub id: i64,
    pub name: String,
    pub stage: String,
    pub wave: i32,
    pub needs: Vec<String>,
    pub image: String,
    pub timeout_s: i32,
    pub tags: Vec<String>,
    /// The secret names only (values never reach the database here, C6).
    pub secrets: Vec<String>,
    pub status: JobStatus,
    pub attempt: i32,
    pub error_code: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

const PIPELINE_COLUMNS: &str = "id, project_id, ref AS git_ref, sha, source, merge_request_id, \
     triggered_by, definition_hash, protected, copy_mark, status, error_code, problems, request_id, \
     to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
     to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at, \
     to_char(finished_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS finished_at";

/// One row of [`PIPELINE_COLUMNS`] (by name: more columns than a tuple reads).
#[derive(Debug, sqlx::FromRow)]
struct PipelineRow {
    id: i64,
    project_id: i64,
    git_ref: String,
    sha: String,
    source: String,
    merge_request_id: Option<i64>,
    triggered_by: Option<i64>,
    definition_hash: Option<String>,
    protected: bool,
    copy_mark: String,
    status: String,
    error_code: Option<String>,
    problems: serde_json::Value,
    request_id: String,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>,
}

fn unreadable(reason: &'static str, id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("pipeline_id", id)
}

fn problem_of(id: i64, p: &serde_json::Value) -> Result<StoredProblem, ForgeError> {
    let line = p.get("line").and_then(serde_json::Value::as_i64);
    let reason = p.get("reason").and_then(serde_json::Value::as_str);
    let (Some(line), Some(reason)) = (line, reason) else {
        return Err(unreadable("stored_problem_invalid", id));
    };
    Ok(StoredProblem {
        line,
        reason: reason.to_owned(),
        subject: p
            .get("subject")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

fn pipeline_of(row: PipelineRow) -> Result<StoredPipeline, ForgeError> {
    let id = row.id;
    // The table's CHECK keeps it an array.
    let problems = row
        .problems
        .as_array()
        .ok_or_else(|| unreadable("stored_problems_invalid", id))?
        .iter()
        .map(|p| problem_of(id, p))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(StoredPipeline {
        id,
        project: ProjectId::new(row.project_id),
        git_ref: row.git_ref,
        sha: row.sha,
        source: Source::parse(&row.source)
            .ok_or_else(|| unreadable("stored_source_invalid", id))?,
        merge_request: row.merge_request_id.map(MergeRequestId::new),
        triggered_by: row.triggered_by.map(UserId::new),
        definition_hash: row.definition_hash,
        protected: row.protected,
        copy_mark: CopyMark::parse(&row.copy_mark)
            .ok_or_else(|| unreadable("stored_mark_invalid", id))?,
        status: PipelineStatus::parse(&row.status)
            .ok_or_else(|| unreadable("stored_status_invalid", id))?,
        error_code: row.error_code,
        problems,
        request_id: row.request_id,
        created_at: row.created_at,
        updated_at: row.updated_at,
        finished_at: row.finished_at,
    })
}

/// A project's pipelines, newest first: at most `limit` (1..=[`MAX_PAGE`])
/// with an id below `before` (the next page), optionally of one ref.
pub async fn list(
    db: &Db,
    project: ProjectId,
    git_ref: Option<&str>,
    before: Option<i64>,
    limit: i64,
) -> Result<Vec<StoredPipeline>, ForgeError> {
    const QUERY: &str = "pipelines.list";
    let limit = limit.clamp(1, MAX_PAGE);
    let rows: Vec<PipelineRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {PIPELINE_COLUMNS} FROM forge.ci_pipelines WHERE project_id = $1 \
                 AND ($2::text IS NULL OR ref = $2) AND ($3::bigint IS NULL OR id < $3) \
                 ORDER BY id DESC LIMIT $4"
            ))
            .bind(project.get())
            .bind(git_ref)
            .bind(before)
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(pipeline_of).collect()
}

type JobRow = (
    i64,
    String,
    String,
    i32,
    Vec<String>,
    String,
    i32,
    Vec<String>,
    Vec<String>,
    String,
    i32,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

/// One pipeline of `project` with its jobs (by wave, stage, name); `None`
/// when the project has no such pipeline.
pub async fn get(
    db: &Db,
    project: ProjectId,
    pipeline: i64,
) -> Result<Option<(StoredPipeline, Vec<StoredJob>)>, ForgeError> {
    const QUERY: &str = "pipelines.get";
    let (row, jobs): (Option<PipelineRow>, Vec<JobRow>) = db
        .within(QUERY, async {
            let row: Option<PipelineRow> = sqlx::query_as(&format!(
                "SELECT {PIPELINE_COLUMNS} FROM forge.ci_pipelines WHERE project_id = $1 AND id = $2"
            ))
            .bind(project.get())
            .bind(pipeline)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            if row.is_none() {
                return Ok((None, Vec::new()));
            }
            let jobs: Vec<JobRow> = sqlx::query_as(
                "SELECT id, name, stage, wave, needs, image, timeout_s, tags, secrets, status, \
                   attempt, error_code, \
                   to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                   to_char(started_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                   to_char(finished_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 FROM forge.ci_jobs WHERE pipeline_id = $1 ORDER BY wave, stage, name",
            )
            .bind(pipeline)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            Ok((row, jobs))
        })
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored = pipeline_of(row)?;
    let mut out = Vec::with_capacity(jobs.len());
    for j in jobs {
        out.push(StoredJob {
            id: j.0,
            name: j.1,
            stage: j.2,
            wave: j.3,
            needs: j.4,
            image: j.5,
            timeout_s: j.6,
            tags: j.7,
            secrets: j.8,
            status: JobStatus::parse(&j.9)
                .ok_or_else(|| unreadable("stored_job_status_invalid", stored.id))?,
            attempt: j.10,
            error_code: j.11,
            created_at: j.12,
            started_at: j.13,
            finished_at: j.14,
        });
    }
    Ok(Some((stored, out)))
}

/// What a cancel or a retry does to a pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transition {
    Cancel,
    Retry,
}

impl Transition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Retry => "retry",
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Cancel => "pipeline.canceled",
            Self::Retry => "pipeline.retried",
        }
    }

    fn allowed(self, status: PipelineStatus) -> bool {
        match self {
            Self::Cancel => matches!(status, PipelineStatus::Pending | PipelineStatus::Running),
            Self::Retry => matches!(status, PipelineStatus::Failed | PipelineStatus::Canceled),
        }
    }

    fn jobs_sql(self) -> &'static str {
        match self {
            Self::Cancel => {
                "UPDATE forge.ci_jobs SET status = 'canceled', runner_id = NULL, \
                   lease_until = NULL, updated_at = now(), finished_at = now() \
                 WHERE pipeline_id = $1 AND status IN ('created', 'pending', 'running')"
            }
            Self::Retry => {
                "UPDATE forge.ci_jobs SET \
                   status = CASE WHEN cardinality(needs) = 0 THEN 'pending' ELSE 'created' END, \
                   attempt = attempt + 1, runner_id = NULL, lease_until = NULL, \
                   error_code = NULL, started_at = NULL, finished_at = NULL, updated_at = now() \
                 WHERE pipeline_id = $1 AND status IN ('failed', 'canceled')"
            }
        }
    }

    fn pipeline_sql(self) -> &'static str {
        match self {
            Self::Cancel => {
                "UPDATE forge.ci_pipelines SET status = 'canceled', finished_at = now(), \
                   updated_at = now() WHERE id = $1"
            }
            Self::Retry => {
                "UPDATE forge.ci_pipelines SET status = 'pending', finished_at = NULL, \
                   updated_at = now() WHERE id = $1"
            }
        }
    }
}

/// Decide again on the locked project and caller, lock the pipeline,
/// check `transition` is allowed, apply it and write its audit row.
async fn transition<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, pipeline): (ProjectId, i64),
    caller: UserId,
    transition: Transition,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    const QUERY: &str = "pipelines.transition";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let decided = decide(&decided_facts(&mut tx, project, caller).await?)?;
        let status: Option<String> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT status FROM forge.ci_pipelines WHERE project_id = $1 AND id = $2 \
                     FOR UPDATE",
                )
                .bind(project.get())
                .bind(pipeline)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let status = status.ok_or_else(|| {
            ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "pipeline")
                .with_context("reason", "absent")
                .with_context("pipeline_id", pipeline)
        })?;
        let parsed = PipelineStatus::parse(&status)
            .ok_or_else(|| unreadable("stored_status_invalid", pipeline))?;
        if !transition.allowed(parsed) {
            return Err(ForgeError::new(ErrorCode::PipelineStateConflict)
                .with_context("operation", transition.as_str())
                .with_context("status", parsed.as_str())
                .with_context("pipeline_id", pipeline)
                .with_context("project_id", project.get()));
        }
        let jobs = tx
            .step(|c| async move {
                let jobs = sqlx::query(transition.jobs_sql())
                    .bind(pipeline)
                    .execute(&mut *c)
                    .await
                    .map_err(|e| query_error(QUERY, e))?
                    .rows_affected();
                sqlx::query(transition.pipeline_sql())
                    .bind(pipeline)
                    .execute(&mut *c)
                    .await
                    .map_err(|e| query_error(QUERY, e))?;
                Ok(jobs)
            })
            .await?;
        let change = match transition {
            Transition::Cancel => PipelineChange::Canceled {
                project,
                pipeline,
                jobs,
            },
            Transition::Retry => PipelineChange::Retried {
                project,
                pipeline,
                jobs,
            },
        };
        Ok(Settle::Commit(record(change, project), decided))
    }
    .await;
    tx.settle(transition.action(), outcome).await
}

/// Cancel a `pending` or `running` pipeline and its unfinished jobs, with
/// one `pipeline.canceled` audit row (decided again: `decide`). Errors
/// (nothing written): `decide`'s, `FG-REPO-0005 resource=pipeline` (no
/// such pipeline in `project`), `FG-CI-0003` for any other state.
pub async fn cancel_pipeline_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    target: (ProjectId, i64),
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    transition(db, ctx, target, caller, Transition::Cancel, decide).await
}

/// Retry a `failed` or `canceled` pipeline: its failed and cancelled jobs
/// get a new attempt (`pending` without `needs`, else `created`) and the
/// pipeline is `pending` again; one `pipeline.retried` audit row. Errors
/// as [`cancel_pipeline_decided`].
pub async fn retry_pipeline_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    target: (ProjectId, i64),
    caller: UserId,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<P, ForgeError> {
    transition(db, ctx, target, caller, Transition::Retry, decide).await
}

/// What doctor's `ci.pipelines` reads: `error` pipelines of the last 24
/// hours, `pending` / `running` pipelines older than `stale_after`, and dead
/// `ci.pipeline_create` jobs of the last 7 days (each push they stand for
/// may have no pipeline). Ids are the newest first, at most [`HEALTH_IDS`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineHealth {
    pub errors: i64,
    pub error_ids: Vec<i64>,
    pub stale: i64,
    pub stale_ids: Vec<i64>,
    pub dead_jobs: i64,
    pub dead_job_ids: Vec<i64>,
}

/// The most ids [`health`] names per kind.
pub const HEALTH_IDS: i64 = 10;

/// [`PipelineHealth`] on the read pool.
pub async fn health(
    db: &Db,
    stale_after: std::time::Duration,
) -> Result<PipelineHealth, ForgeError> {
    const QUERY: &str = "pipelines.health";
    let row: (i64, Vec<i64>, i64, Vec<i64>, i64, Vec<i64>) = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT \
                   (SELECT count(*) FROM forge.ci_pipelines \
                     WHERE status = 'error' AND created_at > now() - interval '24 hours'), \
                   ARRAY(SELECT id FROM forge.ci_pipelines \
                     WHERE status = 'error' AND created_at > now() - interval '24 hours' \
                     ORDER BY id DESC LIMIT $2), \
                   (SELECT count(*) FROM forge.ci_pipelines WHERE status IN ('pending', 'running') \
                     AND updated_at < now() - make_interval(secs => $1)), \
                   ARRAY(SELECT id FROM forge.ci_pipelines WHERE status IN ('pending', 'running') \
                     AND updated_at < now() - make_interval(secs => $1) ORDER BY id DESC LIMIT $2), \
                   (SELECT count(*) FROM forge.jobs WHERE kind = $3 AND state = 'dead' \
                     AND finished_at > now() - interval '7 days'), \
                   ARRAY(SELECT id FROM forge.jobs WHERE kind = $3 AND state = 'dead' \
                     AND finished_at > now() - interval '7 days' ORDER BY id DESC LIMIT $2)",
            )
            .bind(stale_after.as_secs_f64())
            .bind(HEALTH_IDS)
            .bind(PIPELINE_CREATE_JOB)
            .fetch_one(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(PipelineHealth {
        errors: row.0,
        error_ids: row.1,
        stale: row.2,
        stale_ids: row.3,
        dead_jobs: row.4,
        dead_job_ids: row.5,
    })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
