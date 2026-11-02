//! Keeping decided statuses honest (M3 R2 re-review): the worker's sweep
//! visits decided rows — `can_be_merged`, `cannot_be_merged`, or a
//! `check_failed` with a deterministic reason — of open merge requests, the
//! least recently verified first ([`decided_candidates`]), reads the live
//! target tip with git, and either re-requests the check (the pair moved:
//! a dead-lettered push event, a target push past the deferral bound) or
//! records that the pair is still live ([`confirm_live`]). See the module
//! documentation of `mergeability`.

use forge_core::ids::{MergeRequestId, ProjectId};
use forge_core::{ObjectId, RefName, RepoId};
use forge_errors::ForgeError;

use super::{stored_invalid, ANSWERING, LIVE_JOB};
use crate::{query_error, Db};

/// A decided row the sweep verifies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecidedRow {
    pub merge_request: MergeRequestId,
    pub project: ProjectId,
    pub repository: RepoId,
    pub target_branch: RefName,
    /// The merge request's head now.
    pub head: Option<ObjectId>,
    pub checked_head: Option<ObjectId>,
    pub checked_target: Option<ObjectId>,
}

impl DecidedRow {
    /// Whether the checked pair is (`head`, `tip`).
    pub fn is_live(&self, tip: Option<&ObjectId>) -> bool {
        self.checked_head == self.head && self.checked_target.as_ref() == tip
    }
}

type DecidedTuple = (
    i64,
    i64,
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// At most `limit` decided rows of open merge requests in ready
/// repositories with no live job, the least recently verified first.
pub async fn decided_candidates(db: &Db, limit: i64) -> Result<Vec<DecidedRow>, ForgeError> {
    const QUERY: &str = "review.mergeability_decided";
    let rows: Vec<DecidedTuple> = db
        .within(QUERY, async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error(QUERY, e))?;
            sqlx::query_as(&format!(
                "SELECT g.merge_request_id, g.project_id, r.id, m.target_branch, m.head_sha, \
                 g.checked_head_sha, g.checked_target_sha \
                 FROM forge.merge_request_mergeability g \
                 JOIN forge.merge_requests m ON m.id = g.merge_request_id \
                 JOIN forge.repositories r ON r.project_id = m.project_id \
                 WHERE m.state = 'open' AND r.state = 'ready' AND {ANSWERING} \
                 AND NOT {LIVE_JOB} \
                 ORDER BY COALESCE(g.confirmed_at, g.checked_at) NULLS FIRST, g.merge_request_id \
                 LIMIT $1"
            ))
            .bind(limit)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(
            |(mr, project, repo, target, head, checked_head, checked_target)| {
                let mr = MergeRequestId::new(mr);
                let oid = |raw: Option<String>| {
                    raw.map(|r| {
                        ObjectId::parse(&r)
                            .map_err(|_| stored_invalid("stored_checked_sha_invalid", mr))
                    })
                    .transpose()
                };
                Ok(DecidedRow {
                    merge_request: mr,
                    project: ProjectId::new(project),
                    repository: RepoId::new(repo),
                    target_branch: RefName::parse(&target)
                        .map_err(|_| stored_invalid("stored_branch_invalid", mr))?,
                    head: oid(head)?,
                    checked_head: oid(checked_head)?,
                    checked_target: oid(checked_target)?,
                })
            },
        )
        .collect()
}

/// Record that the checked pair of `rows` was still live (`confirmed_at =
/// now()`), for the rows whose status did not change meanwhile.
pub async fn confirm_live(db: &Db, rows: &[MergeRequestId]) -> Result<u64, ForgeError> {
    const QUERY: &str = "review.mergeability_confirm";
    if rows.is_empty() {
        return Ok(0);
    }
    let ids: Vec<i64> = rows.iter().map(|m| m.get()).collect();
    db.within(QUERY, async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(&format!(
            "UPDATE forge.merge_request_mergeability g SET confirmed_at = now() \
             WHERE g.merge_request_id = ANY($1) AND {ANSWERING}"
        ))
        .bind(&ids)
        .execute(&mut *conn)
        .await
        .map(|r| r.rows_affected())
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}
