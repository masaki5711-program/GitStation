//! The context API's reads of work items (M3 X5, plan 03 §6, ADR-0023
//! Amendment X5), on the read pool: one work item of a project's binding
//! with its confidential mark (plan 06 D-5) and the project's merge
//! requests linked to it, and the marks of a merge request's work items.
//! The caller decided `ContextRead` / `MergeRequestRead` on the project.

use std::collections::BTreeSet;

use forge_core::ids::{IntegrationId, ProjectId};
use forge_core::{IntegrationKind, LinkSource, MergeRequestState, WorkItemRef};
use forge_errors::{ErrorCode, ForgeError};

use super::binding_of;
use crate::{read_query_error, Db};

/// A work item forge knows of (it was named or linked once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownWorkItem {
    pub id: i64,
    /// kanflow marked it confidential (`forge.work_items.confidential`).
    pub confidential: bool,
    /// UTC, RFC 3339: when forge first saw it.
    pub created_at: String,
}

/// A merge request of the project with an open link to the work item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedMergeRequest {
    pub iid: i64,
    pub title: String,
    pub state: MergeRequestState,
    pub draft: bool,
    /// Full ref names (`refs/heads/…`).
    pub source_branch: String,
    pub target_branch: String,
    /// `None` before its first version.
    pub head_sha: Option<String>,
    pub link_source: LinkSource,
    /// UTC, RFC 3339.
    pub linked_at: String,
}

/// One work item of a project's binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemContext {
    pub integration: IntegrationId,
    pub kind: IntegrationKind,
    pub external_project_id: String,
    /// `None`: forge never saw the reference (nothing links it).
    pub work_item: Option<KnownWorkItem>,
    /// Newest first, at most the limit.
    pub merge_requests: Vec<LinkedMergeRequest>,
    /// More merge requests are linked than the limit.
    pub truncated: bool,
}

type MrRow = (
    i64,
    String,
    String,
    bool,
    String,
    String,
    Option<String>,
    String,
    String,
);

fn stored_invalid(field: &'static str, iid: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "stored_work_item_context_invalid")
        .with_context("field", field)
        .with_context("iid", iid)
}

/// `reference` of `project`'s binding (`None`: the project is not bound),
/// with the project's merge requests openly linked to it (at most `limit`,
/// newest first).
pub async fn work_item_context(
    db: &Db,
    project: ProjectId,
    reference: &WorkItemRef,
    limit: usize,
) -> Result<Option<WorkItemContext>, ForgeError> {
    const QUERY: &str = "work_items.context";
    let fetch = i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1);
    db.within(QUERY, async {
        let mut conn = db.acquire().await?;
        let Some(bound) = binding_of(&mut conn, project)
            .await
            .map_err(|e| e.with_context("operation", QUERY))?
        else {
            return Ok(None);
        };
        let item: Option<(i64, bool, String)> = sqlx::query_as(
            "SELECT id, confidential, \
             to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
             FROM forge.work_items \
             WHERE integration_id = $1 AND external_project_id = $2 AND ref = $3",
        )
        .bind(bound.integration.get())
        .bind(&bound.external_project_id)
        .bind(reference.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let mut merge_requests = Vec::new();
        let mut truncated = false;
        if let Some((id, _, _)) = &item {
            let rows: Vec<MrRow> = sqlx::query_as(
                "SELECT m.iid, m.title, m.state, m.draft, m.source_branch, m.target_branch, \
                 m.head_sha, l.source, \
                 to_char(l.linked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 FROM forge.merge_request_work_items AS l \
                 JOIN forge.merge_requests AS m ON m.id = l.merge_request_id \
                 WHERE l.work_item_id = $1 AND l.project_id = $2 AND l.unlinked_at IS NULL \
                 ORDER BY m.iid DESC LIMIT $3",
            )
            .bind(id)
            .bind(project.get())
            .bind(fetch)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            truncated = rows.len() > limit;
            for row in rows.into_iter().take(limit) {
                let (iid, title, state, draft, source, target, head, link, linked_at) = row;
                merge_requests.push(LinkedMergeRequest {
                    iid,
                    title,
                    state: MergeRequestState::parse(&state)
                        .ok_or_else(|| stored_invalid("state", iid))?,
                    draft,
                    source_branch: source,
                    target_branch: target,
                    head_sha: head,
                    link_source: LinkSource::parse(&link)
                        .ok_or_else(|| stored_invalid("source", iid))?,
                    linked_at,
                });
            }
        }
        Ok(Some(WorkItemContext {
            integration: bound.integration,
            kind: bound.kind,
            external_project_id: bound.external_project_id,
            work_item: item.map(|(id, confidential, created_at)| KnownWorkItem {
                id,
                confidential,
                created_at,
            }),
            merge_requests,
            truncated,
        }))
    })
    .await
}

/// Which of the work items `ids` are marked confidential.
pub async fn confidential_work_items(db: &Db, ids: &[i64]) -> Result<BTreeSet<i64>, ForgeError> {
    const QUERY: &str = "work_items.confidential";
    if ids.is_empty() {
        return Ok(BTreeSet::new());
    }
    db.within(QUERY, async {
        let mut conn = db.acquire().await?;
        let marked: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM forge.work_items \
             WHERE id = ANY($1::bigint[]) AND confidential",
        )
        .bind(ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(marked.into_iter().collect())
    })
    .await
}
