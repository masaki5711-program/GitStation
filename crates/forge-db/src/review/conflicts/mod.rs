//! Predicted conflicts between open merge requests (M3 X6, plan 04 C-2,
//! ADR-0024 Amendment X6): the derived tables
//! `forge.merge_request_conflict_scans` (one row per merge request) and
//! `forge.merge_request_conflict_pairs` (one row per unordered pair), and
//! the SQL of the scan job `merge_request.conflict_scan` (the git work is
//! forge-review's `conflicts::scan`).
//!
//! - **Request** (`request.rs`): one statement bumps the scan row's
//!   `scan_seq` and sets `pending`, then the job is enqueued with dedup key
//!   `mr_conflict_scan:<merge_request_id>:<scan_seq>` (a fresh key per
//!   request, as the mergeability check's). Callers: the version job after a
//!   new head ([`request_scan`]), the edit command on a retarget and the
//!   reopen command ([`request_scan_on`], in their own transaction). Nothing
//!   enqueues on a read.
//! - **Run** (`run.rs`): [`begin_scan`] locks the row; a job whose seq is no
//!   longer the latest is `Superseded`; otherwise it sets `scanning`.
//!   [`scan_peers`] lists the other open merge requests on the target
//!   (drafts included), most recently updated first, capped by the caller.
//!   [`record_scan`] writes the result under the row lock again, only while
//!   the seq is the latest, the merge request open and its head and target
//!   the ones scanned; each pair row is locked in (low, high) order, and a
//!   pair that ENTERS `conflict` writes `merge_request.conflict_predicted`
//!   for both sides (one audit row each, actor `system`, dedup
//!   `<prefix>:<merge_request_id>:<other_id>:<prediction_seq>`). Every other
//!   result is a derived-only commit.
//! - **End of life** (`stale.rs`): [`fail_scan`] ends a run that failed for
//!   good (`job_failed`); [`reap_stale_scans`] ends rows left `pending` /
//!   `scanning` with no live job (`job_stale`) — the worker's sweep.
//! - **Reads** ([`conflict_scan`], [`conflict_predictions`]): a pair counts
//!   only while both heads are the merge requests' current heads, both are
//!   open and target the pair's branch.

use std::time::Duration;

use forge_core::ids::MergeRequestId;
use forge_core::{ObjectId, RefName, RequestId};
use forge_errors::{ErrorCode, ForgeError};

use crate::jobs::{self, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::{read_query_error, Db};

mod request;
mod run;
mod stale;

pub(crate) use request::request_scan_on;
pub use request::{
    request_scan, request_scan_for_head, sweep_scans, ScanAsk, SweptScan, SCAN_SWEEP_ATTEMPTS,
    SCAN_SWEEP_BACKOFF,
};
pub use run::{
    abandon_scan, begin_scan, record_scan, scan_peers, PairResult, Peer, Peers, ScanRecorded,
    ScanResult, ScanStart,
};
pub use stale::{fail_scan, reap_stale_scans, SCAN_STALE_AFTER};

/// The kind of the scan job.
pub const CONFLICT_SCAN_JOB: &str = "merge_request.conflict_scan";
/// One run: at most the scan's git budget (forge-review, 120 s) and a few
/// short transactions.
pub const CONFLICT_SCAN_TIMEOUT: Duration = Duration::from_secs(180);

/// The status of a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanStatus {
    /// Requested; its job has not started.
    Pending,
    /// Its job runs.
    Scanning,
    /// Every other open merge request on the target was considered.
    Scanned,
    /// Scanned, but some pairs were not tried (`reason`: a cap, the time
    /// budget, a git failure on a pair).
    Incomplete,
    /// Nothing could be predicted (`reason`).
    Failed,
}

impl ScanStatus {
    pub const ALL: [ScanStatus; 5] = [
        Self::Pending,
        Self::Scanning,
        Self::Scanned,
        Self::Incomplete,
        Self::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Scanning => "scanning",
            Self::Scanned => "scanned",
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }

    /// A finished scan (its pairs are what it found).
    pub fn is_finished(self) -> bool {
        match self {
            Self::Scanned | Self::Incomplete | Self::Failed => true,
            Self::Pending | Self::Scanning => false,
        }
    }
}

/// What a scan found for one pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PairOutcome {
    /// The two change no common path (no trial merge ran).
    NoOverlap,
    /// They change a common path and merge without a conflict there.
    Clean,
    /// `merge-tree` of the two heads conflicts in a path both change.
    Conflict,
    /// Could not be decided (`reason`).
    Failed,
}

impl PairOutcome {
    pub const ALL: [PairOutcome; 4] = [Self::NoOverlap, Self::Clean, Self::Conflict, Self::Failed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoOverlap => "no_overlap",
            Self::Clean => "clean",
            Self::Conflict => "conflict",
            Self::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }
}

/// The stored scan of one merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredScan {
    pub status: ScanStatus,
    pub reason: Option<String>,
    pub scan_seq: i64,
    pub scanned_head_sha: Option<ObjectId>,
    pub scanned_target_branch: Option<RefName>,
    /// The target tip the last finished scan read.
    pub scanned_target_sha: Option<ObjectId>,
    /// UTC, RFC 3339.
    pub scanned_at: Option<String>,
    pub peers: i32,
    pub pairs_tried: i32,
    pub pairs_skipped: i32,
}

/// One current prediction about a merge request and another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPrediction {
    pub other: MergeRequestId,
    pub other_iid: i64,
    pub other_draft: bool,
    /// The other's head the prediction is about (its current head).
    pub other_head: ObjectId,
    pub outcome: PairOutcome,
    pub reason: Option<String>,
    pub conflicts: Vec<String>,
    pub conflicts_truncated: bool,
    /// UTC, RFC 3339.
    pub checked_at: String,
}

/// Predictions read for one merge request at most (the scan stores 20
/// tried pairs per run; older ones may still be current).
pub const MAX_PREDICTIONS: i64 = 100;

const UTC: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

pub(super) fn stored_invalid(reason: &'static str, mr: MergeRequestId) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("merge_request_id", mr.get())
}

type ScanRow = (
    String,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    i32,
    i32,
    i32,
    Option<String>,
);

fn parse_scan(row: ScanRow, mr: MergeRequestId) -> Result<StoredScan, ForgeError> {
    let (status, reason, scan_seq, head, target, scanned_at, peers, tried, skipped, tip) = row;
    Ok(StoredScan {
        status: ScanStatus::parse(&status)
            .ok_or_else(|| stored_invalid("stored_scan_status_invalid", mr))?,
        reason,
        scan_seq,
        scanned_head_sha: head
            .map(|h| ObjectId::parse(&h).map_err(|_| stored_invalid("stored_head_sha_invalid", mr)))
            .transpose()?,
        scanned_target_branch: target
            .map(|t| RefName::parse(&t).map_err(|_| stored_invalid("stored_branch_invalid", mr)))
            .transpose()?,
        scanned_target_sha: tip
            .map(|t| {
                ObjectId::parse(&t).map_err(|_| stored_invalid("stored_target_sha_invalid", mr))
            })
            .transpose()?,
        scanned_at,
        peers,
        pairs_tried: tried,
        pairs_skipped: skipped,
    })
}

/// The stored scan of `mr` (`None`: never requested). One bounded
/// statement on the read pool.
pub async fn conflict_scan(db: &Db, mr: MergeRequestId) -> Result<Option<StoredScan>, ForgeError> {
    const QUERY: &str = "review.conflict_scan";
    let row: Option<ScanRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT status, reason, scan_seq, scanned_head_sha, scanned_target_branch, \
                 to_char(scanned_at AT TIME ZONE 'UTC', {UTC}), peers, pairs_tried, \
                 pairs_skipped, scanned_target_sha FROM forge.merge_request_conflict_scans \
                 WHERE merge_request_id = $1"
            ))
            .bind(mr.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(|r| parse_scan(r, mr)).transpose()
}

type PredictionRow = (
    i64,
    i64,
    bool,
    String,
    String,
    Option<String>,
    Vec<String>,
    bool,
    String,
);

/// The current `conflict` and `failed` predictions of `mr`: pairs whose
/// stored heads are both merge requests' heads now, both open and on the
/// pair's target branch; by the other's number, at most
/// [`MAX_PREDICTIONS`]. Empty when `mr` is not open.
pub async fn conflict_predictions(
    db: &Db,
    mr: MergeRequestId,
) -> Result<Vec<StoredPrediction>, ForgeError> {
    const QUERY: &str = "review.conflict_predictions";
    let rows: Vec<PredictionRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "WITH mine AS ( \
                   SELECT p.high_mr_id AS other, p.low_head_sha AS my_head, \
                          p.high_head_sha AS other_head, p.* \
                   FROM forge.merge_request_conflict_pairs p WHERE p.low_mr_id = $1 \
                   UNION ALL \
                   SELECT p.low_mr_id, p.high_head_sha, p.low_head_sha, p.* \
                   FROM forge.merge_request_conflict_pairs p WHERE p.high_mr_id = $1) \
                 SELECT o.id, o.iid, o.draft, x.other_head, x.outcome, x.reason, x.conflicts, \
                        x.conflicts_truncated, to_char(x.checked_at AT TIME ZONE 'UTC', {UTC}) \
                 FROM mine x \
                 JOIN forge.merge_requests m ON m.id = $1 \
                 JOIN forge.merge_requests o ON o.id = x.other \
                 WHERE m.state = 'open' AND o.state = 'open' \
                   AND o.project_id = m.project_id \
                   AND m.target_branch = x.target_branch AND o.target_branch = x.target_branch \
                   AND m.head_sha = x.my_head AND o.head_sha = x.other_head \
                   AND x.outcome IN ('conflict', 'failed') \
                 ORDER BY o.iid LIMIT $2"
            ))
            .bind(mr.get())
            .bind(MAX_PREDICTIONS)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(
            |(other, other_iid, other_draft, head, outcome, reason, conflicts, truncated, at)| {
                Ok(StoredPrediction {
                    other: MergeRequestId::new(other),
                    other_iid,
                    other_draft,
                    other_head: ObjectId::parse(&head)
                        .map_err(|_| stored_invalid("stored_head_sha_invalid", mr))?,
                    outcome: PairOutcome::parse(&outcome)
                        .ok_or_else(|| stored_invalid("stored_pair_outcome_invalid", mr))?,
                    reason,
                    conflicts,
                    conflicts_truncated: truncated,
                    checked_at: at,
                })
            },
        )
        .collect()
}

fn invalid_job(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", CONFLICT_SCAN_JOB)
}

/// The dedup key of request `scan_seq` of `mr` ([`LIVE_JOB`] builds the
/// same string in SQL).
fn dedup_key(mr: MergeRequestId, scan_seq: i64) -> String {
    format!("mr_conflict_scan:{}:{scan_seq}", mr.get())
}

/// The job that runs request `scan_seq` of `mr`.
pub fn conflict_scan_job(
    mr: MergeRequestId,
    scan_seq: i64,
    origin_request_id: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let key = dedup_key(mr, scan_seq);
    Ok(NewJob {
        kind: JobKind::parse(CONFLICT_SCAN_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({ "merge_request_id": mr.get(), "scan_seq": scan_seq }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(CONFLICT_SCAN_TIMEOUT)
            .ok_or_else(|| invalid_job("job_timeout"))?,
        dedup_key: Some(jobs::DedupKey::parse(&key).ok_or_else(|| invalid_job("dedup_key"))?),
        origin_request_id,
        origin_event_seq,
    })
}

/// What a claimed scan job scans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictScanRequest {
    pub merge_request: MergeRequestId,
    pub scan_seq: i64,
}

/// The payload of a claimed job (`FG-SYS-0001 reason=invalid_job_payload`
/// for anything else: forge wrote it).
pub fn conflict_scan_payload(
    payload: &serde_json::Value,
) -> Result<ConflictScanRequest, ForgeError> {
    let positive = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_i64)
            .filter(|v| *v > 0)
            .ok_or_else(|| invalid_job("invalid_job_payload"))
    };
    Ok(ConflictScanRequest {
        merge_request: MergeRequestId::new(positive("merge_request_id")?),
        scan_seq: positive("scan_seq")?,
    })
}

/// `FG-REVIEW-0022 reason=<reason>`: a scan that did not try every pair
/// (peers_capped, pairs_capped, budget_exhausted, pair_failed) or did not
/// finish (job_failed, job_stale).
pub fn prediction_incomplete(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ConflictPredictionIncomplete).with_context("reason", reason)
}

/// The SQL of "a queued or running job holds this row's latest request"
/// (`s` is the scan row).
const LIVE_JOB: &str = "EXISTS (SELECT 1 FROM forge.jobs j \
     WHERE j.kind = 'merge_request.conflict_scan' \
     AND j.dedup_key = 'mr_conflict_scan:' || s.merge_request_id || ':' || s.scan_seq \
     AND j.state IN ('queued', 'running'))";

#[cfg(test)]
mod tests;
