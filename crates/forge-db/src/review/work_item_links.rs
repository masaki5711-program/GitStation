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
//! who and their kind) with `work_item.unlinked`. The row stays (history).
//! The database requires those audit rows (trigger
//! `merge_request_work_items_audited`).
//!
//! I1b review: linking a work item that is linked by detection makes it
//! the person's (the detected interval ends as `converted`, the event
//! names it as `replaces_link_id`), so a later title edit does not end
//! it. Only a person's (`human`) unlink keeps detection from linking the
//! work item again; an agent's, a service account's or a bot's does not.
//! An unlink may name the work item by id (a link to an earlier binding's
//! item); the limit counts the current binding's links only.
//!
//! ADR-0016 Amendment 2026-11-12: each link or unlink also publishes one
//! `merge_request.work_items_changed` (`reason=manual`, the link added or
//! removed): kanflow follows `merge_request.*`, not `work_item.*`.

use forge_core::ids::{ProjectId, UserId};
use forge_core::{
    AuditDetails, DedupKey, DedupPart, LinkActorKind, LinkSource, LinkTarget, UnlinkSource,
    WorkItemChange, WorkItemRef, WORK_ITEMS_CHANGED_DEDUP,
};
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
    /// The link was made (or a detected one made the person's): one audit
    /// row and one event.
    Changed,
    /// The work item was already linked by hand: nothing written.
    Unchanged,
}

/// The link a command made, ended or found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkAnswer {
    pub link: i64,
    pub work_item: forge_core::WorkItem,
    pub source: LinkSource,
}

/// Who acts: the user and their kind (a person, an agent, …).
pub type LinkActor = (UserId, LinkActorKind);

fn not_bound(project: ProjectId) -> ForgeError {
    ForgeError::new(ErrorCode::WorkItemProjectNotBound)
        .with_context("reason", "project_not_bound")
        .with_context("project_id", project.get())
}

fn record(change: WorkItemChange) -> Result<ChangeRecord, ForgeError> {
    let project = change.project();
    let merge_request = change.merge_request();
    let dedup_key = DedupKey::new(change.dedup_prefix(), &[DedupPart::Id(change.link())])?;
    // ADR-0016 Amendment 2026-11-12: the `merge_request.*` signal of the
    // change, so an integration that follows merge requests (kanflow) reads
    // this one again; `work-item-sync` records its work item.
    let changed = changed_event(&change)?;
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
        events: vec![
            DomainEvent {
                event_type,
                schema_version: 1,
                aggregate_type: "merge_request",
                aggregate_id: merge_request.get(),
                project_id: Some(project),
                payload: value,
                dedup_key: Some(dedup_key),
            },
            changed,
        ],
    })
}

/// The `merge_request.work_items_changed` of a link change by hand,
/// deduplicated by `merge_request_work_items_changed:link:<id>:<linked|unlinked>`
/// (each link row is made once and ends once).
fn changed_event(change: &WorkItemChange) -> Result<DomainEvent, ForgeError> {
    let changed = change.to_changed();
    let verb = match change {
        WorkItemChange::Linked { .. } => "linked",
        WorkItemChange::Unlinked { .. } => "unlinked",
    };
    Ok(DomainEvent {
        event_type: changed.event_type(),
        schema_version: 1,
        aggregate_type: "merge_request",
        aggregate_id: changed.merge_request.get(),
        project_id: Some(changed.project),
        payload: changed.to_value(),
        dedup_key: Some(DedupKey::new(
            WORK_ITEMS_CHANGED_DEDUP,
            &[
                DedupPart::Word("link"),
                DedupPart::Id(change.link()),
                DedupPart::Word(verb),
            ],
        )?),
    })
}

/// End open link `link` (`unlink_source` = `source`) by `(by, kind)`.
async fn end_link(
    tx: &mut ChangeTx,
    link: i64,
    source: UnlinkSource,
    (by, kind): LinkActor,
) -> Result<(), ForgeError> {
    tx.step(|c| async move {
        sqlx::query(
            "UPDATE forge.merge_request_work_items SET unlinked_at = now(), \
             unlink_source = $2, unlinked_by = $3, unlinked_by_kind = $4 \
             WHERE id = $1 AND unlinked_at IS NULL",
        )
        .bind(link)
        .bind(source.as_str())
        .bind(by.get())
        .bind(kind.as_str())
        .execute(c)
        .await
        .map_err(|e| query_error("review.work_item_unlink", e))
    })
    .await
    .map(drop)
}

/// Link work item `reference` to merge request `iid` of `project` as
/// `actor`, decided inside the transaction. After `decide`:
/// `FG-REPO-0005 resource=merge_request` for an absent one;
/// `FG-INT-0018` when the project is not bound to an integration;
/// [`LinkOutcome::Unchanged`] when it is linked by hand already (a
/// detected link becomes the person's: `Changed`); `FG-INT-0019` when the
/// merge request has [`MAX_MANUAL_LINKS`] open manual links of the current
/// binding.
pub async fn link_work_item_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    reference: &WorkItemRef,
    actor: LinkActor,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(LinkOutcome, LinkAnswer, P), ForgeError> {
    const ACTION: &str = "work_item.linked";
    let (viewer, kind) = actor;
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
        let found = open.iter().find(|l| l.work_item.id == item.id);
        if let Some(found) = found.filter(|l| l.source == LinkSource::Manual) {
            let answer = LinkAnswer {
                link: found.link,
                work_item: found.work_item.clone(), // clone: the answer owns it
                source: found.source,
            };
            return Ok(Settle::Discard((LinkOutcome::Unchanged, answer, decided)));
        }
        let manual = open
            .iter()
            .filter(|l| l.source == LinkSource::Manual && bound.owns(&l.work_item))
            .count();
        if i64::try_from(manual).unwrap_or(i64::MAX) >= MAX_MANUAL_LINKS {
            return Err(ForgeError::new(ErrorCode::WorkItemLinkLimitReached)
                .with_context("reason", "manual_link_limit")
                .with_context("limit", MAX_MANUAL_LINKS)
                .with_context("project_id", project.get())
                .with_context("merge_request_id", mr.get()));
        }
        // A detected link of it becomes the person's (I1b review).
        let replaces = found.map(|l| l.link);
        if let Some(detected) = replaces {
            end_link(&mut tx, detected, UnlinkSource::Converted, actor).await?;
        }
        let item_id = item.id;
        let link: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, \
                     work_item_id, source, linked_by, linked_by_kind) \
                     VALUES ($1, $2, $3, 'manual', $4, $5) RETURNING id",
                )
                .bind(mr.get())
                .bind(project.get())
                .bind(item_id)
                .bind(viewer.get())
                .bind(kind.as_str())
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
            by: Some(actor),
            replaces,
        };
        Ok(Settle::Commit(
            record(change)?,
            (LinkOutcome::Changed, answer, decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// End the open link `target` names on merge request `iid` of `project`
/// as `actor` (a detected or a person's link), decided inside the
/// transaction. After `decide`: `FG-REPO-0005 resource=merge_request` for
/// an absent one; `FG-INT-0018` for a reference when the project is not
/// bound; `FG-REPO-0005 resource=work_item_link` when it is not linked now.
pub async fn unlink_work_item_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    (project, iid): (ProjectId, i64),
    target: &LinkTarget,
    actor: LinkActor,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(LinkAnswer, P), ForgeError> {
    const ACTION: &str = "work_item.unlinked";
    let (viewer, _) = actor;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.merge_request.ok_or_else(absent)?;
        let mr = current.id;
        let open: Vec<OpenLink> = tx.step(|c| open_links(c, mr)).await?;
        let found = match target {
            LinkTarget::Ref(reference) => {
                let bound = tx
                    .step(|c| binding_of(c, project))
                    .await?
                    .ok_or_else(|| not_bound(project))?;
                open.into_iter()
                    .find(|l| bound.owns(&l.work_item) && &l.work_item.reference == reference)
            }
            LinkTarget::WorkItem(id) => open.into_iter().find(|l| l.work_item.id == *id),
        }
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "work_item_link")
                .with_context("reason", "not_linked")
        })?;
        end_link(&mut tx, found.link, UnlinkSource::Manual, actor).await?;
        let answer = LinkAnswer {
            link: found.link,
            work_item: found.work_item.clone(), // clone: the change owns one
            source: found.source,
        };
        let change = WorkItemChange::Unlinked {
            project,
            merge_request: mr,
            iid: current.iid,
            link: found.link,
            work_item: found.work_item,
            source: UnlinkSource::Manual,
            by: Some(actor),
        };
        Ok(Settle::Commit(record(change)?, (answer, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
