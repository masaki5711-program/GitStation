//! The hidden refs of the versions (M3 R1b, ADR-0024 §2): the per merge
//! request lock under which forge-review reads a source tip, records a
//! version and writes the refs, and the administrator's purge.
//!
//! [`lock_version_refs`] takes `pg_advisory_xact_lock` on
//! `merge_request_refs:<merge_request_id>` in a transaction of its own and
//! holds it until [`RefsLock::release`] (review: two refresh jobs of one
//! merge request could interleave — A reads tip T1, B records and syncs
//! T2, A moves `head` back). Everything that writes a merge request's
//! hidden refs takes it first: the version job for its whole run (tip
//! read, version row, sync) and the purge for its sync. Nothing else is
//! locked: [`versions_snapshot`] is a plain read, and edits of the merge
//! request never wait for git. The purge's marks (`refs_purged_at`) may
//! commit while a sync runs: that sync may still write a just-marked
//! version's ref, and the purge's own sync — which waits for the lock —
//! deletes it; every later sync reads the mark (ADR-0024 closing table
//! #42).
//!
//! **Connections**: a locked run holds one write-pool connection (idle in
//! its transaction) for at most [`REFS_LOCK_HOLD`], plus at most one more at
//! a time for its short statements. The worker runs one job at a time
//! (`forge_db::jobs::JobRunner`) and `forge serve` one sync per purge
//! request, so the write pool needs two connections per concurrent run.
//! The transaction's `idle_in_transaction_session_timeout` is raised to
//! cover the hold; [`RefsLock::alive`] proves the lock still stands just
//! before a ref is written.
//!
//! [`purge_mr_version_refs_decided`] marks the versions as purged
//! (`merge_request.version_refs_purged`, audit-only), decided inside the
//! transaction like every review command; forge-review then deletes the
//! refs under the lock.

use std::time::Duration;

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{AuditDetails, MergeRequestState, MergeRequestVersionChange, ObjectId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::Postgres;
use sqlx::Transaction;

use super::commands::{absent, facts, MergeRequestFacts};
use super::versions::{versions_on, StoredVersion};
use super::StoredMergeRequest;
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::{query_error, Db};

/// What a ref sync sees: the merge request's number and head, and every
/// version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionsSnapshot {
    pub merge_request: MergeRequestId,
    pub iid: i64,
    pub head_sha: Option<ObjectId>,
    pub versions: Vec<StoredVersion>,
}

/// The longest wait for another run's lock (`55P03` → FG-DB-0002,
/// retryable).
pub const REFS_LOCK_WAIT: Duration = Duration::from_secs(10);
/// The longest a run holds the lock (the work under it is bounded by this
/// from the moment the lock is taken).
pub const REFS_LOCK_HOLD: Duration = Duration::from_secs(45);
/// The lock's transaction outlives an idle gap of the hold, plus slack.
const IDLE_LIMIT: Duration = Duration::from_secs(60);
/// The bound of the explicit ROLLBACK that releases the lock.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// The per merge request lock (module docs). Dropping it without
/// [`RefsLock::release`] still releases the lock when the connection's
/// transaction ends, but late: release it explicitly.
pub struct RefsLock {
    tx: Transaction<'static, Postgres>,
    merge_request: MergeRequestId,
    taken: tokio::time::Instant,
}

impl std::fmt::Debug for RefsLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefsLock")
            .field("merge_request", &self.merge_request)
            .finish_non_exhaustive()
    }
}

/// Take `mr`'s lock, waiting at most [`REFS_LOCK_WAIT`] (`FG-DB-0002` when
/// another run holds it longer: retry later).
pub async fn lock_version_refs(db: &Db, mr: MergeRequestId) -> Result<RefsLock, ForgeError> {
    const QUERY: &str = "review.version_refs_lock";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        for setting in [
            format!(
                "SET LOCAL lock_timeout = '{}ms'",
                REFS_LOCK_WAIT.as_millis()
            ),
            format!(
                "SET LOCAL idle_in_transaction_session_timeout = '{}ms'",
                IDLE_LIMIT.as_millis()
            ),
        ] {
            sqlx::query(&setting)
                .execute(&mut *tx)
                .await
                .map_err(|e| query_error(QUERY, e))?;
        }
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('merge_request_refs:' || $1::text, 0))",
        )
        .bind(mr.get())
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e).with_context("merge_request_id", mr.get()))?;
        Ok(RefsLock {
            tx,
            merge_request: mr,
            taken: tokio::time::Instant::now(),
        })
    })
    .await
}

impl RefsLock {
    pub fn merge_request(&self) -> MergeRequestId {
        self.merge_request
    }

    /// When the hold ends ([`REFS_LOCK_HOLD`] after the lock was taken):
    /// every step under the lock is bounded by it.
    pub fn deadline(&self) -> tokio::time::Instant {
        self.taken + REFS_LOCK_HOLD
    }

    /// The lock still stands (its connection answers inside its
    /// transaction): checked right before a ref is written. `FG-DB-0002
    /// reason=refs_lock_lost` otherwise.
    pub async fn alive(&mut self) -> Result<(), ForgeError> {
        let mr = self.merge_request.get();
        let left = self
            .deadline()
            .saturating_duration_since(tokio::time::Instant::now());
        let ping = sqlx::query("SELECT 1").execute(&mut *self.tx);
        match tokio::time::timeout(left.min(RELEASE_TIMEOUT), ping).await {
            Ok(Ok(_)) if !left.is_zero() => Ok(()),
            Ok(Ok(_)) | Err(_) => Err(ForgeError::new(ErrorCode::DbQueryFailed)
                .with_context("reason", "refs_lock_lost")
                .with_context("detail", "hold_exceeded")
                .with_context("merge_request_id", mr)),
            Ok(Err(e)) => Err(query_error("review.version_refs_lock", e)
                .with_context("reason", "refs_lock_lost")
                .with_context("merge_request_id", mr)),
        }
    }

    /// Release the lock now (explicit ROLLBACK, bounded). A failed release
    /// is logged: the server ends the transaction with the connection.
    pub async fn release(self) {
        let mr = self.merge_request.get();
        let failure = match tokio::time::timeout(RELEASE_TIMEOUT, self.tx.rollback()).await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(query_error("review.version_refs_unlock", e)),
            Err(_) => Some(
                ForgeError::new(ErrorCode::DbQueryFailed)
                    .with_context("query", "review.version_refs_unlock")
                    .with_context("reason", "rollback_timeout"),
            ),
        };
        if let Some(e) = failure {
            tracing::warn!(
                target: "forge_db",
                error_code = e.code().id(),
                merge_request_id = mr,
                error = %e,
                "releasing a merge request refs lock failed; the connection ends it"
            );
        }
    }
}

/// `mr`'s number, head and versions (`None`: the merge request is gone),
/// read under `lock` (on its connection, so the read sees every commit
/// before the lock was taken and is bounded by the hold).
pub async fn versions_snapshot(
    lock: &mut RefsLock,
) -> Result<Option<VersionsSnapshot>, ForgeError> {
    const QUERY: &str = "review.version_refs_snapshot";
    let mr = lock.merge_request;
    let head: Option<(i64, Option<String>)> =
        sqlx::query_as("SELECT iid, head_sha FROM forge.merge_requests WHERE id = $1")
            .bind(mr.get())
            .fetch_optional(&mut *lock.tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
    let Some((iid, head)) = head else {
        return Ok(None);
    };
    let versions = versions_on(&mut lock.tx, mr, "", QUERY).await?;
    let head_sha = head.as_deref().map(ObjectId::parse).transpose()?;
    Ok(Some(VersionsSnapshot {
        merge_request: mr,
        iid,
        head_sha,
        versions,
    }))
}

/// What a purge did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeOutcome {
    pub merge_request: StoredMergeRequest,
    /// Marked by this purge.
    pub purged: Vec<i64>,
    /// Marked before (asked again: nothing written for them).
    pub already_purged: Vec<i64>,
}

/// `FG-REVIEW-0002`: who asked, on which merge request, why.
pub(super) fn precondition(
    reason: &'static str,
    mr: &StoredMergeRequest,
    version: i64,
) -> ForgeError {
    ForgeError::new(ErrorCode::PurgePreconditionFailed)
        .with_context("reason", reason)
        .with_context("project_id", mr.project_id.get())
        .with_context("iid", mr.iid)
        .with_context("merge_request_id", mr.id.get())
        .with_context("version", version)
}

/// Mark the refs of `versions` (`None` = every version) of merge request
/// `iid` of `project` as purged, for `viewer`, decided inside the
/// transaction (`decide` sees the facts read under the locks, like
/// `close_mr_decided`). After `decide`: `FG-REPO-0005
/// resource=merge_request` for an absent one; `FG-WEB-0006
/// param_invalid` (`param=versions`, `rule=unknown_version`) for a number
/// the merge request does not have; `FG-REVIEW-0002
/// reason=purge_requires_closed_or_rewritten` when it is open and its
/// current head is among them (the next version job would bring it back).
/// One audit row `merge_request.version_refs_purged` (no event); nothing
/// when every asked version was purged before.
pub async fn purge_mr_version_refs_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    viewer: UserId,
    versions: Option<&[i64]>,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(PurgeOutcome, P), ForgeError> {
    const ACTION: &str = "merge_request.version_refs_purged";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.merge_request.ok_or_else(absent)?;
        // M3 R4 review: a merge in progress keeps its versions' refs until it
        // ends (FG-REVIEW-0011): the merge reads the head they hold.
        crate::review::merges::refuse_live_merge(&mut tx, current.id, ACTION).await?;
        let id = current.id;
        let stored = tx
            .step(|c| async move { versions_on(c, id, "FOR UPDATE", ACTION).await })
            .await?;
        let asked: Vec<&StoredVersion> = match versions {
            None => stored.iter().collect(),
            Some(numbers) => {
                let mut picked = Vec::with_capacity(numbers.len());
                for n in numbers {
                    let v = stored.iter().find(|v| v.version_no == *n).ok_or_else(|| {
                        ForgeError::new(ErrorCode::RequestInvalid)
                            .with_context("reason", "param_invalid")
                            .with_context("param", "versions")
                            .with_context("rule", "unknown_version")
                            .with_context("version", *n)
                    })?;
                    if !picked.contains(&v) {
                        picked.push(v);
                    }
                }
                picked
            }
        };
        if current.state == MergeRequestState::Open {
            if let Some(head) = asked
                .iter()
                .find(|v| Some(v.head_sha.as_str()) == current.head_sha.as_deref())
            {
                return Err(precondition(
                    "purge_requires_closed_or_rewritten",
                    &current,
                    head.version_no,
                ));
            }
        }
        let (already, fresh): (Vec<&StoredVersion>, Vec<&StoredVersion>) =
            asked.into_iter().partition(|v| v.refs_purged);
        let mut purged: Vec<i64> = fresh.iter().map(|v| v.version_no).collect();
        purged.sort_unstable();
        let mut already_purged: Vec<i64> = already.iter().map(|v| v.version_no).collect();
        already_purged.sort_unstable();
        if purged.is_empty() {
            return Ok(Settle::Discard((
                PurgeOutcome {
                    merge_request: current,
                    purged,
                    already_purged,
                },
                decided,
            )));
        }
        let numbers = purged.clone(); // clone: the outcome keeps the list too
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.merge_request_versions SET refs_purged_at = now() \
                 WHERE merge_request_id = $1 AND version_no = ANY($2) AND refs_purged_at IS NULL",
            )
            .bind(id.get())
            .bind(&numbers)
            .execute(c)
            .await
            .map(drop)
            .map_err(|e| query_error("review.version_refs_purge", e))
        })
        .await?;
        // M3 R2: a stored check of a purged head names a commit nobody may
        // read any more: back to `unchecked`, its conflict list emptied.
        let numbers = purged.clone(); // clone: the outcome keeps the list too
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.merge_request_mergeability g SET status = 'unchecked', \
                 reason = NULL, conflicts = '{}', conflicts_truncated = false, \
                 updated_at = now() FROM forge.merge_request_versions v \
                 WHERE g.merge_request_id = $1 AND v.merge_request_id = $1 \
                 AND v.version_no = ANY($2) AND g.checked_head_sha = v.head_sha",
            )
            .bind(id.get())
            .bind(&numbers)
            .execute(c)
            .await
            .map(drop)
            .map_err(|e| query_error("review.version_refs_purge_mergeability", e))
        })
        .await?;
        let details =
            AuditDetails::MergeRequestVersion(MergeRequestVersionChange::VersionRefsPurged {
                merge_request: id,
                iid: current.iid,
                versions: purged.clone(), // clone: the outcome keeps the list too
            });
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
                PurgeOutcome {
                    merge_request: current,
                    purged,
                    already_purged,
                },
                decided,
            ),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
