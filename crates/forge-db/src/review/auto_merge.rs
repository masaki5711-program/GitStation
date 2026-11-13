//! Auto-merge's rows (M4 C9, ADR-0017 Amendment 2026-11-13; migration
//! 20261214100000): `forge.project_auto_merge` (a project's `merge.auto`
//! and emergency stop; no row = `full`, not stopped),
//! `forge.auto_merge_instance` (the instance's emergency stop) and
//! `forge.auto_merge_states` (what auto-merge did with each open merge
//! request last). Every decision is forge-review's.
//!
//! - **A person's commands** (audited, `ChangeTx`): [`set_mode_decided`],
//!   [`set_project_stop_decided`], [`set_instance_stop_decided`].
//! - **Reads**: [`settings`], [`state_of`]; [`state_on`] inside a
//!   transaction (the merge gate's credential).
//! - **The worker**: [`candidates`], [`record`], [`forget_inactive`].

use std::time::Duration;

use forge_core::auto_merge::{AutoMergeChange, AutoMergeMode, StopClass, StopScope};
use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{AuditDetails, ObjectId, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::access::{lock_account, Account};
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::{query_error, read_query_error, Db};

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

fn unreadable(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

/// Who turned a stop on, and when (UTC, RFC 3339).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopMark {
    pub by: Option<UserId>,
    pub at: String,
}

/// A project's auto-merge settings as stored (or the default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoMergeSettings {
    pub project: ProjectId,
    pub mode: AutoMergeMode,
    /// The project has a row (`false`: the default of a new project).
    pub stored: bool,
    pub mode_by: Option<UserId>,
    pub mode_at: Option<String>,
    pub project_stop: Option<StopMark>,
    pub instance_stop: Option<StopMark>,
}

impl AutoMergeSettings {
    /// The emergency stop in effect (the instance's first).
    pub fn stop(&self) -> Option<StopScope> {
        if self.instance_stop.is_some() {
            Some(StopScope::Instance)
        } else if self.project_stop.is_some() {
            Some(StopScope::Project)
        } else {
            None
        }
    }
}

type SettingsRow = (
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

fn settings_sql(lock: &str) -> String {
    format!(
        "SELECT a.mode, a.mode_by, to_char(a.mode_at AT TIME ZONE 'UTC', {TS}), \
         a.stopped_by, to_char(a.stopped_at AT TIME ZONE 'UTC', {TS}), \
         i.stopped_by, to_char(i.stopped_at AT TIME ZONE 'UTC', {TS}) \
         FROM forge.auto_merge_instance i \
         LEFT JOIN forge.project_auto_merge a ON a.project_id = $1 \
         WHERE i.id = 1 {lock}"
    )
}

fn parse_settings(
    project: ProjectId,
    row: Option<SettingsRow>,
) -> Result<AutoMergeSettings, ForgeError> {
    let (mode, mode_by, mode_at, stopped_by, stopped_at, i_by, i_at) =
        row.ok_or_else(|| unreadable("auto_merge_instance_row_missing"))?;
    let stored = mode.is_some();
    let mode = match mode {
        Some(raw) => AutoMergeMode::parse(&raw)
            .ok_or_else(|| unreadable("stored_auto_merge_mode_invalid"))?,
        None => AutoMergeMode::NEW_PROJECT,
    };
    let mark = |by: Option<i64>, at: Option<String>| {
        at.map(|at| StopMark {
            by: by.map(UserId::new),
            at,
        })
    };
    Ok(AutoMergeSettings {
        project,
        mode,
        stored,
        mode_by: mode_by.map(UserId::new),
        mode_at,
        project_stop: mark(stopped_by, stopped_at),
        instance_stop: mark(i_by, i_at),
    })
}

/// The settings of `project` inside a transaction (shared locks on the
/// rows read, so a toggle committed before is seen and one after waits).
pub(crate) async fn settings_on(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<AutoMergeSettings, ForgeError> {
    let row: Option<SettingsRow> = sqlx::query_as(&settings_sql("FOR SHARE OF i"))
        .bind(project.get())
        .fetch_optional(conn)
        .await
        .map_err(|e| query_error("auto_merge.settings_on", e))?;
    parse_settings(project, row)
}

/// The mode and the emergency stop in effect for `project`, read in the
/// caller's transaction (the merge gate's auto-merge credential).
pub(crate) async fn state_on(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<(AutoMergeMode, Option<StopScope>), ForgeError> {
    let s = settings_on(conn, project).await?;
    Ok((s.mode, s.stop()))
}

/// The settings of `project` (`GET …/auto-merge`, the worker).
pub async fn settings(db: &Db, project: ProjectId) -> Result<AutoMergeSettings, ForgeError> {
    const QUERY: &str = "auto_merge.settings";
    db.within(QUERY, async {
        let row: Option<SettingsRow> = sqlx::query_as(&settings_sql(""))
            .bind(project.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        parse_settings(project, row)
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

fn record(change: AutoMergeChange) -> ChangeRecord {
    let project = change.project();
    let details = AuditDetails::AutoMerge(change);
    ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: if project.is_some() {
                "project"
            } else {
                "instance"
            },
            resource_id: project.map(ProjectId::get),
            project_id: project,
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

/// Set `project`'s `merge.auto` to `mode` for `caller`, decided again on
/// the locked project (`decide`: forge-review's reconfirmed
/// `AutoMergeSettingsManage`). One `auto_merge.mode_changed` audit row;
/// [`ChangeOutcome::Unchanged`] writes nothing.
pub async fn set_mode_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    mode: AutoMergeMode,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(ChangeOutcome, AutoMergeSettings, P), ForgeError> {
    const ACTION: &str = "auto_merge.mode_changed";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock_shared(&mut tx, project).await?;
        let decided = decide(&locked_access_facts(&mut tx, project, caller, &locked).await?)?;
        let before = tx
            .step(|c| async move { settings_on(c, project).await })
            .await?;
        if before.mode == mode && before.stored {
            return Ok(Settle::Discard((ChangeOutcome::Unchanged, before, decided)));
        }
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.project_auto_merge (project_id, mode, mode_by, mode_at) \
                 VALUES ($1, $2, $3, now()) ON CONFLICT (project_id) DO UPDATE SET \
                 mode = EXCLUDED.mode, mode_by = EXCLUDED.mode_by, mode_at = now(), \
                 updated_at = now()",
            )
            .bind(project.get())
            .bind(mode.as_str())
            .bind(caller.get())
            .execute(c)
            .await
            .map_err(|e| query_error("auto_merge.set_mode", e))
            .map(drop)
        })
        .await?;
        let after = tx
            .step(|c| async move { settings_on(c, project).await })
            .await?;
        let change = AutoMergeChange::ModeChanged {
            project,
            from: before.mode,
            to: mode,
        };
        Ok(Settle::Commit(
            record(change),
            (ChangeOutcome::Changed, after, decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Turn `project`'s emergency stop on (`stopped`) or off for `caller`,
/// decided again on the locked project. One `auto_merge.stopped` /
/// `.resumed` audit row on the project.
pub async fn set_project_stop_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    stopped: bool,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(ChangeOutcome, AutoMergeSettings, P), ForgeError> {
    let action = if stopped {
        "auto_merge.stopped"
    } else {
        "auto_merge.resumed"
    };
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock_shared(&mut tx, project).await?;
        let decided = decide(&locked_access_facts(&mut tx, project, caller, &locked).await?)?;
        let before = tx
            .step(|c| async move { settings_on(c, project).await })
            .await?;
        if before.project_stop.is_some() == stopped {
            return Ok(Settle::Discard((ChangeOutcome::Unchanged, before, decided)));
        }
        // A project without a row keeps its default mode (`full`).
        let mode = before.mode;
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.project_auto_merge (project_id, mode, stopped_by, stopped_at) \
                 VALUES ($1, $2, CASE WHEN $3 THEN $4 END, CASE WHEN $3 THEN now() END) \
                 ON CONFLICT (project_id) DO UPDATE SET \
                 stopped_by = EXCLUDED.stopped_by, stopped_at = EXCLUDED.stopped_at, \
                 updated_at = now()",
            )
            .bind(project.get())
            .bind(mode.as_str())
            .bind(stopped)
            .bind(caller.get())
            .execute(c)
            .await
            .map_err(|e| query_error("auto_merge.set_project_stop", e))
            .map(drop)
        })
        .await?;
        let after = tx
            .step(|c| async move { settings_on(c, project).await })
            .await?;
        let project = Some(project);
        let change = if stopped {
            AutoMergeChange::Stopped { project }
        } else {
            AutoMergeChange::Resumed { project }
        };
        Ok(Settle::Commit(
            record(change),
            (ChangeOutcome::Changed, after, decided),
        ))
    }
    .await;
    tx.settle(action, outcome).await
}

/// Turn the instance's emergency stop on or off for `caller`, decided
/// again on the caller's locked account (`decide`). One
/// `auto_merge.stopped` / `.resumed` audit row on the instance. Returns
/// who / when it is stopped now (`None`: running).
pub async fn set_instance_stop_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    caller: UserId,
    stopped: bool,
    decide: impl FnOnce(&Account) -> Result<P, ForgeError>,
) -> Result<(ChangeOutcome, Option<StopMark>, P), ForgeError> {
    let action = if stopped {
        "auto_merge.stopped"
    } else {
        "auto_merge.resumed"
    };
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let account = tx
            .step(|c| async move { lock_account(c, caller).await })
            .await?;
        let decided = decide(&account)?;
        let changed: Option<(Option<i64>, Option<String>)> = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "UPDATE forge.auto_merge_instance SET \
                     stopped_by = CASE WHEN $1 THEN $2 END, \
                     stopped_at = CASE WHEN $1 THEN now() END, updated_at = now() \
                     WHERE id = 1 AND (stopped_at IS NOT NULL) <> $1 \
                     RETURNING stopped_by, to_char(stopped_at AT TIME ZONE 'UTC', {TS})"
                ))
                .bind(stopped)
                .bind(caller.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("auto_merge.set_instance_stop", e))
            })
            .await?;
        let mark = |(by, at): (Option<i64>, Option<String>)| {
            at.map(|at| StopMark {
                by: by.map(UserId::new),
                at,
            })
        };
        match changed {
            Some(row) => {
                let change = if stopped {
                    AutoMergeChange::Stopped { project: None }
                } else {
                    AutoMergeChange::Resumed { project: None }
                };
                Ok(Settle::Commit(
                    record(change),
                    (ChangeOutcome::Changed, mark(row), decided),
                ))
            }
            None => {
                let row: (Option<i64>, Option<String>) = tx
                    .step(|c| async move {
                        sqlx::query_as(&format!(
                            "SELECT stopped_by, to_char(stopped_at AT TIME ZONE 'UTC', {TS}) \
                             FROM forge.auto_merge_instance WHERE id = 1"
                        ))
                        .fetch_one(c)
                        .await
                        .map_err(|e| query_error("auto_merge.instance_read", e))
                    })
                    .await?;
                Ok(Settle::Discard((
                    ChangeOutcome::Unchanged,
                    mark(row),
                    decided,
                )))
            }
        }
    }
    .await;
    tx.settle(action, outcome).await
}

/// What auto-merge did with one merge request last, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredState {
    pub merge_request: MergeRequestId,
    pub project: ProjectId,
    pub head_sha: ObjectId,
    /// `queued` / `waiting` / `held` / `stopped`.
    pub state: String,
    pub class: Option<StopClass>,
    pub reason: String,
    pub detail: Option<String>,
    pub error_code: Option<String>,
    pub request_id: String,
    pub checked_at: String,
    pub changed_at: String,
}

type StateRow = (
    i64,
    i64,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
);

fn state_columns() -> String {
    format!(
        "merge_request_id, project_id, head_sha, state, class, reason, detail, error_code, \
         request_id, to_char(checked_at AT TIME ZONE 'UTC', {TS}), \
         to_char(changed_at AT TIME ZONE 'UTC', {TS})"
    )
}

fn parse_state(row: StateRow) -> Result<StoredState, ForgeError> {
    let (mr, project, head, state, class, reason, detail, code, request_id, checked, changed) = row;
    Ok(StoredState {
        merge_request: MergeRequestId::new(mr),
        project: ProjectId::new(project),
        head_sha: ObjectId::parse(&head)
            .map_err(|_| unreadable("stored_auto_merge_head_invalid"))?,
        state,
        class: class
            .map(|c| {
                StopClass::parse(&c).ok_or_else(|| unreadable("stored_auto_merge_class_invalid"))
            })
            .transpose()?,
        reason,
        detail,
        error_code: code,
        request_id,
        checked_at: checked,
        changed_at: changed,
    })
}

/// The stored state of merge request `mr` (`None`: auto-merge has not
/// looked at it, or its project does not use auto-merge).
pub async fn state_of(db: &Db, mr: MergeRequestId) -> Result<Option<StoredState>, ForgeError> {
    const QUERY: &str = "auto_merge.state_of";
    db.within(QUERY, async {
        let row: Option<StateRow> = sqlx::query_as(&format!(
            "SELECT {} FROM forge.auto_merge_states WHERE merge_request_id = $1",
            state_columns()
        ))
        .bind(mr.get())
        .fetch_optional(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        row.map(parse_state).transpose()
    })
    .await
}

/// An open merge request the worker looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub merge_request: MergeRequestId,
    pub project: ProjectId,
    pub iid: i64,
}

/// At most `limit` open merge requests of projects whose `merge.auto`
/// acts (no row = `full`), not pending deletion, with a head: those never
/// looked at, whose head changed, or last looked at more than `recheck`
/// ago — the oldest look first (a round robin).
pub async fn candidates(
    db: &Db,
    limit: i64,
    recheck: Duration,
) -> Result<Vec<Candidate>, ForgeError> {
    const QUERY: &str = "auto_merge.candidates";
    let recheck_ms = i64::try_from(recheck.as_millis()).unwrap_or(i64::MAX);
    db.within(QUERY, async {
        let rows: Vec<(i64, i64, i64)> = sqlx::query_as(
            "SELECT m.id, m.project_id, m.iid FROM forge.merge_requests m \
             JOIN forge.projects p ON p.id = m.project_id AND p.deleted_at IS NULL \
             LEFT JOIN forge.project_auto_merge a ON a.project_id = m.project_id \
             LEFT JOIN forge.auto_merge_states s ON s.merge_request_id = m.id \
             WHERE m.state = 'open' AND m.head_sha IS NOT NULL \
             AND coalesce(a.mode, 'full') IN ('conditional', 'full') \
             AND (s.merge_request_id IS NULL OR s.head_sha <> m.head_sha \
                  OR s.checked_at < now() - make_interval(secs => $2::double precision / 1000)) \
             ORDER BY s.checked_at NULLS FIRST, m.id LIMIT $1",
        )
        .bind(limit)
        .bind(recheck_ms)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(rows
            .into_iter()
            .map(|(mr, project, iid)| Candidate {
                merge_request: MergeRequestId::new(mr),
                project: ProjectId::new(project),
                iid,
            })
            .collect())
    })
    .await
}

/// A state the worker records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewState<'a> {
    pub merge_request: MergeRequestId,
    pub project: ProjectId,
    pub head_sha: &'a ObjectId,
    pub state: &'static str,
    pub class: Option<StopClass>,
    pub reason: &'a str,
    pub detail: Option<&'a str>,
    pub error_code: Option<ErrorCode>,
    pub request_id: &'a RequestId,
}

/// Record `new` (one row per merge request). Returns whether it changed
/// (another state, reason, class or head than stored): the worker logs a
/// stop once, when it starts.
pub async fn record_state(db: &Db, new: &NewState<'_>) -> Result<bool, ForgeError> {
    const QUERY: &str = "auto_merge.record";
    let detail = new
        .detail
        .map(|d| d.chars().take(255).collect::<String>())
        .filter(|d| !d.is_empty() && d.len() <= 255);
    db.within(QUERY, async {
        let changed: bool = sqlx::query_scalar(
            "WITH old AS (SELECT state, class, reason, head_sha FROM forge.auto_merge_states \
             WHERE merge_request_id = $1) \
             INSERT INTO forge.auto_merge_states (merge_request_id, project_id, head_sha, state, \
             class, reason, detail, error_code, request_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (merge_request_id) DO UPDATE SET head_sha = EXCLUDED.head_sha, \
             state = EXCLUDED.state, class = EXCLUDED.class, reason = EXCLUDED.reason, \
             detail = EXCLUDED.detail, error_code = EXCLUDED.error_code, \
             request_id = CASE WHEN (forge.auto_merge_states.state, \
               coalesce(forge.auto_merge_states.class, ''), forge.auto_merge_states.reason, \
               forge.auto_merge_states.head_sha) \
               IS DISTINCT FROM (EXCLUDED.state, coalesce(EXCLUDED.class, ''), EXCLUDED.reason, \
               EXCLUDED.head_sha) THEN EXCLUDED.request_id \
               ELSE forge.auto_merge_states.request_id END, \
             changed_at = CASE WHEN (forge.auto_merge_states.state, \
               coalesce(forge.auto_merge_states.class, ''), forge.auto_merge_states.reason, \
               forge.auto_merge_states.head_sha) \
               IS DISTINCT FROM (EXCLUDED.state, coalesce(EXCLUDED.class, ''), EXCLUDED.reason, \
               EXCLUDED.head_sha) THEN now() ELSE forge.auto_merge_states.changed_at END, \
             checked_at = now() \
             RETURNING NOT EXISTS (SELECT 1 FROM old o WHERE o.state = $4 \
               AND coalesce(o.class, '') = coalesce($5, '') AND o.reason = $6 AND o.head_sha = $3)",
        )
        .bind(new.merge_request.get())
        .bind(new.project.get())
        .bind(new.head_sha.as_str())
        .bind(new.state)
        .bind(new.class.map(StopClass::as_str))
        .bind(new.reason)
        .bind(detail)
        .bind(new.error_code.map(|c| c.id()))
        .bind(new.request_id.as_str())
        .fetch_one(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(changed)
    })
    .await
}

/// Forget the state of `mr` (its project's mode no longer acts).
pub async fn forget(db: &Db, mr: MergeRequestId) -> Result<(), ForgeError> {
    const QUERY: &str = "auto_merge.forget";
    db.within(QUERY, async {
        sqlx::query("DELETE FROM forge.auto_merge_states WHERE merge_request_id = $1")
            .bind(mr.get())
            .execute(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
            .map(drop)
    })
    .await
}

/// Forget at most `limit` states of merge requests that are no longer open
/// or whose project's mode no longer acts; how many.
pub async fn forget_inactive(db: &Db, limit: i64) -> Result<u64, ForgeError> {
    const QUERY: &str = "auto_merge.forget_inactive";
    db.within(QUERY, async {
        let done = sqlx::query(
            "DELETE FROM forge.auto_merge_states WHERE merge_request_id IN ( \
             SELECT s.merge_request_id FROM forge.auto_merge_states s \
             JOIN forge.merge_requests m ON m.id = s.merge_request_id \
             LEFT JOIN forge.project_auto_merge a ON a.project_id = s.project_id \
             WHERE m.state <> 'open' OR coalesce(a.mode, 'full') NOT IN ('conditional', 'full') \
             LIMIT $1)",
        )
        .bind(limit)
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(done.rows_affected())
    })
    .await
}

/// The summary of the `forge/risk` run on `head` of `project` (`None`: no
/// run): the auto-merge worker reads the `stop` rules it names.
pub async fn risk_summary(
    db: &Db,
    project: ProjectId,
    head: &ObjectId,
) -> Result<Option<String>, ForgeError> {
    const QUERY: &str = "auto_merge.risk_summary";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT summary FROM forge.check_runs WHERE project_id = $1 AND head_sha = $2 \
             AND name = $3",
        )
        .bind(project.get())
        .bind(head.as_str())
        .bind(forge_core::checks::RISK_CHECK)
        .fetch_optional(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}
