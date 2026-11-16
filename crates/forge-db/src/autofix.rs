//! The deterministic autofix's rows (M4 C14, ADR-0017 Amendment C14;
//! migration 20261221100000): `forge.project_autofix` (a project's switch;
//! no row = off) and `forge.autofix_attempts` (one fixer run for one merge
//! request at one head). Every decision is forge-review's (`Autofixer`).
//!
//! - **A person's command** (audited, `ChangeTx`): [`set_enabled_decided`]
//!   (`autofix.settings_changed`).
//! - **The trigger** ([`AutofixTriggers`], subscriber `autofix-triggers`,
//!   `independent`): a `check.completed` of `ci/pipeline` that concluded
//!   `failure` on a merge request's current merge result
//!   (`merge_result_refs`), and every `merge_request.conflict_detected`,
//!   record one `queued` attempt — only for a project whose autofix is on,
//!   for an open merge request, once per (merge request, trigger, head,
//!   target) (`autofix_attempts_once`). A subscriber never calls git.
//! - **The worker** (`forge serve`): [`queued`], [`running`], [`start`],
//!   [`finish`] (the attempt's end and the merge request's event
//!   `merge_request.autofix_finished`, actor system, in one transaction),
//!   [`requeue`] (the next fixer on the same head), [`tried_fixers`],
//!   [`fixer_runs_today`].
//! - **Reads**: [`settings`], [`latest`] (the merge request's one line),
//!   [`is_bot_commit`] (the version job carries no approval onto the bot's
//!   commit, ADR-0017 decision 7).

use forge_core::autofix::{AttemptState, AutofixChange, AutofixFinished, AutofixTrigger};
use forge_core::checks::CI_PIPELINE_CHECK;
use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{AuditDetails, DedupKey, DedupPart, EventType, MergeRequestChange, ObjectId};
use forge_core::{RequestId, UserKind};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome, Settle,
};
use crate::pipelines::NewPipeline;
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};
use crate::{query_error, read_query_error, Db};

/// The subscriber's name (`forge.event_subscribers`).
pub const AUTOFIX_TRIGGERS_SUBSCRIBER: &str = "autofix-triggers";

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

fn unreadable(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

/// A project's autofix switch as stored (or the default: off).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutofixSettings {
    pub project: ProjectId,
    pub enabled: bool,
    /// Who turned it on or off last (the enabler); `None`: never set.
    pub changed_by: Option<UserId>,
    /// UTC, RFC 3339.
    pub changed_at: Option<String>,
}

type SettingsRow = (bool, i64, String);

fn settings_of(project: ProjectId, row: Option<SettingsRow>) -> AutofixSettings {
    match row {
        Some((enabled, by, at)) => AutofixSettings {
            project,
            enabled,
            changed_by: Some(UserId::new(by)),
            changed_at: Some(at),
        },
        None => AutofixSettings {
            project,
            enabled: false,
            changed_by: None,
            changed_at: None,
        },
    }
}

fn settings_sql() -> String {
    format!(
        "SELECT enabled, changed_by, to_char(changed_at AT TIME ZONE 'UTC', {TS}) \
         FROM forge.project_autofix WHERE project_id = $1"
    )
}

/// The settings of `project` (`GET …/autofix`, the worker).
pub async fn settings(db: &Db, project: ProjectId) -> Result<AutofixSettings, ForgeError> {
    const QUERY: &str = "autofix.settings";
    db.within(QUERY, async {
        let row: Option<SettingsRow> = sqlx::query_as(&settings_sql())
            .bind(project.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        Ok(settings_of(project, row))
    })
    .await
}

/// What a person's change did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOutcome {
    Changed,
    /// Already so: nothing written.
    Unchanged,
}

/// Turn `project`'s autofix on or off for `caller`, decided again on the
/// locked project (`decide`: forge-review's reconfirmed
/// `AutofixSettingsManage`). One `autofix.settings_changed` audit row;
/// [`ChangeOutcome::Unchanged`] writes nothing. Turning it on makes
/// `caller` the enabler.
pub async fn set_enabled_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    enabled: bool,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(ChangeOutcome, AutofixSettings, P), ForgeError> {
    const ACTION: &str = "autofix.settings_changed";
    const QUERY: &str = "autofix.set_enabled";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock_shared(&mut tx, project).await?;
        let decided = decide(&locked_access_facts(&mut tx, project, caller, &locked).await?)?;
        let before: Option<SettingsRow> = tx
            .step(|c| async move {
                sqlx::query_as(&settings_sql())
                    .bind(project.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let before = settings_of(project, before);
        if before.enabled == enabled && before.changed_by.is_some() {
            return Ok(Settle::Discard((ChangeOutcome::Unchanged, before, decided)));
        }
        let after: SettingsRow = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "INSERT INTO forge.project_autofix (project_id, enabled, changed_by) \
                     VALUES ($1, $2, $3) ON CONFLICT (project_id) DO UPDATE SET \
                     enabled = EXCLUDED.enabled, changed_by = EXCLUDED.changed_by, \
                     changed_at = now() \
                     RETURNING enabled, changed_by, \
                       to_char(changed_at AT TIME ZONE 'UTC', {TS})"
                ))
                .bind(project.get())
                .bind(enabled)
                .bind(caller.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let details = AuditDetails::Autofix(AutofixChange::SettingsChanged { project, enabled });
        let record = ChangeRecord {
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
        };
        Ok(Settle::Commit(
            record,
            (
                ChangeOutcome::Changed,
                settings_of(project, Some(after)),
                decided,
            ),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// One finding of a refused or proposed attempt (`detail`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailPath {
    pub path: String,
    pub reason: String,
}

/// One attempt as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAttempt {
    pub id: i64,
    pub project: ProjectId,
    pub merge_request: MergeRequestId,
    /// The merge request's number.
    pub iid: i64,
    pub trigger: AutofixTrigger,
    pub head_sha: ObjectId,
    pub target_sha: ObjectId,
    pub fixer: Option<String>,
    pub state: AttemptState,
    pub reason: Option<String>,
    pub detail: Vec<DetailPath>,
    pub error_code: Option<String>,
    pub input_sha: Option<ObjectId>,
    pub pipeline_id: Option<i64>,
    pub job_id: Option<i64>,
    pub commit_sha: Option<ObjectId>,
    pub request_id: String,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub updated_at: String,
}

#[derive(sqlx::FromRow)]
struct AttemptRow {
    id: i64,
    project_id: i64,
    merge_request_id: i64,
    iid: i64,
    trigger: String,
    head_sha: String,
    target_sha: String,
    fixer: Option<String>,
    state: String,
    reason: Option<String>,
    detail: serde_json::Value,
    error_code: Option<String>,
    input_sha: Option<String>,
    pipeline_id: Option<i64>,
    job_id: Option<i64>,
    commit_sha: Option<String>,
    request_id: String,
    created_at: String,
    updated_at: String,
}

fn columns() -> String {
    format!(
        "a.id, a.project_id, a.merge_request_id, m.iid, a.trigger, a.head_sha, a.target_sha,          a.fixer, a.state, a.reason, a.detail, a.error_code, a.input_sha, a.pipeline_id,          a.job_id, a.commit_sha, a.request_id,          to_char(a.created_at AT TIME ZONE 'UTC', {TS}) AS created_at,          to_char(a.updated_at AT TIME ZONE 'UTC', {TS}) AS updated_at"
    )
}

fn oid(raw: &str) -> Result<ObjectId, ForgeError> {
    ObjectId::parse(raw).map_err(|_| unreadable("stored_autofix_sha_invalid"))
}

fn attempt_of(r: AttemptRow) -> Result<StoredAttempt, ForgeError> {
    let detail = r
        .detail
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|d| {
                    Some(DetailPath {
                        path: d.get("path")?.as_str()?.to_owned(),
                        reason: d.get("reason")?.as_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(StoredAttempt {
        id: r.id,
        project: ProjectId::new(r.project_id),
        merge_request: MergeRequestId::new(r.merge_request_id),
        iid: r.iid,
        trigger: AutofixTrigger::parse(&r.trigger)
            .ok_or_else(|| unreadable("stored_trigger_invalid"))?,
        head_sha: oid(&r.head_sha)?,
        target_sha: oid(&r.target_sha)?,
        fixer: r.fixer,
        state: AttemptState::parse(&r.state).ok_or_else(|| unreadable("stored_state_invalid"))?,
        reason: r.reason,
        detail,
        error_code: r.error_code,
        input_sha: r.input_sha.as_deref().map(oid).transpose()?,
        pipeline_id: r.pipeline_id,
        job_id: r.job_id,
        commit_sha: r.commit_sha.as_deref().map(oid).transpose()?,
        request_id: r.request_id,
        created_at: r.created_at,
        updated_at: r.updated_at,
    })
}

async fn attempts_where(
    db: &Db,
    query: &'static str,
    filter: &str,
    bind: i64,
) -> Result<Vec<StoredAttempt>, ForgeError> {
    let rows: Vec<AttemptRow> = db
        .within(query, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.autofix_attempts a \
                 JOIN forge.merge_requests m ON m.id = a.merge_request_id WHERE {filter}",
                columns()
            ))
            .bind(bind)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(query, e))
        })
        .await?;
    rows.into_iter().map(attempt_of).collect()
}

/// The oldest queued attempts (at most `limit`).
pub async fn queued(db: &Db, limit: i64) -> Result<Vec<StoredAttempt>, ForgeError> {
    attempts_where(
        db,
        "autofix.queued",
        "a.state = 'queued' ORDER BY a.id LIMIT $1",
        limit,
    )
    .await
}

/// A running attempt and its fixer job's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningAttempt {
    pub attempt: StoredAttempt,
    /// `ci_jobs.status` of its job (`created` … `canceled`).
    pub job_status: String,
    /// The `autofix.bundle` artifact of the job's current attempt:
    /// `(artifact id, sha256 hex)`.
    pub bundle: Option<(i64, String)>,
}

/// The running attempts whose job ended (at most `limit`, oldest first).
pub async fn running(db: &Db, limit: i64) -> Result<Vec<RunningAttempt>, ForgeError> {
    const QUERY: &str = "autofix.running";
    let attempts = attempts_where(
        db,
        QUERY,
        "a.state = 'running' AND EXISTS (SELECT 1 FROM forge.ci_jobs j WHERE j.id = a.job_id \
           AND j.status IN ('succeeded', 'failed', 'canceled')) ORDER BY a.id LIMIT $1",
        limit,
    )
    .await?;
    let mut out = Vec::with_capacity(attempts.len());
    for attempt in attempts {
        let job = attempt
            .job_id
            .ok_or_else(|| unreadable("running_without_job"))?;
        let (status, bundle): (String, Option<(i64, Vec<u8>)>) = db
            .within(QUERY, async {
                let status: String =
                    sqlx::query_scalar("SELECT status FROM forge.ci_jobs WHERE id = $1")
                        .bind(job)
                        .fetch_one(db.write_pool())
                        .await
                        .map_err(|e| query_error(QUERY, e))?;
                let bundle: Option<(i64, Vec<u8>)> = sqlx::query_as(
                    "SELECT a.id, a.sha256 FROM forge.ci_artifacts a JOIN forge.ci_jobs j \
                     ON j.id = a.job_id AND j.attempt = a.attempt \
                     WHERE a.job_id = $1 AND a.path = $2 ORDER BY a.id DESC LIMIT 1",
                )
                .bind(job)
                .bind(forge_core::autofix::BUNDLE_ARTIFACT_PATH)
                .fetch_optional(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))?;
                Ok((status, bundle))
            })
            .await?;
        out.push(RunningAttempt {
            attempt,
            job_status: status,
            bundle: bundle
                .map(|(id, digest)| (id, digest.iter().map(|b| format!("{b:02x}")).collect())),
        });
    }
    Ok(out)
}

/// The merge request's latest attempt (the one line).
pub async fn latest(db: &Db, mr: MergeRequestId) -> Result<Option<StoredAttempt>, ForgeError> {
    Ok(attempts_where(
        db,
        "autofix.latest",
        "a.merge_request_id = $1 ORDER BY a.id DESC LIMIT 1",
        mr.get(),
    )
    .await?
    .into_iter()
    .next())
}

/// The fixers already run for `attempt`'s merge request, trigger, head and
/// target (the next fixer is one not in it).
pub async fn tried_fixers(db: &Db, attempt: &StoredAttempt) -> Result<Vec<String>, ForgeError> {
    const QUERY: &str = "autofix.tried";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT fixer FROM forge.autofix_attempts WHERE merge_request_id = $1 \
             AND trigger = $2 AND head_sha = $3 AND target_sha = $4 AND fixer IS NOT NULL",
        )
        .bind(attempt.merge_request.get())
        .bind(attempt.trigger.as_str())
        .bind(attempt.head_sha.as_str())
        .bind(attempt.target_sha.as_str())
        .fetch_all(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// How many fixers ran for merge request `mr` in the last 24 hours.
pub async fn fixer_runs_today(db: &Db, mr: MergeRequestId) -> Result<i64, ForgeError> {
    const QUERY: &str = "autofix.runs_today";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT count(*) FROM forge.autofix_attempts WHERE merge_request_id = $1 \
             AND fixer IS NOT NULL AND created_at > now() - interval '24 hours'",
        )
        .bind(mr.get())
        .fetch_one(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// Whether `sha` is a commit the bot built in `project` (recorded before
/// it lands: the database is the truth, never the trailers).
pub async fn is_bot_commit(
    db: &Db,
    project: ProjectId,
    sha: &ObjectId,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "autofix.is_bot_commit";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.autofix_attempts WHERE project_id = $1 \
             AND commit_sha = $2)",
        )
        .bind(project.get())
        .bind(sha.as_str())
        .fetch_one(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// Start `attempt` with `fixer`: its pipeline and job (`new`, source
/// `autofix`) and the attempt `running`, in one transaction. `None`: the
/// attempt is no longer queued (nothing written). The job's id otherwise.
pub async fn start(
    db: &Db,
    attempt: i64,
    fixer: &str,
    input: &ObjectId,
    new: &NewPipeline,
) -> Result<Option<(i64, i64)>, ForgeError> {
    const QUERY: &str = "autofix.start";
    if new.request.source != crate::pipelines::Source::Autofix {
        return Err(unreadable("autofix_pipeline_source"));
    }
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let live: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM forge.autofix_attempts WHERE id = $1 AND state = 'queued' \
             FOR UPDATE SKIP LOCKED",
        )
        .bind(attempt)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        if live.is_none() {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Ok(None);
        }
        let pipeline = crate::pipelines::insert_autofix(&mut tx, new).await?;
        let job: i64 = sqlx::query_scalar(
            "SELECT id FROM forge.ci_jobs WHERE pipeline_id = $1 ORDER BY id LIMIT 1",
        )
        .bind(pipeline)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(
            "UPDATE forge.autofix_attempts SET state = 'running', fixer = $2, input_sha = $3, \
             pipeline_id = $4, job_id = $5, updated_at = now() WHERE id = $1",
        )
        .bind(attempt)
        .bind(fixer)
        .bind(input.as_str())
        .bind(pipeline)
        .bind(job)
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Some((pipeline, job)))
    })
    .await
}

/// Record `commit` as the bot's commit of running attempt `attempt`, just
/// before the source branch is moved to it (the version job then knows it
/// is the bot's). `false`: the attempt is no longer running.
pub async fn set_candidate(db: &Db, attempt: i64, commit: &ObjectId) -> Result<bool, ForgeError> {
    const QUERY: &str = "autofix.set_candidate";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.autofix_attempts SET commit_sha = $2, updated_at = now()              WHERE id = $1 AND state = 'running'",
        )
        .bind(attempt)
        .bind(commit.as_str())
        .execute(db.write_pool())
        .await
        .map(|r| r.rows_affected() == 1)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finish {
    pub state: AttemptState,
    pub reason: Option<&'static str>,
    pub detail: Vec<DetailPath>,
    pub error_code: Option<ErrorCode>,
    pub commit: Option<ObjectId>,
    /// The fixer chosen (an attempt may end before one was).
    pub fixer: Option<String>,
}

/// End `attempt` (queued or running) as `finish`, and record the merge
/// request's event `merge_request.autofix_finished` (actor `ctx`'s: the
/// system) in the same transaction. `false`: it had ended already
/// (nothing written).
pub async fn finish(
    db: &Db,
    ctx: &ChangeContext,
    attempt: &StoredAttempt,
    finish: &Finish,
) -> Result<bool, ForgeError> {
    const ACTION: &str = "merge_request.autofix_finished";
    const QUERY: &str = "autofix.finish";
    if !finish.state.is_terminal() {
        return Err(unreadable("autofix_finish_not_terminal"));
    }
    let detail = serde_json::Value::Array(
        finish
            .detail
            .iter()
            .take(20)
            .map(|d| {
                let path: String = d.path.chars().take(255).collect();
                serde_json::json!({"path": path, "reason": d.reason})
            })
            .collect(),
    );
    let change = MergeRequestChange::AutofixFinished(AutofixFinished {
        merge_request: attempt.merge_request,
        iid: attempt.iid,
        attempt: attempt.id,
        trigger: attempt.trigger,
        fixer: finish.fixer.clone().or_else(|| attempt.fixer.clone()), // clone: the event owns it
        state: finish.state,
        reason: finish.reason.map(str::to_owned),
        commit: finish.commit.clone(), // clone: the event owns it
    });
    let dedup = DedupKey::new(
        change.dedup_prefix(),
        &[
            DedupPart::Id(attempt.merge_request.get()),
            DedupPart::Id(attempt.id),
        ],
    )?;
    let details = AuditDetails::MergeRequest(change);
    let event_type: EventType = details
        .event_type()
        .ok_or_else(|| unreadable("audit_only_event"))?;
    let value = details.to_value();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let id = attempt.id;
    let fixer = finish.fixer.clone(); // clone: bound in the step
    let state = finish.state.as_str();
    let reason = finish.reason;
    let code = finish.error_code.map(|c| c.id());
    let commit = finish.commit.as_ref().map(|c| c.as_str().to_owned());
    let outcome = async {
        let updated = tx
            .step(|c| async move {
                sqlx::query(
                    "UPDATE forge.autofix_attempts SET state = $2, reason = $3, detail = $4, \
                     error_code = $5, commit_sha = coalesce($6, commit_sha), fixer = coalesce(fixer, $7), \
                     updated_at = now(), finished_at = now() \
                     WHERE id = $1 AND state IN ('queued', 'running')",
                )
                .bind(id)
                .bind(state)
                .bind(reason)
                .bind(&detail)
                .bind(code)
                .bind(commit)
                .bind(fixer)
                .execute(c)
                .await
                .map(|r| r.rows_affected())
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        if updated == 0 {
            return Ok(Settle::Discard(false));
        }
        let record = ChangeRecord {
            audit: AuditEntry {
                action: details.action(),
                outcome: Outcome::Succeeded,
                resource_type: "project",
                resource_id: Some(attempt.project.get()),
                project_id: Some(attempt.project),
                error_code: finish.error_code,
                details: value.clone(), // clone: the same JSON is the payload
            },
            affected: Vec::new(),
            events: vec![DomainEvent {
                event_type,
                schema_version: 1,
                aggregate_type: "merge_request",
                aggregate_id: attempt.merge_request.get(),
                project_id: Some(attempt.project),
                payload: value,
                dedup_key: Some(dedup),
            }],
        };
        Ok(Settle::Commit(record, true))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Queue the next fixer for `attempt`'s merge request, trigger, head and
/// target (the fixer column empty again). `false`: one is queued already.
pub async fn requeue(
    db: &Db,
    attempt: &StoredAttempt,
    request_id: &RequestId,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "autofix.requeue";
    db.within(QUERY, async {
        sqlx::query(
            "INSERT INTO forge.autofix_attempts (project_id, merge_request_id, trigger, head_sha, \
               target_sha, state, request_id) VALUES ($1, $2, $3, $4, $5, 'queued', $6) \
             ON CONFLICT (merge_request_id, trigger, head_sha, target_sha, coalesce(fixer, '')) \
             DO NOTHING",
        )
        .bind(attempt.project.get())
        .bind(attempt.merge_request.get())
        .bind(attempt.trigger.as_str())
        .bind(attempt.head_sha.as_str())
        .bind(attempt.target_sha.as_str())
        .bind(request_id.as_str())
        .execute(db.write_pool())
        .await
        .map(|r| r.rows_affected() == 1)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// The facts the bot's target is built from (forge-review builds
/// `Resource::Autofix` from them), read in one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetFacts {
    pub mr_open: bool,
    pub author: UserId,
    pub author_kind: UserKind,
    pub author_active: bool,
    pub enabled: bool,
    pub enabler: Option<UserId>,
    pub enabler_active: bool,
    pub head_sha: Option<String>,
    pub source_branch: String,
    pub target_branch: String,
}

/// The facts of merge request `mr` of `project` (`None`: no such merge
/// request).
pub async fn target_facts(
    db: &Db,
    project: ProjectId,
    mr: MergeRequestId,
) -> Result<Option<TargetFacts>, ForgeError> {
    const QUERY: &str = "autofix.target_facts";
    type Row = (
        bool,
        i64,
        String,
        bool,
        Option<bool>,
        Option<i64>,
        Option<bool>,
        Option<String>,
        String,
        String,
    );
    let row: Option<Row> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT m.state = 'open', m.author_id, u.kind, u.state = 'active', a.enabled, \
                   a.changed_by, e.state = 'active', m.head_sha, m.source_branch, m.target_branch \
                 FROM forge.merge_requests m JOIN forge.users u ON u.id = m.author_id \
                 LEFT JOIN forge.project_autofix a ON a.project_id = m.project_id \
                 LEFT JOIN forge.users e ON e.id = a.changed_by \
                 WHERE m.id = $1 AND m.project_id = $2",
            )
            .bind(mr.get())
            .bind(project.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(|r| {
        Ok(TargetFacts {
            mr_open: r.0,
            author: UserId::new(r.1),
            author_kind: UserKind::parse(&r.2)
                .ok_or_else(|| unreadable("stored_user_kind_invalid"))?,
            author_active: r.3,
            enabled: r.4.unwrap_or(false),
            enabler: r.5.map(UserId::new),
            enabler_active: r.6.unwrap_or(false),
            head_sha: r.7,
            source_branch: r.8,
            target_branch: r.9,
        })
    })
    .transpose()
}

/// The trigger (module docs).
#[derive(Debug, Default, Clone, Copy)]
pub struct AutofixTriggers;

fn bad_payload(event: &SourceEvent, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_event_payload")
        .with_context("subscriber", AUTOFIX_TRIGGERS_SUBSCRIBER)
        .with_context("event_type", event.event_type.clone()) // clone: the context owns it
        .with_context("field", field)
        .with_context("seq", event.seq)
}

fn sha_field(event: &SourceEvent, field: &'static str) -> Result<ObjectId, ForgeError> {
    event
        .payload
        .get(field)
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| ObjectId::parse(raw).ok())
        .ok_or_else(|| bad_payload(event, field))
}

impl AutofixTriggers {
    /// A failed `ci/pipeline` on a merge request's current merge result.
    async fn on_check(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        const QUERY: &str = "autofix.trigger_pipeline";
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
        let sha = sha_field(event, "head_sha")?;
        let request_id = event.request_id().unwrap_or_else(RequestId::generate);
        sqlx::query(
            "INSERT INTO forge.autofix_attempts (project_id, merge_request_id, trigger, \
               head_sha, target_sha, state, request_id) \
             SELECT r.project_id, r.merge_request_id, 'pipeline_failed', r.head_sha, \
               r.target_sha, 'queued', $3 \
             FROM forge.merge_result_refs r \
             JOIN forge.merge_requests m ON m.id = r.merge_request_id \
             JOIN forge.project_autofix a ON a.project_id = r.project_id AND a.enabled \
             WHERE r.project_id = $1 AND r.result_sha = $2 AND m.state = 'open' \
               AND m.head_sha = r.head_sha \
             ON CONFLICT (merge_request_id, trigger, head_sha, target_sha, coalesce(fixer, '')) \
             DO NOTHING",
        )
        .bind(project.get())
        .bind(sha.as_str())
        .bind(request_id.as_str())
        .execute(tx.conn())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(())
    }

    /// A merge request that started to conflict with its target.
    async fn on_conflict(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        const QUERY: &str = "autofix.trigger_conflict";
        let mr = event
            .payload
            .get("merge_request_id")
            .and_then(serde_json::Value::as_i64)
            .filter(|id| *id > 0)
            .ok_or_else(|| bad_payload(event, "merge_request_id"))?;
        let head = sha_field(event, "head_sha")?;
        let target = sha_field(event, "target_sha")?;
        let request_id = event.request_id().unwrap_or_else(RequestId::generate);
        sqlx::query(
            "INSERT INTO forge.autofix_attempts (project_id, merge_request_id, trigger, \
               head_sha, target_sha, state, request_id) \
             SELECT m.project_id, m.id, 'conflict', $2, $3, 'queued', $4 \
             FROM forge.merge_requests m \
             JOIN forge.project_autofix a ON a.project_id = m.project_id AND a.enabled \
             WHERE m.id = $1 AND m.state = 'open' \
             ON CONFLICT (merge_request_id, trigger, head_sha, target_sha, coalesce(fixer, '')) \
             DO NOTHING",
        )
        .bind(mr)
        .bind(head.as_str())
        .bind(target.as_str())
        .bind(request_id.as_str())
        .execute(tx.conn())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(())
    }
}

impl Subscriber for AutofixTriggers {
    fn name(&self) -> &'static str {
        AUTOFIX_TRIGGERS_SUBSCRIBER
    }

    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            match EventType::parse(&event.event_type) {
                Some(EventType::CheckCompleted) => Self::on_check(event, tx).await,
                Some(EventType::MergeRequestConflictDetected) => Self::on_conflict(event, tx).await,
                // Every other type: nothing to fix.
                Some(_) => Ok(()),
                None => {
                    tracing::warn!(
                        target: "forge_db",
                        event = "forge.subscribers.unknown_event_type",
                        subscriber = AUTOFIX_TRIGGERS_SUBSCRIBER,
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
#[path = "autofix_tests.rs"]
mod tests;
