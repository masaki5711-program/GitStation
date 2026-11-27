//! The recommendation's LLM asks (plan 16 A11, ADR-0032 Amendment A11;
//! migration 20270113110000): `forge.agent_recommend_asks`, one row per
//! project.
//!
//! - **The route** (`POST …/agent-builder/recommend`, via
//!   forge-marketplace): [`upsert_ask`] on every read — a new row is
//!   `pending`; a row answered for another tip becomes `pending` again
//!   (`pending_since` now); `paused` is about the gateway, not a tip, and
//!   stays until the worker answers. A read moves `asked_at` only:
//!   `updated_at` is the answer's time, so re-reading never postpones a
//!   retry. The row as it is then is what the reader is told
//!   ([`forge_core::llm::agent_recommend::reasons_state`]).
//! - **The worker** (`agent_recommend`): [`recent_asks`] (asked within a
//!   window and wanting an answer, most recently read first) and
//!   [`record_answer`] (the state, the tip, the FG code, the advice).
//!
//! No input is stored here or anywhere (plan 19 §5).

use forge_core::llm::agent_recommend::{AskRecord, AskState};
use forge_core::ProjectId;
use forge_errors::{ErrorCode, ForgeError};

use crate::{query_error, read_query_error, Db};

type AskRow = (String, Option<String>, Option<String>, Option<i64>, f64);

fn record_of(row: AskRow) -> Result<AskRecord, ForgeError> {
    let (state, head_sha, error_code, advice_id, pending) = row;
    Ok(AskRecord {
        state: AskState::parse(&state).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "llm_stored_value_invalid")
                .with_context("column", "agent_recommend_asks.state")
        })?,
        head_sha,
        error_code,
        advice_id,
        // Whole seconds; a clock step back reads as 0.
        pending_secs: pending.max(0.0) as i64,
    })
}

const RETURNING: &str = "state, head_sha, error_code, advice_id, \
     extract(epoch FROM now() - pending_since)::float8";

/// Record a read of the recommendation of `project` at `tip` (`None`: an
/// empty repository) and return the ask as it is now (module docs).
pub async fn upsert_ask(
    db: &Db,
    project: ProjectId,
    tip: Option<&str>,
    request_id: &str,
) -> Result<AskRecord, ForgeError> {
    const QUERY: &str = "llm.agent_recommend.upsert_ask";
    let row: AskRow = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "INSERT INTO forge.agent_recommend_asks AS a (project_id, request_id) \
                 VALUES ($1, $3) \
                 ON CONFLICT (project_id) DO UPDATE SET \
                   asked_at = now(), \
                   request_id = EXCLUDED.request_id, \
                   pending_since = CASE WHEN a.state NOT IN ('pending', 'paused') \
                     AND a.head_sha IS DISTINCT FROM $2 THEN now() ELSE a.pending_since END, \
                   error_code = CASE WHEN a.state NOT IN ('pending', 'paused') \
                     AND a.head_sha IS DISTINCT FROM $2 THEN NULL ELSE a.error_code END, \
                   advice_id = CASE WHEN a.state NOT IN ('pending', 'paused') \
                     AND a.head_sha IS DISTINCT FROM $2 THEN NULL ELSE a.advice_id END, \
                   state = CASE WHEN a.state NOT IN ('pending', 'paused') \
                     AND a.head_sha IS DISTINCT FROM $2 THEN 'pending' ELSE a.state END, \
                   updated_at = CASE WHEN a.state NOT IN ('pending', 'paused') \
                     AND a.head_sha IS DISTINCT FROM $2 THEN now() ELSE a.updated_at END \
                 RETURNING {RETURNING}"
            ))
            .bind(project.get())
            .bind(tip)
            .bind(request_id)
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    record_of(row)
}

/// An ask the worker looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentAsk {
    pub project: ProjectId,
    pub ask: AskRecord,
    pub request_id: String,
    /// Seconds since the row was last written.
    pub updated_secs: i64,
}

/// The asks read within the last `within_secs` seconds that want an
/// answer — `pending`, `paused`, `ready` whose advice is gone, or
/// `unavailable` / `refused` last written `retry_secs` ago or more — the
/// most recently read first, at most `limit` (read pool). An answered ask
/// never takes the place of one that waits.
pub async fn recent_asks(
    db: &Db,
    within_secs: i64,
    retry_secs: i64,
    limit: i64,
) -> Result<Vec<RecentAsk>, ForgeError> {
    const QUERY: &str = "llm.agent_recommend.recent_asks";
    type Row = (
        i64,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        f64,
        f64,
    );
    let rows: Vec<Row> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT project_id, request_id, state, head_sha, error_code, advice_id, \
                   extract(epoch FROM now() - pending_since)::float8, \
                   extract(epoch FROM now() - updated_at)::float8 \
                 FROM forge.agent_recommend_asks \
                 WHERE asked_at > now() - make_interval(secs => $1) \
                   AND (state IN ('pending', 'paused') \
                     OR (state = 'ready' AND advice_id IS NULL) \
                     OR (state IN ('unavailable', 'refused') \
                       AND updated_at <= now() - make_interval(secs => $2))) \
                 ORDER BY asked_at DESC, project_id LIMIT $3",
            )
            .bind(within_secs as f64)
            .bind(retry_secs as f64)
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(
            |(project, request_id, state, head, code, advice, pending, updated)| {
                Ok(RecentAsk {
                    project: ProjectId::new(project),
                    ask: record_of((state, head, code, advice, pending))?,
                    request_id,
                    updated_secs: updated.max(0.0) as i64,
                })
            },
        )
        .collect()
}

/// What the worker answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer<'a> {
    pub state: AskState,
    pub head_sha: Option<&'a str>,
    pub error_code: Option<ErrorCode>,
    pub advice_id: Option<i64>,
}

/// Record the worker's answer for `project`; `false` when the row is gone
/// (the project was purged).
pub async fn record_answer(
    db: &Db,
    project: ProjectId,
    answer: &Answer<'_>,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "llm.agent_recommend.record_answer";
    let done = db
        .within(QUERY, async {
            sqlx::query(
                "UPDATE forge.agent_recommend_asks SET state = $2, head_sha = $3, \
                   error_code = $4, advice_id = $5, updated_at = now(), \
                   pending_since = CASE WHEN $2 = 'pending' AND state <> 'pending' \
                     THEN now() ELSE pending_since END \
                 WHERE project_id = $1",
            )
            .bind(project.get())
            .bind(answer.state.as_str())
            .bind(answer.head_sha)
            .bind(answer.error_code.map(|c| c.id()))
            .bind(answer.advice_id)
            .execute(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
#[path = "agent_recommend_tests.rs"]
mod tests;
