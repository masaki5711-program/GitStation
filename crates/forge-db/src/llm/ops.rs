//! A person's changes to the local LLM (plan 19 §8.2; migration
//! 20270114100000), each in one audited `ChangeTx`:
//!
//! - [`set_review_mode_decided`]: a project's `llm.review`
//!   (`forge.project_llm`), decided again on the locked project
//!   (`LlmSettingsManage`); `llm.review_changed` on the project.
//!   [`review_setting`] reads it with who changed it last.
//! - [`set_paused_decided`]: the instance's pause of the gateway
//!   (`forge.llm_gateway`, one row), decided again on the caller's locked
//!   account (`LlmPause` / `LlmResume`); `llm.paused` / `llm.resumed`.
//!   [`gateway_pause`] reads it (the route; the worker every 10 s).
//! - [`request_rerun`]: ask a merge request's first pass again
//!   (`AiReviewRerun`), decided again on the locked project, merge request
//!   and account: under the merge request's row lock, refused when
//!   it is not open, has no head or its project's `llm.review` is `off`
//!   (`FG-LLM-0008`), nothing written when the head still waits for its
//!   pass (`Pending`), limited to [`RERUNS_PER_HOUR`] accepted reruns per
//!   merge request in a rolling hour (`FG-WEB-0010
//!   route=ai_review_rerun`, `retry_after_s`). An accepted rerun is one
//!   `forge.llm_review_reruns` row (the count record the worker reads,
//!   `review::review_candidates`) and one `llm.review_rerun_requested`
//!   audit row on the project.
//! - [`feedback_totals`]: the verdicts per purpose
//!   (`forge_llm_advice_feedback_total`, the worker's metrics snapshot).

use forge_core::checks::AI_REVIEW_CHECK;
use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::llm::ops::{LlmChange, RERUNS_PER_HOUR, RERUN_WINDOW_SECS};
use forge_core::llm::ReviewMode;
use forge_core::{AuditDetails, MergeRequestState};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::stored_invalid;
use crate::access::{lock_account, Account};
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::review::auto_merge::ChangeOutcome;
use crate::review::commands::facts as merge_request_facts;
use crate::review::MergeRequestFacts;
use crate::{query_error, read_query_error, Db};

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

fn record(change: LlmChange) -> ChangeRecord {
    let project = match &change {
        LlmChange::ReviewMode { project, .. } | LlmChange::ReviewRerun { project, .. } => {
            Some(*project)
        }
        LlmChange::Gateway { .. } => None,
    };
    let details = AuditDetails::Llm(change);
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

// ── llm.review ──

/// A project's `llm.review` as stored (or the default, off).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSetting {
    pub project: ProjectId,
    pub mode: ReviewMode,
    /// Who changed it last (`None`: never changed).
    pub changed_by: Option<UserId>,
    /// When (UTC, RFC 3339).
    pub changed_at: Option<String>,
}

type SettingRow = (String, i64, String);

fn setting_sql(lock: &str) -> String {
    format!(
        "SELECT review, changed_by, to_char(changed_at AT TIME ZONE 'UTC', {TS}) \
         FROM forge.project_llm WHERE project_id = $1 {lock}"
    )
}

fn parse_setting(project: ProjectId, row: Option<SettingRow>) -> Result<ReviewSetting, ForgeError> {
    match row {
        Some((mode, by, at)) => Ok(ReviewSetting {
            project,
            mode: ReviewMode::parse(&mode).ok_or_else(|| stored_invalid("review"))?,
            changed_by: Some(UserId::new(by)),
            changed_at: Some(at),
        }),
        None => Ok(ReviewSetting {
            project,
            mode: ReviewMode::Off,
            changed_by: None,
            changed_at: None,
        }),
    }
}

async fn setting_on(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<ReviewSetting, ForgeError> {
    let row: Option<SettingRow> = sqlx::query_as(&setting_sql("FOR UPDATE"))
        .bind(project.get())
        .fetch_optional(conn)
        .await
        .map_err(|e| query_error("llm.review_setting_on", e))?;
    parse_setting(project, row)
}

/// `project`'s `llm.review` and who changed it last (read pool).
pub async fn review_setting(db: &Db, project: ProjectId) -> Result<ReviewSetting, ForgeError> {
    const QUERY: &str = "llm.review_setting";
    let row: Option<SettingRow> = db
        .within(QUERY, async {
            sqlx::query_as(&setting_sql(""))
                .bind(project.get())
                .fetch_optional(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    parse_setting(project, row)
}

/// Set `project`'s `llm.review` to `mode` for `caller`, decided again on
/// the locked project (`decide`: the caller's reconfirmed
/// `LlmSettingsManage`). One `llm.review_changed` audit row; already so
/// ([`ChangeOutcome::Unchanged`]) writes nothing. Turning it `off` removes
/// no advice, run or feedback.
pub async fn set_review_mode_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    mode: ReviewMode,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(ChangeOutcome, ReviewSetting, P), ForgeError> {
    const ACTION: &str = "llm.review_changed";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock_shared(&mut tx, project).await?;
        let decided = decide(&locked_access_facts(&mut tx, project, caller, &locked).await?)?;
        let before = tx
            .step(|c| async move { setting_on(c, project).await })
            .await?;
        // No row is `off`: setting `off` on a project without one writes
        // nothing.
        if before.mode == mode {
            return Ok(Settle::Discard((ChangeOutcome::Unchanged, before, decided)));
        }
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.project_llm (project_id, review, changed_by, changed_at) \
                 VALUES ($1, $2, $3, now()) ON CONFLICT (project_id) DO UPDATE SET \
                 review = EXCLUDED.review, changed_by = EXCLUDED.changed_by, changed_at = now()",
            )
            .bind(project.get())
            .bind(mode.as_str())
            .bind(caller.get())
            .execute(c)
            .await
            .map_err(|e| query_error("llm.set_review_mode", e))
            .map(drop)
        })
        .await?;
        let after = tx
            .step(|c| async move { setting_on(c, project).await })
            .await?;
        let change = LlmChange::ReviewMode {
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

// ── the gateway's pause ──

/// The instance's pause of the LLM gateway as stored.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GatewayPause {
    pub paused: bool,
    /// Who changed it last (`None`: never changed).
    pub changed_by: Option<UserId>,
    /// When (UTC, RFC 3339).
    pub changed_at: Option<String>,
}

type PauseRow = (bool, Option<i64>, Option<String>);

fn pause_sql(lock: &str) -> String {
    format!(
        "SELECT paused, changed_by, to_char(changed_at AT TIME ZONE 'UTC', {TS}) \
         FROM forge.llm_gateway WHERE id = 1 {lock}"
    )
}

fn parse_pause((paused, by, at): PauseRow) -> GatewayPause {
    GatewayPause {
        paused,
        changed_by: by.map(UserId::new),
        changed_at: at,
    }
}

/// The stored pause (read pool). The row is the migration's: a missing
/// one is `FG-SYS-0001 reason=llm_stored_value_invalid`, never "running".
pub async fn gateway_pause(db: &Db) -> Result<GatewayPause, ForgeError> {
    const QUERY: &str = "llm.gateway_pause";
    let row: Option<PauseRow> = db
        .within(QUERY, async {
            sqlx::query_as(&pause_sql(""))
                .fetch_optional(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(parse_pause)
        .ok_or_else(|| stored_invalid("llm_gateway"))
}

/// Pause (`paused`) or resume the gateway for `caller`, decided again on
/// the caller's locked account (`decide`). One `llm.paused` /
/// `llm.resumed` audit row on the instance; already so writes nothing.
pub async fn set_paused_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    caller: UserId,
    paused: bool,
    decide: impl FnOnce(&Account) -> Result<P, ForgeError>,
) -> Result<(ChangeOutcome, GatewayPause, P), ForgeError> {
    let action = if paused { "llm.paused" } else { "llm.resumed" };
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let account = tx
            .step(|c| async move { lock_account(c, caller).await })
            .await?;
        let decided = decide(&account)?;
        let before: Option<PauseRow> = tx
            .step(|c| async move {
                sqlx::query_as(&pause_sql("FOR UPDATE"))
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("llm.gateway_pause_on", e))
            })
            .await?;
        let before = parse_pause(before.ok_or_else(|| stored_invalid("llm_gateway"))?);
        if before.paused == paused {
            return Ok(Settle::Discard((ChangeOutcome::Unchanged, before, decided)));
        }
        let after: PauseRow = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "UPDATE forge.llm_gateway SET paused = $1, changed_by = $2, \
                     changed_at = now() WHERE id = 1 \
                     RETURNING paused, changed_by, to_char(changed_at AT TIME ZONE 'UTC', {TS})"
                ))
                .bind(paused)
                .bind(caller.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("llm.set_gateway_pause", e))
            })
            .await?;
        Ok(Settle::Commit(
            record(LlmChange::Gateway { paused }),
            (ChangeOutcome::Changed, parse_pause(after), decided),
        ))
    }
    .await;
    tx.settle(action, outcome).await
}

// ── asking the first pass again ──

/// What [`request_rerun`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rerun {
    /// Recorded: the worker asks the head again on its next pass.
    Queued {
        head_sha: String,
        /// Reruns left in the hour after this one.
        remaining: i64,
    },
    /// Nothing written or counted: the head still waits for its pass (no
    /// run yet, a run not completed, or an earlier rerun not picked up).
    Pending { head_sha: String },
}

fn refused(reason: &'static str, project: ProjectId, iid: i64) -> ForgeError {
    ForgeError::new(ErrorCode::AiReviewRerunRefused)
        .with_context("reason", reason)
        .with_context("project_id", project.get())
        .with_context("iid", iid)
}

/// Ask merge request `iid` of `project` to be reviewed again for `caller`
/// (module docs), decided again on the locked project, merge request and
/// account (`decide`: the caller's reconfirmed `AiReviewRerun`). The
/// record's time is the insert's own (`clock_timestamp()`), taken after the
/// check that the head's run is completed.
/// Errors (nothing written): `decide`'s, `FG-LLM-0008` (`not_open`,
/// `no_head`, `review_off`), `FG-WEB-0010` (`route=ai_review_rerun`,
/// `retry_after_s`), `FG-REPO-0005` (the merge request went away),
/// `FG-DB-*`.
pub async fn request_rerun<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    caller: UserId,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(Rerun, P), ForgeError> {
    const ACTION: &str = "llm.review_rerun_requested";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        // The project, the merge request (`FOR UPDATE`: two presses count
        // one after the other) and the caller, locked; decided again.
        let facts = merge_request_facts(&mut tx, project, Some(iid), caller).await?;
        let decided = decide(&facts)?;
        let Some(stored) = facts.merge_request else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "merge_request")
                .with_context("project_id", project.get())
                .with_context("iid", iid));
        };
        let mr = stored.id.get();
        if stored.state != MergeRequestState::Open {
            return Err(refused("not_open", project, iid));
        }
        let Some(head) = stored.head_sha else {
            return Err(refused("no_head", project, iid));
        };
        let mode = tx
            .step(|c| async move { setting_on(c, project).await })
            .await?
            .mode;
        if mode == ReviewMode::Off {
            return Err(refused("review_off", project, iid));
        }
        let h = head.clone(); // clone: the step owns its binds
        let waiting: bool = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT NOT EXISTS ( \
                       SELECT 1 FROM forge.check_runs c \
                       WHERE c.project_id = $1 AND c.head_sha = $2 AND c.name = $3 \
                         AND c.status = 'completed' \
                         AND NOT EXISTS ( \
                           SELECT 1 FROM forge.llm_review_reruns r \
                           WHERE r.merge_request_id = $4 AND r.head_sha = c.head_sha \
                             AND r.requested_at > c.updated_at))",
                )
                .bind(project.get())
                .bind(h)
                .bind(AI_REVIEW_CHECK)
                .bind(mr)
                .fetch_one(c)
                .await
                .map_err(|e| query_error("llm.rerun_waiting", e))
            })
            .await?;
        if waiting {
            return Ok(Settle::Discard((
                Rerun::Pending { head_sha: head },
                decided,
            )));
        }
        let (count, retry_after): (i64, Option<f64>) = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT count(*), \
                       EXTRACT(EPOCH FROM (min(requested_at) \
                         + make_interval(secs => $2) - now()))::float8 \
                     FROM forge.llm_review_reruns \
                     WHERE merge_request_id = $1 \
                       AND requested_at > now() - make_interval(secs => $2)",
                )
                .bind(mr)
                .bind(RERUN_WINDOW_SECS as f64)
                .fetch_one(c)
                .await
                .map_err(|e| query_error("llm.rerun_count", e))
            })
            .await?;
        if count >= RERUNS_PER_HOUR {
            let secs = retry_after.map_or(1, |s| s.ceil().max(1.0) as i64);
            return Err(ForgeError::new(ErrorCode::RateLimited)
                .with_context("route", "ai_review_rerun")
                .with_context("limit", RERUNS_PER_HOUR)
                .with_context("window_s", RERUN_WINDOW_SECS)
                .with_context("retry_after_s", secs)
                .with_context("project_id", project.get())
                .with_context("iid", iid));
        }
        let (h, request_id) = (head.clone(), ctx.request_id.as_str().to_owned()); // clone: the step owns its binds
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.llm_review_reruns \
                   (merge_request_id, project_id, head_sha, requested_by, request_id, \
                    requested_at) \
                 VALUES ($1, $2, $3, $4, $5, clock_timestamp())",
            )
            .bind(mr)
            .bind(project.get())
            .bind(h)
            .bind(caller.get())
            .bind(request_id)
            .execute(c)
            .await
            .map_err(|e| query_error("llm.rerun_insert", e))
            .map(drop)
        })
        .await?;
        let change = LlmChange::ReviewRerun {
            project,
            merge_request: MergeRequestId::new(mr),
            iid,
            head_sha: head.clone(), // clone: the record and the answer both name it
        };
        Ok(Settle::Commit(
            record(change),
            (
                Rerun::Queued {
                    head_sha: head,
                    remaining: RERUNS_PER_HOUR - count - 1,
                },
                decided,
            ),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Where the first pass of a merge request's head stands for a rerun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerunState {
    /// No completed `forge/ai-review` run on the head (none yet, or one
    /// waiting): nothing to repeat.
    NotDone,
    /// Its run is completed and no rerun waits: it may be asked again.
    Possible,
    /// A rerun is newer than its completed run: the worker asks it again
    /// on its next pass.
    Asked,
}

/// [`RerunState`] of head `head` of merge request `mr` (read pool; a hint
/// for the read — the limit is the rerun's, which answers 429).
pub async fn rerun_state(
    db: &Db,
    project: ProjectId,
    mr: MergeRequestId,
    head: &str,
) -> Result<RerunState, ForgeError> {
    const QUERY: &str = "llm.rerun_state";
    let row: Option<bool> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT EXISTS (                    SELECT 1 FROM forge.llm_review_reruns r                    WHERE r.merge_request_id = $4 AND r.head_sha = c.head_sha                      AND r.requested_at > c.updated_at)                  FROM forge.check_runs c                  WHERE c.project_id = $1 AND c.head_sha = $2 AND c.name = $3                    AND c.status = 'completed'",
            )
            .bind(project.get())
            .bind(head)
            .bind(AI_REVIEW_CHECK)
            .bind(mr.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(match row {
        None => RerunState::NotDone,
        Some(false) => RerunState::Possible,
        Some(true) => RerunState::Asked,
    })
}

// ── metrics ──

/// The verdicts given so far, per purpose and verdict (rows are never
/// deleted, so a count is a total; a person who changes their verdict
/// moves one from one label to the other).
pub async fn feedback_totals(db: &Db) -> Result<Vec<(String, String, u64)>, ForgeError> {
    const QUERY: &str = "llm.feedback_totals";
    let rows: Vec<(String, String, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT purpose, verdict, count(*) FROM forge.llm_advice_feedback \
                 GROUP BY purpose, verdict ORDER BY purpose, verdict",
            )
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(p, v, n)| (p, v, u64::try_from(n).unwrap_or(0)))
        .collect())
}

#[cfg(test)]
#[path = "ops_tests.rs"]
mod tests;
