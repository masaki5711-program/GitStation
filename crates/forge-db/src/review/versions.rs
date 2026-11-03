//! Merge request versions (M3 R1b, ADR-0024 §1, §2; plan 11 §11.3).
//!
//! A version is one head commit a merge request had. The version job
//! (`merge_request.refresh`, run by `forge worker` through forge-review)
//! reads the real source branch with forge-git, then:
//!
//! 1. [`record_mr_version`] commits the version row FIRST (its number drawn
//!    under the merge request's row lock: `max + 1`, unique per merge
//!    request), moves `merge_requests.head_sha`, and writes one
//!    `merge_request.pushed` audit row and event (actor `system`,
//!    dedup `merge_request_pushed:<merge_request_id>:<head_sha>`);
//! 2. only then are the hidden refs written, from the committed rows
//!    ([`with_versions_locked`], forge-review's ref sync).
//!
//! The job is enqueued by the `merge-request-refresh` subscriber
//! (`refresh.rs`) for every open merge request whose source branch a push
//! moved, by [`super::open_mr_decided`] (the first version) and by
//! [`super::reopen_mr_decided`]. [`refresh_job`] builds it; every trigger has
//! its own dedup key, so a retried enqueue never doubles a job while each
//! push still gets one.

use std::time::Duration;

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{
    AuditDetails, DedupKey, DedupPart, MergeRequestChange, MergeRequestState,
    MergeRequestVersionChange, ObjectId, RepoId, RepositoryState, RequestId,
};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{Row, StoredMergeRequest, COLUMNS};
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome};
use crate::jobs::{self, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::{query_error, Db};

/// The kind of the version job.
pub const REFRESH_JOB: &str = "merge_request.refresh";
/// One run of the version job: two git reads, two short transactions and
/// one `update-ref`, each bounded on its own.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

/// One version as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVersion {
    pub version_no: i64,
    pub head_sha: ObjectId,
    pub pushed_by: Option<UserId>,
    /// Its refs were purged (secret removal): never written again.
    pub refs_purged: bool,
    /// The target's tip when the version was recorded (M3 R2; `None`
    /// until the version job recorded it, or when the target was missing).
    pub base_sha: Option<ObjectId>,
    /// The merge base of the head and `base_sha` (`None`: not recorded, or
    /// no common history). A version's diff is `merge_base_sha..head_sha`.
    pub merge_base_sha: Option<ObjectId>,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// `(version_no, head_sha, pushed_by, refs_purged, base_sha, merge_base_sha,
/// created_at)`.
type VersionRow = (
    i64,
    String,
    Option<i64>,
    bool,
    Option<String>,
    Option<String>,
    String,
);

pub(super) const VERSION_COLUMNS: &str =
    "version_no, head_sha, pushed_by, refs_purged_at IS NOT NULL, base_sha, merge_base_sha, \
     to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')";

fn stored_version(row: VersionRow, mr: MergeRequestId) -> Result<StoredVersion, ForgeError> {
    let (version_no, head, pushed_by, refs_purged, base, merge_base, created_at) = row;
    let oid = |raw: &str, reason: &'static str| {
        ObjectId::parse(raw).map_err(|_| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", reason)
                .with_context("merge_request_id", mr.get())
        })
    };
    Ok(StoredVersion {
        version_no,
        head_sha: oid(&head, "stored_head_sha_invalid")?,
        pushed_by: pushed_by.map(UserId::new),
        refs_purged,
        base_sha: base
            .as_deref()
            .map(|b| oid(b, "stored_base_sha_invalid"))
            .transpose()?,
        merge_base_sha: merge_base
            .as_deref()
            .map(|b| oid(b, "stored_merge_base_sha_invalid"))
            .transpose()?,
        created_at,
    })
}

/// Every version of `mr`, oldest first, on `conn` (`lock`: `FOR SHARE` /
/// `FOR UPDATE` / empty).
pub(super) async fn versions_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
    lock: &'static str,
    query: &'static str,
) -> Result<Vec<StoredVersion>, ForgeError> {
    let rows: Vec<VersionRow> = sqlx::query_as(&format!(
        "SELECT {VERSION_COLUMNS} FROM forge.merge_request_versions \
         WHERE merge_request_id = $1 ORDER BY version_no {lock}"
    ))
    .bind(mr.get())
    .fetch_all(conn)
    .await
    .map_err(|e| query_error(query, e))?;
    rows.into_iter().map(|r| stored_version(r, mr)).collect()
}

/// Every version of `mr` (write pool: the job and the tests read what was
/// just committed).
pub async fn list_versions(db: &Db, mr: MergeRequestId) -> Result<Vec<StoredVersion>, ForgeError> {
    const QUERY: &str = "review.versions";
    db.within(QUERY, async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        versions_on(&mut conn, mr, "", QUERY).await
    })
    .await
}

/// Every version of `mr`, oldest first, on the read pool (the API). One
/// bounded statement.
pub async fn read_versions(db: &Db, mr: MergeRequestId) -> Result<Vec<StoredVersion>, ForgeError> {
    const QUERY: &str = "review.versions_read";
    let rows: Vec<VersionRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {VERSION_COLUMNS} FROM forge.merge_request_versions \
                 WHERE merge_request_id = $1 ORDER BY version_no"
            ))
            .bind(mr.get())
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| crate::read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(|r| stored_version(r, mr)).collect()
}

/// What the version job works on.
#[derive(Debug, Clone)]
pub struct RefreshTarget {
    pub merge_request: StoredMergeRequest,
    pub repository: RepoId,
    pub repository_ready: bool,
}

/// The merge request `mr` and its project's repository (`None`: the merge
/// request is gone — the project was purged).
pub async fn refresh_target(
    db: &Db,
    mr: MergeRequestId,
) -> Result<Option<RefreshTarget>, ForgeError> {
    const QUERY: &str = "review.refresh_target";
    db.within(QUERY, async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM forge.merge_requests WHERE id = $1"
        ))
        .bind(mr.get())
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let stored = row.parse()?;
        let repo: Option<(i64, String)> =
            sqlx::query_as("SELECT id, state FROM forge.repositories WHERE project_id = $1")
                .bind(stored.project_id.get())
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| query_error(QUERY, e))?;
        let Some((repo, state)) = repo else {
            return Ok(None);
        };
        Ok(Some(RefreshTarget {
            merge_request: stored,
            repository: RepoId::new(repo),
            repository_ready: state == RepositoryState::Ready.as_str(),
        }))
    })
    .await
}

/// What enqueued a version job (its dedup key's last part).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshTrigger {
    /// The merge request was opened (`mr_refresh:<id>:opened`).
    Opened,
    /// It was reopened at this revision (`mr_refresh:<id>:reopened:<rev>`).
    Reopened { revision: i64 },
    /// The `repository.pushed` event with this seq
    /// (`mr_refresh:<id>:push:<seq>`).
    Pushed { seq: i64 },
}

/// The job's payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshRequest {
    pub merge_request: MergeRequestId,
    /// The pusher whose push triggered the job (`None` for open / reopen).
    pub pushed_by: Option<UserId>,
}

fn invalid_job(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", REFRESH_JOB)
}

/// The version job for `request`, enqueued because of `trigger`.
pub fn refresh_job(
    request: RefreshRequest,
    trigger: RefreshTrigger,
    origin_request_id: RequestId,
    origin_event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let id = request.merge_request.get();
    let key = match trigger {
        RefreshTrigger::Opened => format!("mr_refresh:{id}:opened"),
        RefreshTrigger::Reopened { revision } => format!("mr_refresh:{id}:reopened:{revision}"),
        RefreshTrigger::Pushed { seq } => format!("mr_refresh:{id}:push:{seq}"),
    };
    Ok(NewJob {
        kind: JobKind::parse(REFRESH_JOB).ok_or_else(|| invalid_job("invalid_job_kind"))?,
        payload: serde_json::json!({
            "merge_request_id": id,
            "pushed_by": request.pushed_by.map(UserId::get),
        }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(REFRESH_TIMEOUT).ok_or_else(|| invalid_job("job_timeout"))?,
        dedup_key: Some(jobs::DedupKey::parse(&key).ok_or_else(|| invalid_job("dedup_key"))?),
        origin_request_id,
        origin_event_seq,
    })
}

/// The payload of a claimed version job (`FG-SYS-0001
/// reason=invalid_job_payload` for anything else: forge wrote it).
pub fn refresh_payload(payload: &serde_json::Value) -> Result<RefreshRequest, ForgeError> {
    let id = payload
        .get("merge_request_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(|| invalid_job("invalid_job_payload"))?;
    let pushed_by = match payload.get("pushed_by") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(UserId::new(
            v.as_i64()
                .filter(|u| *u > 0)
                .ok_or_else(|| invalid_job("invalid_job_payload"))?,
        )),
    };
    Ok(RefreshRequest {
        merge_request: MergeRequestId::new(id),
        pushed_by,
    })
}

/// What [`record_mr_version`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionOutcome {
    /// Version `version` was added and is the head (`merge_request.pushed`).
    Added { version: i64 },
    /// The head went back to existing version `version`
    /// (`merge_request.head_restored`, audit-only).
    Restored { version: i64 },
    /// `head` already is the merge request's head: nothing written.
    AlreadyHead { version: Option<i64> },
    /// `head` is a version whose refs were purged: it never becomes the
    /// head again; only the audit-only `merge_request.purged_head_pushed`
    /// is written (runbook `secret-removal`).
    PurgedHead { version: i64 },
    /// The merge request is not open: nothing written.
    NotOpen(MergeRequestState),
    /// The merge request is gone.
    Gone,
}

fn audit_entry(project: ProjectId, details: &AuditDetails) -> AuditEntry {
    AuditEntry {
        action: details.action(),
        outcome: Outcome::Succeeded,
        resource_type: "project",
        resource_id: Some(project.get()),
        project_id: Some(project),
        error_code: None,
        details: details.to_value(),
    }
}

/// Record `head` as the head of merge request `mr` (module docs, step 1),
/// in one transaction under the merge request's row lock, as `ctx`'s actor
/// (the worker: `system`). Idempotent: a rerun with the same head is
/// [`VersionOutcome::AlreadyHead`] and writes nothing.
pub async fn record_mr_version(
    db: &Db,
    ctx: &ChangeContext,
    mr: MergeRequestId,
    head: &ObjectId,
    pushed_by: Option<UserId>,
) -> Result<VersionOutcome, ForgeError> {
    record_mr_version_carrying(db, ctx, mr, head, pushed_by, None).await
}

/// End the live approvals of `current` for a new head `head`, or carry
/// them (M3 R6, ADR-0024 §8.2) when every condition holds under the merge
/// request's lock: forge-review's pure `may_carry_approvals` allowed the
/// change from `carry_from` (the head it was computed for, still the
/// recorded head), the project keeps approvals on push
/// (`reset_approvals_on_push = false`, read now), and the target is not a
/// protected branch (its rules read now). Anything else ends them with
/// `new_commits` (the default; fail closed).
async fn end_or_carry(
    tx: &mut ChangeTx,
    current: &StoredMergeRequest,
    head: &ObjectId,
    carry_from: Option<&ObjectId>,
) -> Result<(Vec<crate::change::AuditEntry>, Vec<DomainEvent>), ForgeError> {
    let asked = carry_from.is_some_and(|from| current.head_sha.as_deref() == Some(from.as_str()));
    if asked {
        let project = current.project_id;
        let settings = tx
            .step(|c| async move { super::settings::settings_on(c, project).await })
            .await?;
        let rules = tx
            .step(|c| async move { crate::protected_refs::rules_on(c, project).await })
            .await?;
        if !settings.values.reset_approvals_on_push
            && rules.levels(&current.target_branch).is_empty()
        {
            return super::approvals::carry_on(tx, current, head).await;
        }
    }
    super::approvals::revoke_on(tx, current, forge_core::UnapproveReason::NewCommits).await
}

/// [`record_mr_version`] for a head whose change forge-review allowed to
/// carry the approvals from `carry_from` (M3 R6; see `end_or_carry`).
pub async fn record_mr_version_carrying(
    db: &Db,
    ctx: &ChangeContext,
    mr: MergeRequestId,
    head: &ObjectId,
    pushed_by: Option<UserId>,
    carry_from: Option<&ObjectId>,
) -> Result<VersionOutcome, ForgeError> {
    const ACTION: &str = "merge_request.pushed";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let row: Option<Row> = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "SELECT {COLUMNS} FROM forge.merge_requests WHERE id = $1 FOR UPDATE"
                ))
                .bind(mr.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("review.version_lock", e))
            })
            .await?;
        let Some(current) = row.map(Row::parse).transpose()? else {
            return Ok(crate::change::Settle::Discard(VersionOutcome::Gone));
        };
        if current.state != MergeRequestState::Open {
            return Ok(crate::change::Settle::Discard(VersionOutcome::NotOpen(
                current.state,
            )));
        }
        let known = tx
            .step(|c| async move { versions_on(c, mr, "", "review.version_read").await })
            .await?;
        let existing = known.iter().find(|v| v.head_sha == *head);
        if current.head_sha.as_deref() == Some(head.as_str()) {
            return Ok(crate::change::Settle::Discard(
                VersionOutcome::AlreadyHead {
                    version: existing.map(|v| v.version_no),
                },
            ));
        }
        let previous_head = current
            .head_sha
            .as_deref()
            .map(ObjectId::parse)
            .transpose()?;
        if let Some(v) = existing {
            if v.refs_purged {
                // Never the head again; leave a trace (security review):
                // the purged commit was pushed back.
                let details = AuditDetails::MergeRequestVersion(
                    MergeRequestVersionChange::PurgedHeadPushed {
                        merge_request: mr,
                        iid: current.iid,
                        version: v.version_no,
                        head_sha: head.clone(), // clone: the details own their values
                    },
                );
                let record = ChangeRecord {
                    audit: audit_entry(current.project_id, &details),
                    affected: Vec::new(),
                    events: Vec::new(),
                };
                return Ok(crate::change::Settle::Commit(
                    record,
                    VersionOutcome::PurgedHead {
                        version: v.version_no,
                    },
                ));
            }
            let version = v.version_no;
            move_head(&mut tx, mr, head).await?;
            // M3 R2: the stored mergeability is about another head now.
            tx.step(|c| async move { super::mergeability::downgrade_on(c, mr, head).await })
                .await?;
            // M3 R5 (ADR-0024 §8.2): approvals of the old head end (M3 R6:
            // or are carried).
            let (affected, events) = end_or_carry(&mut tx, &current, head, carry_from).await?;
            let details =
                AuditDetails::MergeRequestVersion(MergeRequestVersionChange::HeadRestored {
                    merge_request: mr,
                    iid: current.iid,
                    version,
                    head_sha: head.clone(), // clone: the details own their values
                    previous_head,
                });
            let record = ChangeRecord {
                audit: audit_entry(current.project_id, &details),
                affected,
                events,
            };
            return Ok(crate::change::Settle::Commit(
                record,
                VersionOutcome::Restored { version },
            ));
        }
        let version = known.iter().map(|v| v.version_no).max().unwrap_or(0) + 1;
        let project = current.project_id;
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.merge_request_versions \
                 (merge_request_id, project_id, version_no, head_sha, pushed_by) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(mr.get())
            .bind(project.get())
            .bind(version)
            .bind(head.as_str())
            .bind(pushed_by.map(UserId::get))
            .execute(c)
            .await
            .map(drop)
            .map_err(|e| query_error("review.version_insert", e))
        })
        .await?;
        move_head(&mut tx, mr, head).await?;
        // M3 R2: the stored mergeability is about another head now.
        tx.step(|c| async move { super::mergeability::downgrade_on(c, mr, head).await })
            .await?;
        // M3 R5 (ADR-0024 §8.2, plan 11 Q4): new commits end the approvals
        // (M3 R6: or carry them when the project keeps them and the change
        // allows it).
        let (affected, mut revoked) = end_or_carry(&mut tx, &current, head, carry_from).await?;
        let change = MergeRequestChange::Pushed {
            merge_request: mr,
            iid: current.iid,
            version,
            head_sha: head.clone(), // clone: the change owns its values
            previous_head,
            pushed_by,
        };
        let dedup = DedupKey::new(
            change.dedup_prefix(),
            &[DedupPart::Id(mr.get()), DedupPart::Object(head)],
        )?;
        let details = AuditDetails::MergeRequest(change);
        let event_type = details.event_type().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "audit_only_event")
        })?;
        let mut events = vec![DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: "merge_request",
            aggregate_id: mr.get(),
            project_id: Some(project),
            payload: details.to_value(),
            dedup_key: Some(dedup),
        }];
        events.append(&mut revoked);
        let record = ChangeRecord {
            audit: audit_entry(project, &details),
            affected,
            events,
        };
        Ok(crate::change::Settle::Commit(
            record,
            VersionOutcome::Added { version },
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// `merge_requests.head_sha = head`, one more revision; the new revision.
async fn move_head(
    tx: &mut ChangeTx,
    mr: MergeRequestId,
    head: &ObjectId,
) -> Result<i64, ForgeError> {
    tx.step(|c| async move {
        sqlx::query_scalar(
            "UPDATE forge.merge_requests SET head_sha = $2, revision = revision + 1, \
             updated_at = now() WHERE id = $1 RETURNING revision",
        )
        .bind(mr.get())
        .bind(head.as_str())
        .fetch_one(c)
        .await
        .map_err(|e| query_error("review.version_head", e))
    })
    .await
}
