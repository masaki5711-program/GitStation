//! The local LLM's records (plan 19 §8.3, ADR-0039 §3; migration
//! 20270108100000): `forge.llm_requests`, `forge.llm_advice`,
//! `forge.llm_advice_feedback` and a project's `llm.review`
//! (`forge.project_llm`).
//!
//! - **The gateway** (forge-llm, in the worker): [`insert_request`] when a
//!   request is queued, [`mark_running`], [`finish`] (succeeded / failed /
//!   abandoned with its FG code), [`insert_advice`] (an older advice of the
//!   same subject and purpose is marked superseded in the same
//!   transaction), [`abandon_stale`] (a dead worker's live rows),
//!   [`sweep_retention`].
//! - **The arbitration**: [`interactive_active`] (the `gpu-eval` claim
//!   waits for it, plan 19 §1.1), [`gpu_eval_running`] (batch work waits
//!   for the bench).
//! - **`forge doctor`**: [`queue_health`] (`llm.queue_age`, plan 19 §8.4).
//! - **People**: [`record_feedback_decided`], [`advice_decided`] (one verdict per advice, finding and
//!   person; changing it updates the row), [`review_mode`]. Reads and
//!   votes are `*_decided`: the store loads the advice's own subject and
//!   the caller's `decide` (its `authorize()`) runs on exactly that, so a
//!   decision on one subject can never open another subject's advice.
//!
//! The input is never stored: a request row has its SHA-256 and class
//! only. The advice's class is at least the request's and never lowered
//! (the table's trigger `llm_advice_guarded`).

use forge_core::llm::{InputClass, Priority, Purpose, ReviewMode, SubjectKind, Verdict};
use forge_core::{ProjectId, RequestId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use crate::{query_error, read_query_error, Db};

pub mod asset_draft;
pub mod diagnosis;
pub mod review;

/// Requests kept this long (plan 19 §8.3).
pub const REQUEST_RETENTION_DAYS: i32 = 90;
/// Advice kept this long.
pub const ADVICE_RETENTION_DAYS: i32 = 180;
/// The most rows one sweep deletes from each table.
pub const SWEEP_BATCH: i64 = 1_000;
/// A live interactive request older than this no longer holds the
/// `gpu-eval` claim back (its deadline is 2 min; a crashed worker's row
/// must not hold the GPU).
pub const INTERACTIVE_STALE_SECS: i64 = 15 * 60;

/// A request as queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRequest {
    pub purpose: Purpose,
    pub priority: Priority,
    pub subject_kind: SubjectKind,
    pub subject_id: Option<i64>,
    pub project: Option<ProjectId>,
    pub requested_by: Option<UserId>,
    /// `name:tag`.
    pub model: String,
    /// 64 hex, as checked at start.
    pub model_digest: Option<String>,
    pub prompt_id: String,
    pub prompt_version: i32,
    pub input_sha256: String,
    pub input_class: InputClass,
    pub request_id: RequestId,
}

/// Queue a request; its id.
pub async fn insert_request(db: &Db, r: &NewRequest) -> Result<i64, ForgeError> {
    const QUERY: &str = "llm.insert_request";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "INSERT INTO forge.llm_requests (purpose, priority, subject_kind, subject_id, \
               project_id, requested_by, model, model_digest, prompt_id, prompt_version, \
               input_sha256, input_class, request_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING id",
        )
        .bind(r.purpose.as_str())
        .bind(r.priority.as_str())
        .bind(r.subject_kind.as_str())
        .bind(r.subject_id)
        .bind(r.project.map(ProjectId::get))
        .bind(r.requested_by.map(UserId::get))
        .bind(&r.model)
        .bind(&r.model_digest)
        .bind(&r.prompt_id)
        .bind(r.prompt_version)
        .bind(&r.input_sha256)
        .bind(r.input_class.as_str())
        .bind(r.request_id.as_str())
        .fetch_one(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// `queued` → `running`; `false` when the row is not queued any more.
pub async fn mark_running(db: &Db, id: i64) -> Result<bool, ForgeError> {
    const QUERY: &str = "llm.mark_running";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.llm_requests SET state = 'running', started_at = now() \
             WHERE id = $1 AND state = 'queued'",
        )
        .bind(id)
        .execute(db.write_pool())
        .await
        .map(|r| r.rows_affected() == 1)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// How a request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Succeeded,
    /// A model or check failure, with its code.
    Failed(ErrorCode),
    /// Given up before running (deadline, pause, restart), with its code.
    Abandoned(ErrorCode),
}

impl Ending {
    fn state(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed(_) => "failed",
            Self::Abandoned(_) => "abandoned",
        }
    }

    fn code(self) -> Option<&'static str> {
        match self {
            Self::Succeeded => None,
            Self::Failed(c) | Self::Abandoned(c) => Some(c.id()),
        }
    }
}

/// What a finished request cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub attempts: i16,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
}

/// End a live request; `false` when it had already ended.
pub async fn finish(db: &Db, id: i64, ending: Ending, usage: Usage) -> Result<bool, ForgeError> {
    const QUERY: &str = "llm.finish";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.llm_requests SET state = $2, error_code = $3, attempts = $4, \
               input_tokens = $5, output_tokens = $6, finished_at = now(), \
               started_at = CASE WHEN $2 = 'succeeded' THEN coalesce(started_at, now()) \
                                 ELSE started_at END \
             WHERE id = $1 AND state IN ('queued', 'running')",
        )
        .bind(id)
        .bind(ending.state())
        .bind(ending.code())
        .bind(usage.attempts)
        .bind(usage.input_tokens)
        .bind(usage.output_tokens)
        .execute(db.write_pool())
        .await
        .map(|r| r.rows_affected() == 1)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// A live request older than this cannot still be waited for (the batch
/// deadline, 6 h, plus a model call): a worker that died left it.
pub const STALE_LIVE_SECS: i64 = 7 * 60 * 60;

/// Every live request queued more than `older_than_secs` ago ends
/// `abandoned` with `code` (a worker that died left it; the purpose's job
/// asks again). Live requests of running workers are younger. How many.
pub async fn abandon_stale(
    db: &Db,
    code: ErrorCode,
    older_than_secs: i64,
) -> Result<u64, ForgeError> {
    const QUERY: &str = "llm.abandon_stale";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.llm_requests SET state = 'abandoned', error_code = $1, \
               finished_at = now() WHERE state IN ('queued', 'running') \
               AND queued_at < now() - make_interval(secs => $2)",
        )
        .bind(code.id())
        .bind(older_than_secs as f64)
        .execute(db.write_pool())
        .await
        .map(|r| r.rows_affected())
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// An advice as written (its findings as the JSON array the checks kept).
#[derive(Debug, Clone, PartialEq)]
pub struct NewAdvice {
    pub llm_request_id: Option<i64>,
    pub purpose: Purpose,
    pub subject_kind: SubjectKind,
    pub subject_id: Option<i64>,
    pub project: Option<ProjectId>,
    /// The head the advice is about (a merge request, an approval).
    pub head_sha: Option<String>,
    pub summary: String,
    pub findings: serde_json::Value,
    pub deep_review: Option<String>,
    pub dropped_findings: i32,
    /// The summary was withheld or findings were dropped for evidence.
    pub degraded: bool,
    pub input_class: InputClass,
    pub model: String,
    pub prompt_version: i32,
}

/// Write an advice; every older live advice of the same subject and
/// purpose is marked superseded by it. Its id. Two writers of the same
/// subject and purpose take turns (a transaction-scoped advisory lock),
/// so the newest always supersedes the older ones and never the reverse.
pub async fn insert_advice(db: &Db, a: &NewAdvice) -> Result<i64, ForgeError> {
    const QUERY: &str = "llm.insert_advice";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended( \
               'llm_advice:' || $1 || ':' || coalesce($2::text, '') || ':' || $3, 0))",
        )
        .bind(a.subject_kind.as_str())
        .bind(a.subject_id)
        .bind(a.purpose.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO forge.llm_advice (llm_request_id, purpose, subject_kind, subject_id, \
               project_id, head_sha, summary, findings, deep_review, dropped_findings, \
               degraded, input_class, model, prompt_version) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING id",
        )
        .bind(a.llm_request_id)
        .bind(a.purpose.as_str())
        .bind(a.subject_kind.as_str())
        .bind(a.subject_id)
        .bind(a.project.map(ProjectId::get))
        .bind(&a.head_sha)
        .bind(&a.summary)
        .bind(&a.findings)
        .bind(&a.deep_review)
        .bind(a.dropped_findings)
        .bind(a.degraded)
        .bind(a.input_class.as_str())
        .bind(&a.model)
        .bind(a.prompt_version)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(
            "UPDATE forge.llm_advice SET superseded_by = $1 \
             WHERE subject_kind = $2 AND subject_id IS NOT DISTINCT FROM $3 AND purpose = $4 \
               AND id < $1 AND superseded_by IS NULL",
        )
        .bind(id)
        .bind(a.subject_kind.as_str())
        .bind(a.subject_id)
        .bind(a.purpose.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(id)
    })
    .await
}

/// What a reader of an advice turned out to be, as the caller's decision
/// on the advice's own subject says (L5-4: the route's `authorize()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderKind {
    /// A person (a web session or a person's token).
    Person,
    /// An agent token or a delegated (act-as) tool.
    Agent,
}

/// The subject of a stored advice, as the store loaded it: what the
/// caller's `decide` closure authorizes. The store builds it from the row
/// itself, so a decision is always about this advice's own subject — a
/// caller cannot pair a decision on one subject with another's advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdviceSubject {
    pub advice_id: i64,
    pub kind: SubjectKind,
    /// `None` for an incident diagnosis (no row of its own).
    pub id: Option<i64>,
    pub project: Option<ProjectId>,
    pub input_class: InputClass,
}

/// A stored advice (read pool).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredAdvice {
    pub id: i64,
    pub llm_request_id: Option<i64>,
    pub purpose: Purpose,
    pub subject_kind: SubjectKind,
    pub subject_id: Option<i64>,
    pub head_sha: Option<String>,
    pub summary: String,
    pub findings: serde_json::Value,
    pub deep_review: Option<String>,
    pub degraded: bool,
    pub input_class: InputClass,
    pub superseded_by: Option<i64>,
    /// What labels it on a screen (plan 19 §4): the model (`name:tag`),
    /// the prompt version and when it was made (UTC,
    /// `YYYY-MM-DDTHH:MM:SSZ`).
    pub model: String,
    pub prompt_version: i32,
    pub created_at: String,
}

/// What a reader gets for an advice.
#[derive(Debug, Clone, PartialEq)]
pub enum AdviceView {
    Shown(Box<StoredAdvice>),
    /// A confidential advice asked for by an agent: only that it exists
    /// ("機密の入力のため表示しません"), never its words.
    Withheld {
        id: i64,
        input_class: InputClass,
    },
}

type AdviceRow = (
    i64,
    Option<i64>,
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<String>,
    String,
    serde_json::Value,
    Option<String>,
    bool,
    String,
    Option<i64>,
    String,
    i32,
    String,
);

fn stored_invalid(column: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "llm_stored_value_invalid")
        .with_context("column", column)
}

fn subject_of(
    id: i64,
    kind: &str,
    subject: Option<i64>,
    project: Option<i64>,
    class: &str,
) -> Result<AdviceSubject, ForgeError> {
    Ok(AdviceSubject {
        advice_id: id,
        kind: SubjectKind::parse(kind).ok_or_else(|| stored_invalid("subject_kind"))?,
        id: subject,
        project: project.map(ProjectId::new),
        input_class: InputClass::parse(class).ok_or_else(|| stored_invalid("input_class"))?,
    })
}

/// The advice `id`, read only after `decide` allowed the reader on the
/// advice's own subject ([`AdviceSubject`], loaded by the store). `None`:
/// no such advice (or swept). `decide`'s error is returned as it is (the
/// route's "not found" for a subject the reader cannot see, so existence
/// does not leak). A confidential advice is [`AdviceView::Withheld`] from
/// an agent.
pub async fn advice_decided(
    db: &Db,
    id: i64,
    decide: impl FnOnce(&AdviceSubject) -> Result<ReaderKind, ForgeError>,
) -> Result<Option<AdviceView>, ForgeError> {
    const QUERY: &str = "llm.advice";
    let row: Option<AdviceRow> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT id, llm_request_id, purpose, subject_kind, subject_id, project_id, \
                   head_sha, summary, findings, deep_review, degraded, input_class, superseded_by, \
                   model, prompt_version, \
                   to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 FROM forge.llm_advice WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let Some((
        id,
        req,
        purpose,
        kind,
        subject,
        project,
        head,
        summary,
        findings,
        deep,
        degraded,
        class,
        sup,
        model,
        prompt_version,
        created_at,
    )) = row
    else {
        return Ok(None);
    };
    let decided = subject_of(id, &kind, subject, project, &class)?;
    let reader = decide(&decided)?;
    if reader == ReaderKind::Agent && !decided.input_class.shown_to_agents() {
        return Ok(Some(AdviceView::Withheld {
            id,
            input_class: decided.input_class,
        }));
    }
    Ok(Some(AdviceView::Shown(Box::new(StoredAdvice {
        id,
        llm_request_id: req,
        purpose: Purpose::parse(&purpose).ok_or_else(|| stored_invalid("purpose"))?,
        subject_kind: decided.kind,
        subject_id: subject,
        head_sha: head,
        summary,
        findings,
        deep_review: deep,
        degraded,
        input_class: decided.input_class,
        superseded_by: sup,
        model,
        prompt_version,
        created_at,
    }))))
}

/// What [`record_feedback_decided`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackOutcome {
    Recorded,
    /// The person's earlier verdict on the same finding was changed.
    Changed,
    /// The same verdict was already there.
    Unchanged,
    /// No such advice (or swept), or no such finding.
    NoAdvice,
    /// An agent token or a delegated tool: feedback is people's only (it
    /// is the measure of the advice; plan 19 §4).
    NotAllowed,
}

/// A person's verdict on `finding` (`None`: the advice as a whole) of an
/// advice, written only after `decide` allowed the voter on the advice's
/// own subject ([`AdviceSubject`], loaded by the store; its error is
/// returned as it is). One statement writes it, so a concurrent vote of
/// the same person is never reported as recorded twice: a new row is
/// `Recorded`, a changed verdict `Changed`, the same verdict `Unchanged`.
pub async fn record_feedback_decided(
    db: &Db,
    advice_id: i64,
    finding: Option<u8>,
    user: UserId,
    verdict: Verdict,
    decide: impl FnOnce(&AdviceSubject) -> Result<ReaderKind, ForgeError>,
) -> Result<FeedbackOutcome, ForgeError> {
    const QUERY: &str = "llm.record_feedback";
    let index: i16 = finding.map_or(-1, i16::from);
    let found: Option<(String, Option<i64>, Option<i64>, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT subject_kind, subject_id, project_id, input_class \
                 FROM forge.llm_advice WHERE id = $1",
            )
            .bind(advice_id)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let Some((kind, subject, project, class)) = found else {
        return Ok(FeedbackOutcome::NoAdvice);
    };
    let decided = subject_of(advice_id, &kind, subject, project, &class)?;
    if decide(&decided)? == ReaderKind::Agent {
        return Ok(FeedbackOutcome::NotAllowed);
    }
    db.within(QUERY, async {
        // `RETURNING (xmax = 0)`: PostgreSQL leaves xmax 0 on a row this
        // statement inserted and sets it (to this transaction) on a row the
        // ON CONFLICT branch updated — true = Recorded, false = Changed. No
        // row: the advice (still of the decided subject) or the finding is
        // not there, or the verdict was already this one (the DO UPDATE's
        // WHERE).
        let written: Option<bool> = sqlx::query_scalar(
            "INSERT INTO forge.llm_advice_feedback (advice_id, purpose, input_class, \
               finding_index, user_id, verdict) \
             SELECT a.id, a.purpose, a.input_class, $2, $3, $4 FROM forge.llm_advice a \
              WHERE a.id = $1 AND a.subject_kind = $5 AND a.subject_id IS NOT DISTINCT FROM $6 \
                AND ($2 < 0 OR $2 < jsonb_array_length(a.findings)) \
             ON CONFLICT (advice_id, finding_index, user_id) WHERE advice_id IS NOT NULL \
             DO UPDATE SET verdict = EXCLUDED.verdict, updated_at = now() \
               WHERE forge.llm_advice_feedback.verdict IS DISTINCT FROM EXCLUDED.verdict \
             RETURNING (xmax = 0)",
        )
        .bind(advice_id)
        .bind(index)
        .bind(user.get())
        .bind(verdict.as_str())
        .bind(decided.kind.as_str())
        .bind(decided.id)
        .fetch_optional(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?;
        match written {
            Some(true) => Ok(FeedbackOutcome::Recorded),
            Some(false) => Ok(FeedbackOutcome::Changed),
            None => {
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM forge.llm_advice_feedback \
                     WHERE advice_id = $1 AND finding_index = $2 AND user_id = $3)",
                )
                .bind(advice_id)
                .bind(index)
                .bind(user.get())
                .fetch_one(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))?;
                Ok(if exists {
                    FeedbackOutcome::Unchanged
                } else {
                    FeedbackOutcome::NoAdvice
                })
            }
        }
    })
    .await
}

/// A project's `llm.review` (no row = off).
pub async fn review_mode(db: &Db, project: ProjectId) -> Result<ReviewMode, ForgeError> {
    const QUERY: &str = "llm.review_mode";
    let stored: Option<String> = db
        .within(QUERY, async {
            sqlx::query_scalar("SELECT review FROM forge.project_llm WHERE project_id = $1")
                .bind(project.get())
                .fetch_optional(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    match stored {
        None => Ok(ReviewMode::Off),
        Some(s) => ReviewMode::parse(&s).ok_or_else(|| stored_invalid("review")),
    }
}

/// The newest advice of a subject and purpose, with what labels it on a
/// screen (plan 19 §4: the model, the prompt version, when) and the
/// SHA-256 of the input it was made from (`None`: its request was swept),
/// so a reader can tell whether the facts it summarized are still the
/// facts (M5 L5-8). Its words are read with [`advice_decided`] only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdviceLabel {
    pub id: i64,
    pub llm_request_id: Option<i64>,
    pub head_sha: Option<String>,
    pub input_sha256: Option<String>,
    pub model: String,
    pub prompt_version: i32,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub created_at: String,
}

/// The newest advice of `purpose` about `kind` / `subject` (`None`: none).
pub async fn latest_advice(
    db: &Db,
    kind: SubjectKind,
    subject: i64,
    purpose: Purpose,
) -> Result<Option<AdviceLabel>, ForgeError> {
    const QUERY: &str = "llm.latest_advice";
    type Row = (
        i64,
        Option<i64>,
        Option<String>,
        Option<String>,
        String,
        i32,
        String,
    );
    let row: Option<Row> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT a.id, a.llm_request_id, a.head_sha, r.input_sha256, a.model, \
                 a.prompt_version, \
                 to_char(a.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 FROM forge.llm_advice a \
                 LEFT JOIN forge.llm_requests r ON r.id = a.llm_request_id \
                 WHERE a.subject_kind = $1 AND a.subject_id = $2 AND a.purpose = $3 \
                 ORDER BY a.id DESC LIMIT 1",
            )
            .bind(kind.as_str())
            .bind(subject)
            .bind(purpose.as_str())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(
        |(id, llm_request_id, head_sha, input_sha256, model, prompt_version, created_at)| {
            AdviceLabel {
                id,
                llm_request_id,
                head_sha,
                input_sha256,
                model,
                prompt_version,
                created_at,
            }
        },
    ))
}

/// The requests already made of `purpose` about `kind` / `subject` with
/// one input (its SHA-256): what a feature reads before asking again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AskedBefore {
    /// Queued, running or succeeded: asked, or answered.
    pub live_or_answered: i64,
    /// Failed or abandoned within the last `recent_secs`.
    pub failed_recently: i64,
    /// Every request of this input.
    pub total: i64,
}

/// [`AskedBefore`] of one input.
pub async fn asked_before(
    db: &Db,
    purpose: Purpose,
    kind: SubjectKind,
    subject: i64,
    input_sha256: &str,
    recent_secs: i64,
) -> Result<AskedBefore, ForgeError> {
    const QUERY: &str = "llm.asked_before";
    let (live_or_answered, failed_recently, total): (i64, i64, i64) = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT count(*) FILTER (WHERE state IN ('queued', 'running', 'succeeded')), \
                 count(*) FILTER (WHERE state IN ('failed', 'abandoned') \
                 AND finished_at > now() - make_interval(secs => $5)), \
                 count(*) \
                 FROM forge.llm_requests \
                 WHERE purpose = $1 AND subject_kind = $2 AND subject_id = $3 \
                 AND input_sha256 = $4",
            )
            .bind(purpose.as_str())
            .bind(kind.as_str())
            .bind(subject)
            .bind(input_sha256)
            .bind(recent_secs as f64)
            .fetch_one(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(AskedBefore {
        live_or_answered,
        failed_recently,
        total,
    })
}

/// The newest request of `purpose` about `kind` / `subject` with one input:
/// its id and FG code (`None`: none). A feature that got the gateway's
/// error back names the request with it (its own event, M5 L5-8).
pub async fn last_request(
    db: &Db,
    purpose: Purpose,
    kind: SubjectKind,
    subject: i64,
    input_sha256: &str,
) -> Result<Option<(i64, Option<String>)>, ForgeError> {
    const QUERY: &str = "llm.last_request";
    db.within(QUERY, async {
        sqlx::query_as(
            "SELECT id, error_code FROM forge.llm_requests WHERE purpose = $1 \
             AND subject_kind = $2 AND subject_id = $3 AND input_sha256 = $4 \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(purpose.as_str())
        .bind(kind.as_str())
        .bind(subject)
        .bind(input_sha256)
        .fetch_optional(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// Whether an interactive request is queued or running (younger than
/// [`INTERACTIVE_STALE_SECS`]): the `gpu-eval` claim waits for it.
pub async fn interactive_active(db: &Db) -> Result<bool, ForgeError> {
    const QUERY: &str = "llm.interactive_active";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.llm_requests \
             WHERE state IN ('queued', 'running') AND priority = 'interactive' \
               AND queued_at > now() - make_interval(secs => $1))",
        )
        .bind(INTERACTIVE_STALE_SECS as f64)
        .fetch_one(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// Whether a `gpu-eval` job is running (batch inference waits for it).
pub async fn gpu_eval_running(db: &Db) -> Result<bool, ForgeError> {
    const QUERY: &str = "llm.gpu_eval_running";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.ci_jobs \
             WHERE status = 'running' AND $1 = ANY(tags))",
        )
        .bind(crate::ci_runners::lease::GPU_EVAL_TAG)
        .fetch_one(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// How long ago a request that gave up in the queue counts for
/// [`QueueHealth::deadline_misses`].
pub const DEADLINE_MISS_WINDOW_SECS: i64 = 60 * 60;

/// The live queue as the records show it (`forge doctor` `llm.queue_age`,
/// plan 19 §8.4). The queue itself is in the worker's memory; its rows
/// are written when a request is queued, starts and ends.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueueHealth {
    /// Requests `queued` now.
    pub queued: i64,
    /// Requests `running` now.
    pub running: i64,
    /// Per tier (as submitted: its deadline is that tier's), how long the
    /// oldest `queued` request has waited, in seconds. Tiers with nothing
    /// queued are absent; highest tier first.
    pub oldest_queued_s: Vec<(Priority, i64)>,
    /// Requests that ended `FG-LLM-0004` (the deadline passed in the queue)
    /// in the last [`DEADLINE_MISS_WINDOW_SECS`].
    pub deadline_misses: i64,
}

/// Read [`QueueHealth`] (the read pool).
pub async fn queue_health(db: &Db) -> Result<QueueHealth, ForgeError> {
    const QUERY: &str = "llm.queue_health";
    db.within(QUERY, async {
        let tiers: Vec<(String, i64, i64, Option<i64>)> = sqlx::query_as(
            "SELECT priority,                count(*) FILTER (WHERE state = 'queued'),                count(*) FILTER (WHERE state = 'running'),                floor(extract(epoch FROM now() - min(queued_at)                  FILTER (WHERE state = 'queued')))::bigint              FROM forge.llm_requests WHERE state IN ('queued', 'running')              GROUP BY priority",
        )
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        // A request that ended within the window was queued at most the
        // stale bound before it: the queued_at index bounds the scan.
        let deadline_misses: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM forge.llm_requests              WHERE queued_at > now() - make_interval(secs => $1)                AND state = 'abandoned' AND error_code = $2                AND finished_at > now() - make_interval(secs => $3)",
        )
        .bind((STALE_LIVE_SECS + DEADLINE_MISS_WINDOW_SECS) as f64)
        .bind(ErrorCode::LlmQueueDeadline.id())
        .bind(DEADLINE_MISS_WINDOW_SECS as f64)
        .fetch_one(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let mut health = QueueHealth {
            deadline_misses,
            ..QueueHealth::default()
        };
        for (priority, queued, running, oldest) in tiers {
            let priority = Priority::parse(&priority).ok_or_else(|| stored_invalid("priority"))?;
            health.queued += queued;
            health.running += running;
            if let Some(age) = oldest {
                health.oldest_queued_s.push((priority, age.max(0)));
            }
        }
        health.oldest_queued_s.sort_by_key(|(p, _)| p.rank());
        Ok(health)
    })
    .await
}

/// What one [`sweep_retention`] deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Swept {
    pub advice: u64,
    pub requests: u64,
}

/// Delete advice older than [`ADVICE_RETENTION_DAYS`] and ended requests
/// older than [`REQUEST_RETENTION_DAYS`], at most [`SWEEP_BATCH`] of each.
/// Feedback is never deleted (its advice_id becomes NULL).
pub async fn sweep_retention(db: &Db) -> Result<Swept, ForgeError> {
    const QUERY: &str = "llm.sweep_retention";
    db.within(QUERY, async {
        let advice = sqlx::query(
            "DELETE FROM forge.llm_advice WHERE id IN (SELECT id FROM forge.llm_advice \
             WHERE created_at < now() - make_interval(days => $1) ORDER BY id LIMIT $2)",
        )
        .bind(ADVICE_RETENTION_DAYS)
        .bind(SWEEP_BATCH)
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?
        .rows_affected();
        let requests = sqlx::query(
            "DELETE FROM forge.llm_requests WHERE id IN (SELECT id FROM forge.llm_requests \
             WHERE queued_at < now() - make_interval(days => $1) \
               AND state NOT IN ('queued', 'running') ORDER BY id LIMIT $2)",
        )
        .bind(REQUEST_RETENTION_DAYS)
        .bind(SWEEP_BATCH)
        .execute(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))?
        .rows_affected();
        Ok(Swept { advice, requests })
    })
    .await
}

#[cfg(test)]
mod tests;
