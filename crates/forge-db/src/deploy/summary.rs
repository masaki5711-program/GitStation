//! The approval summary's reads and its event (M5 L5-8, plan 19 §3.4,
//! ADR-0039 Amendment L5-8). Nothing here decides anything: forge-deploy
//! builds the facts from these reads, the worker asks the local LLM, and
//! the approval itself never reads any of it.
//!
//! - [`pending_for_summary`]: the deployments waiting for an approval
//!   inside their window (the worker's candidates).
//! - [`last_succeeded_sha`]: what the environment runs now (the base of
//!   the facts' range).
//! - [`newer_request`]: a later request of the same environment for
//!   another commit (the summary of the older one is then stale).
//! - [`merged_requests`] / [`completed_checks`]: the merge requests a
//!   range of commits landed and the completed checks of commits.
//! - [`record_approval_summarized`]: the `deployment.approval_summarized`
//!   event with its audit row (actor `system`), deduplicated per LLM
//!   request.

use forge_core::dedup_key::{DedupKey, DedupPart};
use forge_core::deploy::{DeployChange, DeployTier};
use forge_core::{AuditDetails, ProjectId, RequestId};
use forge_errors::ForgeError;

use super::{internal, RESOURCE};
use crate::change::{
    Actor, AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Committed, DomainEvent, Outcome,
};
use crate::{read_query_error, Db};

/// The most deployments one worker pass summarizes.
pub const MAX_SUMMARY_CANDIDATES: i64 = 20;
/// The most commits whose merges and checks are read at once.
pub const MAX_SHAS: usize = 500;

/// A deployment waiting for an approval, as the worker sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryCandidate {
    pub project: ProjectId,
    pub deployment: i64,
}

/// The deployments `pending_approval` inside their approval window,
/// oldest first, at most `limit` (≤ [`MAX_SUMMARY_CANDIDATES`]).
pub async fn pending_for_summary(db: &Db, limit: i64) -> Result<Vec<SummaryCandidate>, ForgeError> {
    const QUERY: &str = "deploy.pending_for_summary";
    let rows: Vec<(i64, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT d.project_id, d.id FROM forge.deployments d \
                   JOIN forge.deploy_environments e ON e.id = d.environment_id \
                 WHERE d.status = 'pending_approval' \
                   AND d.created_at + make_interval(mins => e.approval_ttl_minutes) > now() \
                 ORDER BY d.created_at, d.id LIMIT $1",
            )
            .bind(limit.clamp(1, MAX_SUMMARY_CANDIDATES))
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(p, d)| SummaryCandidate {
            project: ProjectId::new(p),
            deployment: d,
        })
        .collect())
}

/// The commit the environment `tier` of `project` last deployed with
/// success (`None`: never).
pub async fn last_succeeded_sha(
    db: &Db,
    project: ProjectId,
    tier: DeployTier,
) -> Result<Option<String>, ForgeError> {
    const QUERY: &str = "deploy.last_succeeded_sha";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT sha FROM forge.deployments WHERE project_id = $1 AND tier = $2 \
               AND status = 'succeeded' ORDER BY finished_at DESC, id DESC LIMIT 1",
        )
        .bind(project.get())
        .bind(tier.as_str())
        .fetch_optional(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// The commit of the newest request of the same environment made after
/// deployment `id` for another commit (`None`: none).
pub async fn newer_request(
    db: &Db,
    project: ProjectId,
    tier: DeployTier,
    id: i64,
    sha: &str,
) -> Result<Option<String>, ForgeError> {
    const QUERY: &str = "deploy.newer_request";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT sha FROM forge.deployments WHERE project_id = $1 AND tier = $2 \
               AND id > $3 AND sha <> $4 ORDER BY id DESC LIMIT 1",
        )
        .bind(project.get())
        .bind(tier.as_str())
        .bind(id)
        .bind(sha)
        .fetch_optional(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// A merge request whose applied merge made one of the commits asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedRequest {
    pub iid: i64,
    pub title: String,
    /// The head the merge applied (its checks are read there).
    pub head_sha: String,
}

/// The merge requests of `project` whose applied merge produced one of
/// `shas` (at most [`MAX_SHAS`] are looked at), by iid.
pub async fn merged_requests(
    db: &Db,
    project: ProjectId,
    shas: &[String],
) -> Result<Vec<MergedRequest>, ForgeError> {
    const QUERY: &str = "deploy.merged_requests";
    if shas.is_empty() {
        return Ok(Vec::new());
    }
    let shas: Vec<&str> = shas.iter().take(MAX_SHAS).map(String::as_str).collect();
    let rows: Vec<(i64, String, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT DISTINCT ON (mr.iid) mr.iid, mr.title, m.head_sha \
                 FROM forge.merge_request_merges m \
                   JOIN forge.merge_requests mr ON mr.id = m.merge_request_id \
                 WHERE m.project_id = $1 AND m.state = 'applied' AND m.new_sha = ANY($2) \
                 ORDER BY mr.iid, m.id DESC",
            )
            .bind(project.get())
            .bind(&shas)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(iid, title, head_sha)| MergedRequest {
            iid,
            title,
            head_sha,
        })
        .collect())
}

/// A completed check run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedCheck {
    pub head_sha: String,
    pub name: String,
    pub conclusion: String,
}

/// The completed check runs of `project` on `shas` (at most
/// [`MAX_SHAS`]), by commit and name.
pub async fn completed_checks(
    db: &Db,
    project: ProjectId,
    shas: &[String],
) -> Result<Vec<CompletedCheck>, ForgeError> {
    const QUERY: &str = "deploy.completed_checks";
    if shas.is_empty() {
        return Ok(Vec::new());
    }
    let shas: Vec<&str> = shas.iter().take(MAX_SHAS).map(String::as_str).collect();
    let rows: Vec<(String, String, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT head_sha, name, conclusion FROM forge.check_runs \
                 WHERE project_id = $1 AND head_sha = ANY($2) AND status = 'completed' \
                 ORDER BY head_sha, name",
            )
            .bind(project.get())
            .bind(&shas)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(head_sha, name, conclusion)| CompletedCheck {
            head_sha,
            name,
            conclusion,
        })
        .collect())
}

/// What the worker's approval summary of one deployment came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryOutcome {
    pub project: ProjectId,
    pub deployment: i64,
    pub tier: DeployTier,
    pub sha: String,
    /// The advice written (`None`: the LLM failed).
    pub advice: Option<i64>,
    /// The gateway's request row (`None`: the gateway failed before it
    /// wrote one — Ollama unreachable, the model stopped, the deadline).
    pub llm_request: Option<i64>,
    /// Without a request row, the dedup key's part: the worker's attempt
    /// time in whole minutes since the epoch (one event per minute at most).
    pub attempt_minute: i64,
    pub error_code: Option<String>,
}

/// Record `deployment.approval_summarized` (actor `system`) with its audit
/// row. `false`: this LLM request's (or, without one, this minute's)
/// event was recorded already (nothing
/// written). The approval, the deployment and its approvals are not
/// touched (`summary_never_changes_approval_state`).
pub async fn record_approval_summarized(
    db: &Db,
    request_id: &RequestId,
    outcome: &SummaryOutcome,
) -> Result<bool, ForgeError> {
    const PREFIX: &str = "deployment_approval_summarized";
    let change = DeployChange::ApprovalSummarized {
        project: outcome.project,
        deployment: outcome.deployment,
        tier: outcome.tier,
        sha: outcome.sha.clone(), // clone: the event owns it
        advice: outcome.advice,
        llm_request: outcome.llm_request,
        error_code: outcome.error_code.clone(), // clone: as sha
    };
    let event_type = change
        .event_type()
        .ok_or_else(|| internal("approval_summary_without_event"))?;
    let dedup = match outcome.llm_request {
        Some(req) => DedupKey::new(
            PREFIX,
            &[DedupPart::Id(outcome.deployment), DedupPart::Id(req)],
        )?,
        None => DedupKey::new(
            PREFIX,
            &[
                DedupPart::Id(outcome.deployment),
                DedupPart::Word("no_request"),
                DedupPart::Id(outcome.attempt_minute),
            ],
        )?,
    };
    let details = AuditDetails::Deploy(change);
    let value = details.to_value();
    let record = ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(outcome.project.get()),
            project_id: Some(outcome.project),
            error_code: None,
            details: value.clone(), // clone: the same JSON is the payload
        },
        affected: Vec::new(),
        events: vec![DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: "deployment",
            aggregate_id: outcome.deployment,
            project_id: Some(outcome.project),
            payload: value,
            dedup_key: Some(dedup),
        }],
    };
    let ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    let tx = ChangeTx::begin(db, &ctx).await?;
    match tx.commit(record).await {
        Ok(Committed::Recorded { .. }) => Ok(true),
        Ok(Committed::Deduped) => Ok(false),
        Err(e) => Err(e.with_context("step", "approval_summarized")),
    }
}
