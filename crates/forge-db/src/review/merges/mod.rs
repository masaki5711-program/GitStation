//! Merging a merge request (M3 R4, ADR-0024 §4; plan 11 §11 decisions 9 and
//! 10): the merge attempts (`forge.merge_request_merges`, derived state, one
//! row per attempt) and the SQL of the request and of the merge job. The git
//! work and every decision are forge-review's (`forge_review::merges`); this
//! module only reads facts under locks, hands them to `decide`, and moves an
//! attempt through its states with conditional updates.
//!
//! - **Request** (`request.rs`, [`request_merge_mr_decided`]): under the
//!   project, merge request and caller locks (the review lock order) it
//!   reads the target's protection rules, the stored mergeability, the live
//!   attempt and calls `decide`; then inserts the `queued` attempt and its
//!   job `merge_request.merge` (dedup `mr_merge:<merge_id>`) in one
//!   derived-only commit. A live attempt is FG-REVIEW-0011 (the partial
//!   unique index `merge_request_merges_one_live` is the backstop).
//! - **Job** (`job.rs`): [`merge_gate_decided`] re-reads the same facts under
//!   the same locks plus the requester's credential (tx1a: decide only;
//!   tx1b: decide and `queued → prepared` with `expected_old` / `new_sha`;
//!   a refusal sets the attempt `superseded` in the same transaction);
//!   [`fence_merge`] is the compare-and-swap `prepared → executing`;
//!   [`finish_merge`] is tx2 (`executing → applied`, the merge request
//!   `merged`, the audit rows and the `merge_request.merged` /
//!   `repository.pushed` events deduplicated by the merge id);
//!   [`end_merge`] ends an attempt `superseded` / `failed` from given
//!   states; [`stuck_merges`] lists what the reconciliation looks at.
//! - **Lock** (`lock.rs`, [`lock_merges`]): the per repository advisory lock
//!   `merge:<repo_id>` the job holds around the git work and the ref update.
//!
//! The commands' names carry `_mr_` (`request_merge_mr_decided`,
//! `merge_gate_decided`… are called only by forge-review: the test
//! `only_forge_review_calls_the_review_commands`).

use std::time::Duration;

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{MergeMethod, MergeState, ObjectId, RefName, RequestId, SourceOutcome};
use forge_errors::{ErrorCode, ForgeError};

use crate::jobs::{self, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::{query_error, read_query_error, Db};

mod credential;
mod job;
mod lock;
mod request;

pub use credential::{CredentialFacts, CredentialState};
pub use job::{
    end_merge, executing_elapsed, fence_merge, finish_merge, mark_reconcile_seen,
    merge_gate_decided, record_source_deleted, set_source_outcome, stuck_merges, GateOutcome,
    GateStep, MergeFinished, SourceDeletion,
};
pub use lock::{
    is_lock_busy, lock_merges, MergeLock, MERGE_LOCK_BUSY, MERGE_LOCK_HOLD, MERGE_LOCK_WAIT,
};
pub(crate) use request::refuse_live_merge;
pub use request::{request_merge_mr_decided, MergeGate, NewMerge};

/// The kind of the merge job.
pub const MERGE_JOB: &str = "merge_request.merge";
/// One run of the merge job: the lock wait, the trial merge, the commit,
/// the ref update and four short transactions (each git call has its own
/// timeout well inside this).
pub const MERGE_JOB_TIMEOUT: Duration = Duration::from_secs(300);

/// The credential a merge was requested with (ADR-0024 §4.2 step 1: its
/// kind and id only; everything else is read again by the job).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialRef {
    /// A web session (`forge.web_sessions.id`).
    Session(i64),
    /// A personal access token (`forge.tokens.id`).
    Token(i64),
}

impl CredentialRef {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Session(_) => "session",
            Self::Token(_) => "token",
        }
    }

    pub fn id(self) -> i64 {
        match self {
            Self::Session(id) | Self::Token(id) => id,
        }
    }
}

/// One merge attempt as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMerge {
    /// The merge id.
    pub id: i64,
    pub merge_request: MergeRequestId,
    pub project_id: ProjectId,
    pub method: MergeMethod,
    pub state: MergeState,
    pub requested_by: UserId,
    pub credential: CredentialRef,
    /// The request that asked for the merge (the job's logs carry it).
    pub request_id: RequestId,
    /// The head the requester saw and asked to merge.
    pub head_sha: ObjectId,
    pub target_branch: RefName,
    /// The requester's commit message (`None`: built from the merge
    /// request's title and description).
    pub message: Option<String>,
    pub delete_source_branch: bool,
    /// The target tip the trial merge used (set at `prepared`).
    pub expected_old: Option<ObjectId>,
    /// The commit the target moves to (set at `prepared`).
    pub new_sha: Option<ObjectId>,
    /// UTC, RFC 3339 (set at `executing`).
    pub executing_at: Option<String>,
    pub reason: Option<String>,
    pub error_code: Option<String>,
    pub source_outcome: Option<SourceOutcome>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

/// The columns of [`MergeRow`], in order.
pub(crate) fn columns() -> String {
    format!(
        "id, merge_request_id, project_id, method, state, requested_by, credential_kind, \
         credential_id, request_id, head_sha, target_branch, message, delete_source_branch, \
         expected_old, new_sha, to_char(executing_at AT TIME ZONE 'UTC', {TS}) AS executing_at, \
         reason, error_code, source_outcome, \
         to_char(created_at AT TIME ZONE 'UTC', {TS}) AS created_at, \
         to_char(updated_at AT TIME ZONE 'UTC', {TS}) AS updated_at, \
         to_char(finished_at AT TIME ZONE 'UTC', {TS}) AS finished_at"
    )
}

#[derive(sqlx::FromRow)]
pub(crate) struct MergeRow {
    id: i64,
    merge_request_id: i64,
    project_id: i64,
    method: String,
    state: String,
    requested_by: i64,
    credential_kind: String,
    credential_id: i64,
    request_id: String,
    head_sha: String,
    target_branch: String,
    message: Option<String>,
    delete_source_branch: bool,
    expected_old: Option<String>,
    new_sha: Option<String>,
    executing_at: Option<String>,
    reason: Option<String>,
    error_code: Option<String>,
    source_outcome: Option<String>,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>,
}

/// `FG-SYS-0001 reason=<reason>` for a stored attempt this build cannot
/// read (the table's CHECKs make it unreachable).
fn stored_invalid(reason: &'static str, id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("merge_id", id)
}

impl MergeRow {
    pub(crate) fn parse(self) -> Result<StoredMerge, ForgeError> {
        let id = self.id;
        let oid = |raw: &str| {
            ObjectId::parse(raw).map_err(|_| stored_invalid("stored_merge_sha_invalid", id))
        };
        let credential = match self.credential_kind.as_str() {
            "session" => CredentialRef::Session(self.credential_id),
            "token" => CredentialRef::Token(self.credential_id),
            _ => return Err(stored_invalid("stored_credential_kind_invalid", id)),
        };
        Ok(StoredMerge {
            id,
            merge_request: MergeRequestId::new(self.merge_request_id),
            project_id: ProjectId::new(self.project_id),
            method: MergeMethod::parse(&self.method)
                .ok_or_else(|| stored_invalid("stored_merge_method_invalid", id))?,
            state: MergeState::parse(&self.state)
                .ok_or_else(|| stored_invalid("stored_merge_state_invalid", id))?,
            requested_by: UserId::new(self.requested_by),
            credential,
            request_id: RequestId::parse(&self.request_id)
                .ok_or_else(|| stored_invalid("stored_request_id_invalid", id))?,
            head_sha: oid(&self.head_sha)?,
            target_branch: RefName::parse(&self.target_branch)
                .ok()
                .filter(|r| r.branch().is_some())
                .ok_or_else(|| stored_invalid("stored_branch_invalid", id))?,
            message: self.message,
            delete_source_branch: self.delete_source_branch,
            expected_old: self.expected_old.as_deref().map(oid).transpose()?,
            new_sha: self.new_sha.as_deref().map(oid).transpose()?,
            executing_at: self.executing_at,
            reason: self.reason,
            error_code: self.error_code,
            source_outcome: self
                .source_outcome
                .map(|o| {
                    SourceOutcome::parse(&o)
                        .ok_or_else(|| stored_invalid("stored_source_outcome_invalid", id))
                })
                .transpose()?,
            created_at: self.created_at,
            updated_at: self.updated_at,
            finished_at: self.finished_at,
        })
    }
}

/// Merge attempt `id` as committed (the write pool: the job and the
/// reconciliation read what they just wrote). `None`: no such attempt.
pub async fn merge_attempt(db: &Db, id: i64) -> Result<Option<StoredMerge>, ForgeError> {
    const QUERY: &str = "review.merge_attempt";
    let row: Option<MergeRow> = db
        .within(QUERY, async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error(QUERY, e))?;
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_request_merges WHERE id = $1",
                columns()
            ))
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(MergeRow::parse).transpose()
}

/// The latest merge attempt of `mr` (`None`: never asked to merge). One
/// bounded statement on the read pool (the status route).
pub async fn latest_merge(db: &Db, mr: MergeRequestId) -> Result<Option<StoredMerge>, ForgeError> {
    const QUERY: &str = "review.latest_merge";
    let row: Option<MergeRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_request_merges WHERE merge_request_id = $1 \
                 ORDER BY id DESC LIMIT 1",
                columns()
            ))
            .bind(mr.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(MergeRow::parse).transpose()
}

/// The iid of merge request `mr` (the review lock helpers lock by project
/// and iid). `FG-REPO-0005 resource=merge_request` when it is gone.
pub(crate) async fn iid_of(db: &Db, mr: MergeRequestId) -> Result<i64, ForgeError> {
    const QUERY: &str = "review.merge_iid";
    db.within(QUERY, async {
        sqlx::query_scalar("SELECT iid FROM forge.merge_requests WHERE id = $1")
            .bind(mr.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))?
            .ok_or_else(crate::review::commands::absent)
    })
    .await
}

/// The names a commit identity is built from (ADR-0024 §4.1: the database's
/// display name and e-mail, never the client's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountIdentity {
    pub username: String,
    pub display_name: String,
    /// `None`: the account has no e-mail.
    pub email: Option<String>,
}

/// The identity of `user` (`None`: no such user). One statement on the
/// write pool.
pub async fn account_identity(
    db: &Db,
    user: UserId,
) -> Result<Option<AccountIdentity>, ForgeError> {
    const QUERY: &str = "review.merge_identity";
    let row: Option<(String, String, Option<String>)> = db
        .within(QUERY, async {
            sqlx::query_as("SELECT username, display_name, email FROM forge.users WHERE id = $1")
                .bind(user.get())
                .fetch_optional(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(|(username, display_name, email)| AccountIdentity {
        username,
        display_name,
        email,
    }))
}

fn invalid_job(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", MERGE_JOB)
}

/// The job that runs merge attempt `merge_id` (dedup `mr_merge:<merge_id>`:
/// one job per attempt).
pub fn merge_job(merge_id: i64, origin_request_id: RequestId) -> Result<NewJob, ForgeError> {
    let key = format!("mr_merge:{merge_id}");
    Ok(NewJob {
        kind: JobKind::parse(MERGE_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({ "merge_id": merge_id }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(MERGE_JOB_TIMEOUT).ok_or_else(|| invalid_job("job_timeout"))?,
        dedup_key: Some(jobs::DedupKey::parse(&key).ok_or_else(|| invalid_job("dedup_key"))?),
        origin_request_id,
        origin_event_seq: None,
    })
}

/// The merge id of a claimed merge job (`FG-SYS-0001
/// reason=invalid_job_payload` for anything else: forge wrote it).
pub fn merge_payload(payload: &serde_json::Value) -> Result<i64, ForgeError> {
    payload
        .get("merge_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|v| *v > 0)
        .ok_or_else(|| invalid_job("invalid_job_payload"))
}

/// `FG-REVIEW-0011` MergeInProgress (409): `mr` has live attempt
/// `merge_id`; `action` names what was refused.
pub fn in_progress(mr: MergeRequestId, merge_id: Option<i64>, action: &'static str) -> ForgeError {
    let e = ForgeError::new(ErrorCode::MergeInProgress)
        .with_context("merge_request_id", mr.get())
        .with_context("action", action);
    match merge_id {
        Some(id) => e.with_context("merge_id", id),
        None => e.with_context("reason", "unique_index"),
    }
}
