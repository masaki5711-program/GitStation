//! The AI asset draft's reads (plan 17 P17-10, ADR-0039 Amendment
//! P17-10): which draft improvement merge requests may get a draft, and
//! the facts its write is decided on. The rows it writes are the gateway's
//! (`llm_requests` / `llm_advice`, purpose `ai_asset_draft`); the draft
//! itself is a branch in git.

use forge_core::{MergeRequestId, ProjectId};
use forge_errors::ForgeError;

use crate::{query_error, read_query_error, Db};

/// The most candidates one pass reads.
pub const MAX_CANDIDATES: i64 = 10;

/// A draft improvement merge request that may get a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub merge_request: MergeRequestId,
    pub project: ProjectId,
    pub iid: i64,
    pub title: String,
    pub description: String,
    pub source_branch: String,
    pub target_branch: String,
    pub head_sha: String,
}

/// The open draft merge requests from `refs/heads/improve/*` with a head,
/// in projects whose `ai_assets.enabled` is on and that are not being
/// deleted, least recently updated first, at most `limit` (read pool).
pub async fn candidates(db: &Db, limit: i64) -> Result<Vec<Candidate>, ForgeError> {
    const QUERY: &str = "llm.asset_draft.candidates";
    type Row = (i64, i64, i64, String, String, String, String, String);
    let rows: Vec<Row> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT m.id, m.project_id, m.iid, m.title, m.description, m.source_branch, \
                   m.target_branch, m.head_sha \
                 FROM forge.merge_requests m \
                 JOIN forge.project_ai_assets a ON a.project_id = m.project_id AND a.enabled \
                 JOIN forge.projects p ON p.id = m.project_id AND p.deleted_at IS NULL \
                 WHERE m.state = 'open' AND m.draft AND m.head_sha IS NOT NULL \
                   AND m.source_branch LIKE 'refs/heads/improve/_%' \
                 ORDER BY m.updated_at, m.id LIMIT $1",
            )
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| Candidate {
            merge_request: MergeRequestId::new(r.0),
            project: ProjectId::new(r.1),
            iid: r.2,
            title: r.3,
            description: r.4,
            source_branch: r.5,
            target_branch: r.6,
            head_sha: r.7,
        })
        .collect())
}

/// The facts the draft write is decided on, read in one statement on the
/// write pool (fresh) right before the write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetFacts {
    pub mr_open: bool,
    pub mr_draft: bool,
    pub source_branch: String,
    pub head_sha: Option<String>,
    /// The project's `ai_assets.enabled` now.
    pub ai_assets_enabled: bool,
    /// The project is being deleted now.
    pub pending_deletion: bool,
}

/// The facts of merge request `mr` of `project` (`None`: no such merge
/// request).
pub async fn target_facts(
    db: &Db,
    project: ProjectId,
    mr: MergeRequestId,
) -> Result<Option<TargetFacts>, ForgeError> {
    const QUERY: &str = "llm.asset_draft.target_facts";
    type Row = (bool, bool, String, Option<String>, Option<bool>, bool);
    let row: Option<Row> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT m.state = 'open', m.draft, m.source_branch, m.head_sha, a.enabled, \
                   p.deleted_at IS NOT NULL \
                 FROM forge.merge_requests m \
                 JOIN forge.projects p ON p.id = m.project_id \
                 LEFT JOIN forge.project_ai_assets a ON a.project_id = m.project_id \
                 WHERE m.id = $1 AND m.project_id = $2",
            )
            .bind(mr.get())
            .bind(project.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(|r| TargetFacts {
        mr_open: r.0,
        mr_draft: r.1,
        source_branch: r.2,
        head_sha: r.3,
        ai_assets_enabled: r.4.unwrap_or(false),
        pending_deletion: r.5,
    }))
}

/// The draft requests already made about one merge request and input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Asks {
    /// Queued or running.
    pub live: i64,
    /// Ended (succeeded, failed or abandoned) within the last `recent_secs`.
    pub recent: i64,
    /// Every request of this input.
    pub total: i64,
}

/// [`Asks`] of `input_sha256` about merge request `mr`. An answered
/// request does not end the asking by itself: only the draft branch does
/// (an answer whose commit failed, or that proposed nothing acceptable, is
/// asked again within the bounds).
pub async fn asks(
    db: &Db,
    mr: MergeRequestId,
    input_sha256: &str,
    recent_secs: i64,
) -> Result<Asks, ForgeError> {
    const QUERY: &str = "llm.asset_draft.asks";
    let (live, recent, total): (i64, i64, i64) = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT count(*) FILTER (WHERE state IN ('queued', 'running')), \
                 count(*) FILTER (WHERE state IN ('succeeded', 'failed', 'abandoned') \
                 AND finished_at > now() - make_interval(secs => $3)), \
                 count(*) \
                 FROM forge.llm_requests \
                 WHERE purpose = 'ai_asset_draft' AND subject_kind = 'merge_request' \
                 AND subject_id = $1 AND input_sha256 = $2",
            )
            .bind(mr.get())
            .bind(input_sha256)
            .bind(recent_secs as f64)
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(Asks {
        live,
        recent,
        total,
    })
}
