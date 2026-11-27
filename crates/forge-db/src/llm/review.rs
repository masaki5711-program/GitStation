//! The merge request first pass's reads (M5 L5-5, plan 19 §3.1): which
//! open merge requests the worker reviews, and their head now.
//!
//! - [`review_candidates`]: open, non-draft merge requests of projects
//!   whose `llm.review` is `advisory` or `required` (never `off`: no row at
//!   all), whose current head's version the version job has recorded (its
//!   target tip known, so its merge base — or that it has none — is
//!   known: a head not ready yet never crowds the others out), whose
//!   current head has no `forge/ai-review` run yet — or a run
//!   still waiting longer than `stale_secs` (a worker stopped while it
//!   waited). A completed run (success, neutral, failure, error) is not
//!   asked again on the same head — a new head is — unless a person asked
//!   for a rerun of that head after the run was written
//!   (`forge.llm_review_reruns`, `ops::request_rerun`): the worker's
//!   `queued` write makes the run newer than the rerun, so one rerun asks
//!   once.
//! - [`current_head`]: the merge request's head and state now (the worker
//!   drops an answer for a head the merge request moved on from).

use forge_core::checks::AI_REVIEW_CHECK;
use forge_core::llm::ReviewMode;
use forge_core::{MergeRequestId, ProjectId};
use forge_errors::ForgeError;

use super::stored_invalid;
use crate::{read_query_error, Db};

/// The most merge requests one worker pass looks at.
pub const MAX_REVIEW_CANDIDATES: i64 = 20;

/// A merge request whose head waits for its first pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewCandidate {
    pub mr: MergeRequestId,
    pub project: ProjectId,
    pub iid: i64,
    pub head_sha: String,
    pub mode: ReviewMode,
}

/// The candidates (module docs), the least recently updated first, at
/// most `limit` (≤ [`MAX_REVIEW_CANDIDATES`]).
pub async fn review_candidates(
    db: &Db,
    limit: i64,
    stale_secs: i64,
) -> Result<Vec<ReviewCandidate>, ForgeError> {
    const QUERY: &str = "llm.review_candidates";
    let rows: Vec<(i64, i64, i64, String, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT m.id, m.project_id, m.iid, m.head_sha, l.review \
                 FROM forge.project_llm l \
                   JOIN forge.projects p ON p.id = l.project_id AND p.deleted_at IS NULL \
                   JOIN forge.merge_requests m ON m.project_id = l.project_id \
                 WHERE l.review IN ('advisory', 'required') \
                   AND m.state = 'open' AND NOT m.draft AND m.head_sha IS NOT NULL \
                   AND EXISTS ( \
                     SELECT 1 FROM forge.merge_request_versions v \
                     WHERE v.merge_request_id = m.id AND v.head_sha = m.head_sha \
                       AND v.base_sha IS NOT NULL) \
                   AND NOT EXISTS ( \
                     SELECT 1 FROM forge.check_runs c \
                     WHERE c.project_id = m.project_id AND c.head_sha = m.head_sha \
                       AND c.name = $2 \
                       AND ((c.status = 'completed' AND NOT EXISTS ( \
                              SELECT 1 FROM forge.llm_review_reruns r \
                              WHERE r.merge_request_id = m.id AND r.head_sha = m.head_sha \
                                AND r.requested_at > c.updated_at)) \
                            OR (c.status <> 'completed' \
                                AND c.updated_at > now() - make_interval(secs => $3)))) \
                 ORDER BY m.updated_at, m.id LIMIT $1",
            )
            .bind(limit.clamp(1, MAX_REVIEW_CANDIDATES))
            .bind(AI_REVIEW_CHECK)
            .bind(stale_secs.max(1) as f64)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(|(mr, project, iid, head_sha, mode)| {
            Ok(ReviewCandidate {
                mr: MergeRequestId::new(mr),
                project: ProjectId::new(project),
                iid,
                head_sha,
                mode: ReviewMode::parse(&mode).ok_or_else(|| stored_invalid("review"))?,
            })
        })
        .collect()
}

/// The head of merge request `mr` now, and whether it is still open
/// (`None`: gone).
pub async fn current_head(
    db: &Db,
    mr: MergeRequestId,
) -> Result<Option<(Option<String>, bool)>, ForgeError> {
    const QUERY: &str = "llm.review_current_head";
    let row: Option<(Option<String>, String)> = db
        .within(QUERY, async {
            sqlx::query_as("SELECT head_sha, state FROM forge.merge_requests WHERE id = $1")
                .bind(mr.get())
                .fetch_optional(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(|(head, state)| (head, state == "open")))
}

#[cfg(test)]
#[path = "review_tests.rs"]
mod tests;
