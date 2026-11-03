//! A person links or unlinks a work item on a merge request (M3 I1b, plan
//! 02 §2 "MR の画面から手動でリンクを付け外しできる", `WorkItemLink`). Like the
//! other review commands: decided inside the transaction on the rows as
//! locked (project, then the merge request, then the caller), called by
//! forge-review only (`only_forge_review_calls_the_review_commands`).
//!
//! The work item is the reference in the project's binding (02 §2): a
//! project that is not bound is `FG-INT-0018`. A link writes a `manual`
//! row of `forge.merge_request_work_items` with one `work_item.linked`
//! audit row on the project and its event (deduplicated by
//! `work_item_linked:<link_id>`); an unlink ends the open row (`manual`,
//! the person) with `work_item.unlinked`. The row stays (history), and
//! detection (`work-item-sync`) does not link that work item to this merge
//! request again until a person does.

use forge_core::ids::{ProjectId, UserId};
use forge_core::{AuditDetails, DedupKey, DedupPart, LinkSource, WorkItemChange, WorkItemRef};
use forge_errors::{ErrorCode, ForgeError};

use super::commands::{absent, facts, MergeRequestFacts};
use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome, Settle,
};
use crate::work_items::{binding_of, open_links, work_item, OpenLink, MAX_MANUAL_LINKS};
use crate::{query_error, Db};

/// What a link or an unlink did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The link was made (or ended): one audit row and one event.
    Changed,
    /// The work item was already linked: nothing written.
    Unchanged,
}

/// The link a command made, ended or found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkAnswer {
    pub link: i64,
    pub work_item: forge_core::WorkItem,
    pub source: LinkSource,
}

fn not_bound(project: ProjectId) -> ForgeError {
    ForgeError::new(ErrorCode::WorkItemProjectNotBound)
        .with_context("reason", "project_not_bound")
        .with_context("project_id", project.get())
}

fn record(change: WorkItemChange) -> Result<ChangeRecord, ForgeError> {
    let project = change.project();
    let merge_request = change.merge_request();
    let dedup_key = DedupKey::new(change.dedup_prefix(), &[DedupPart::Id(change.link())])?;
    let details = AuditDetails::WorkItem(change);
    let event_type = details.event_type().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "audit_only_event")
            .with_context("action", details.action())
    })?;
    let value = details.to_value();
    Ok(ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: value.clone(), // clone: the same JSON is the event payload
        },
        affected: Vec::new(),
        events: vec![DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: "merge_request",
            aggregate_id: merge_request.get(),
            project_id: Some(project),
            payload: value,
            dedup_key: Some(dedup_key),
        }],
    })
}

/// Link work item `reference` to merge request `iid` of `project` as
/// `viewer`, decided inside the transaction. After `decide`:
/// `FG-REPO-0005 resource=merge_request` for an absent one;
/// `FG-INT-0018` when the project is not bound to an integration;
/// [`LinkOutcome::Unchanged`] when the work item is linked already (by
/// detection or a person); `FG-INT-0019` when the merge request has
/// [`MAX_MANUAL_LINKS`] open manual links.
pub async fn link_work_item_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    reference: &WorkItemRef,
    viewer: UserId,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(LinkOutcome, LinkAnswer, P), ForgeError> {
    const ACTION: &str = "work_item.linked";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.merge_request.ok_or_else(absent)?;
        let bound = tx
            .step(|c| binding_of(c, project))
            .await?
            .ok_or_else(|| not_bound(project))?;
        let item = tx.step(|c| work_item(c, &bound, reference)).await?;
        let mr = current.id;
        let open: Vec<OpenLink> = tx.step(|c| open_links(c, mr)).await?;
        if let Some(found) = open.iter().find(|l| l.work_item.id == item.id) {
            let answer = LinkAnswer {
                link: found.link,
                work_item: found.work_item.clone(), // clone: the answer owns it
                source: found.source,
            };
            return Ok(Settle::Discard((LinkOutcome::Unchanged, answer, decided)));
        }
        let manual = i64::try_from(
            open.iter()
                .filter(|l| l.source == LinkSource::Manual)
                .count(),
        )
        .unwrap_or(i64::MAX);
        if manual >= MAX_MANUAL_LINKS {
            return Err(ForgeError::new(ErrorCode::WorkItemLinkLimitReached)
                .with_context("reason", "manual_link_limit")
                .with_context("limit", MAX_MANUAL_LINKS)
                .with_context("project_id", project.get())
                .with_context("merge_request_id", mr.get()));
        }
        let item_id = item.id;
        let link: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, \
                     work_item_id, source, linked_by) VALUES ($1, $2, $3, 'manual', $4) \
                     RETURNING id",
                )
                .bind(mr.get())
                .bind(project.get())
                .bind(item_id)
                .bind(viewer.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("review.work_item_link", e))
            })
            .await?;
        let answer = LinkAnswer {
            link,
            work_item: item.clone(), // clone: the change owns one
            source: LinkSource::Manual,
        };
        let change = WorkItemChange::Linked {
            project,
            merge_request: mr,
            iid: current.iid,
            link,
            work_item: item,
            source: LinkSource::Manual,
            by: Some(viewer),
        };
        Ok(Settle::Commit(
            record(change)?,
            (LinkOutcome::Changed, answer, decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// End the open link of work item `reference` on merge request `iid` of
/// `project` as `viewer` (a detected or a person's link), decided inside
/// the transaction. After `decide`: `FG-REPO-0005 resource=merge_request`
/// for an absent one; `FG-INT-0018` when the project is not bound;
/// `FG-REPO-0005 resource=work_item_link` when the work item is not linked
/// to it now.
pub async fn unlink_work_item_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    reference: &WorkItemRef,
    viewer: UserId,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(LinkAnswer, P), ForgeError> {
    const ACTION: &str = "work_item.unlinked";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.merge_request.ok_or_else(absent)?;
        let bound = tx
            .step(|c| binding_of(c, project))
            .await?
            .ok_or_else(|| not_bound(project))?;
        let mr = current.id;
        let open: Vec<OpenLink> = tx.step(|c| open_links(c, mr)).await?;
        let found = open
            .into_iter()
            .find(|l| {
                l.work_item.integration == bound.integration
                    && l.work_item.external_project_id == bound.external_project_id
                    && &l.work_item.reference == reference
            })
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::NotFound)
                    .with_context("resource", "work_item_link")
                    .with_context("reason", "not_linked")
            })?;
        let link = found.link;
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.merge_request_work_items SET unlinked_at = now(), \
                 unlink_source = 'manual', unlinked_by = $2 \
                 WHERE id = $1 AND unlinked_at IS NULL",
            )
            .bind(link)
            .bind(viewer.get())
            .execute(c)
            .await
            .map_err(|e| query_error("review.work_item_unlink", e))
        })
        .await?;
        let answer = LinkAnswer {
            link,
            work_item: found.work_item.clone(), // clone: the change owns one
            source: found.source,
        };
        let change = WorkItemChange::Unlinked {
            project,
            merge_request: mr,
            iid: current.iid,
            link,
            work_item: found.work_item,
            source: LinkSource::Manual,
            by: Some(viewer),
        };
        Ok(Settle::Commit(record(change)?, (answer, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
