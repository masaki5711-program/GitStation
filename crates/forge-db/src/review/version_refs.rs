//! The hidden refs of the versions (M3 R1b, ADR-0024 §2): the lock under
//! which forge-review writes them, and the administrator's purge.
//!
//! [`with_versions_locked`] holds `FOR SHARE` locks on a merge request's
//! version rows while forge-review reads the refs and runs ONE `update-ref`
//! transaction (bounded by its own timeout). The purge's `UPDATE … SET
//! refs_purged_at` needs those rows' locks, so a ref sync and a purge never
//! interleave: a sync that read a version as not purged writes its ref
//! before the purge commits, and the purge's own sync (after its commit)
//! deletes it; a sync that starts later reads the mark and never writes
//! the ref (ADR-0024 closing table #42). The merge request row is not
//! locked: edits are never held up by git.
//!
//! [`purge_mr_version_refs_decided`] marks the versions as purged
//! (`merge_request.version_refs_purged`, audit-only), decided inside the
//! transaction like every review command; forge-review then deletes the
//! refs through the same ref sync.

use std::future::Future;
use std::time::Duration;

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{AuditDetails, MergeRequestState, MergeRequestVersionChange, ObjectId};
use forge_errors::{ErrorCode, ForgeError};

use super::commands::{absent, facts, MergeRequestFacts};
use super::versions::{versions_on, StoredVersion};
use super::StoredMergeRequest;
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::{query_error, Db};

/// What a ref sync sees: the merge request's number and head, and every
/// version (locked `FOR SHARE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionsSnapshot {
    pub merge_request: MergeRequestId,
    pub iid: i64,
    pub head_sha: Option<ObjectId>,
    pub versions: Vec<StoredVersion>,
}

/// The longest wait for the version rows' locks (a purge in progress).
const LOCK_TIMEOUT: &str = "5s";

/// Run `write` with `mr`'s version rows locked `FOR SHARE` (module docs),
/// the whole of it bounded by `timeout` (`FG-DB-0002
/// reason=version_refs_timeout`). `Ok(None)`: the merge request is gone.
pub async fn with_versions_locked<T, F, Fut>(
    db: &Db,
    mr: MergeRequestId,
    timeout: Duration,
    write: F,
) -> Result<Option<T>, ForgeError>
where
    F: FnOnce(VersionsSnapshot) -> Fut,
    Fut: Future<Output = Result<T, ForgeError>>,
{
    const QUERY: &str = "review.version_refs";
    let run = async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(&format!("SET LOCAL lock_timeout = '{LOCK_TIMEOUT}'"))
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let head: Option<(i64, Option<String>)> =
            sqlx::query_as("SELECT iid, head_sha FROM forge.merge_requests WHERE id = $1")
                .bind(mr.get())
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| query_error(QUERY, e))?;
        let Some((iid, head)) = head else {
            return Ok(None);
        };
        let versions = versions_on(&mut tx, mr, "FOR SHARE", QUERY).await?;
        let head_sha = head.as_deref().map(ObjectId::parse).transpose()?;
        let out = write(VersionsSnapshot {
            merge_request: mr,
            iid,
            head_sha,
            versions,
        })
        .await?;
        // Nothing was written: the commit only releases the locks.
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Some(out))
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::DbQueryFailed)
            .with_context("query", QUERY)
            .with_context("reason", "version_refs_timeout")
            .with_context("timeout_ms", timeout.as_millis())
            .with_context("merge_request_id", mr.get())),
    }
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
