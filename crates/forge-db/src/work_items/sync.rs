//! The `work-item-sync` subscriber (M3 I1b, ADR-0016 §7 "ordered", plan 11
//! §5 I1 option B): it reads every published event in order, one per
//! transaction, and keeps the work item links and `event_work_items` up to
//! date. `ordered`: an event it cannot process stops it
//! (`FG-DB-0008 reason=stuck` after five failures in a row, doctor
//! `subscribers.lag`, runbook subscriber-lag) — it never skips one by
//! itself, so no link and no event's work items are lost silently.
//!
//! Per event type (matched without `_`, ADR-0016 §5):
//!
//! - `merge_request.*` — the merge request's row is locked (`FOR NO KEY
//!   UPDATE`: a person's link or unlink locks it too) and, when its project
//!   is bound to an integration, its **detected** links are brought in line
//!   with the references in its *current* title and source branch
//!   (`forge_core::find_refs`, at most [`MAX_AUTO_LINKS`]): a new reference
//!   is linked (`source = auto`, derived `work_item.linked`), an auto link
//!   whose reference is gone is ended (`unlink_source = auto`, derived
//!   `work_item.unlinked`; the row stays). Manual links are never ended
//!   here, and a work item a person unlinked from this merge request is not
//!   linked again by detection (until a person links it). The stored source
//!   branch never changes, so deleting the branch keeps its links (02 §5.7).
//!   Reading the current row (not the event's moment) converges: a later
//!   event of the same merge request is handled after this one anyway.
//!   Then the event is recorded with the merge request's open links.
//! - `repository.pushed` — the event is recorded with the work items its
//!   listed branch names reference (bound projects only) and with the open
//!   links of the open merge requests from those branches. A cut list
//!   (`refs_truncated`) records what it lists.
//! - `work_item.linked` / `.unlinked` — recorded with their work item.
//! - Every other type concerns no work item.
//!
//! An event recorded with more than [`MAX_EVENT_WORK_ITEMS`] work items, or
//! a merge request referencing more than [`MAX_AUTO_LINKS`], is cut there
//! and logged at warn (`forge.work_items.event_items_capped` /
//! `forge.work_items.detected_capped`), never silently.

use std::collections::HashSet;

use forge_core::ids::{MergeRequestId, ProjectId};
use forge_core::{
    find_refs, DedupKey, DedupPart, EventType, LinkSource, UnlinkSource, WorkItemChange,
    WorkItemRef,
};
use forge_errors::{ErrorCode, ForgeError};

use super::{
    binding_of, open_links, record_event_items, work_item, BoundTo, MAX_AUTO_LINKS,
    MAX_EVENT_WORK_ITEMS,
};
use crate::change::DomainEvent;
use crate::query_error;
use crate::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};

/// The subscriber's name (its row in forge.event_subscribers).
pub const WORK_ITEM_SYNC: &str = "work-item-sync";

/// The subscriber (no configuration).
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkItemSync;

const BRANCH_PREFIX: &str = "refs/heads/";

fn bad_payload(event: &SourceEvent, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_event_payload")
        .with_context("subscriber", WORK_ITEM_SYNC)
        .with_context("event_type", event.event_type.clone()) // clone: the context owns it
        .with_context("field", field)
        .with_context("seq", event.seq)
}

fn positive(event: &SourceEvent, field: &'static str) -> Result<i64, ForgeError> {
    event
        .payload
        .get(field)
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(|| bad_payload(event, field))
}

fn project_of(event: &SourceEvent) -> Result<ProjectId, ForgeError> {
    event
        .project_id
        .ok_or_else(|| bad_payload(event, "project_id"))
}

/// The references of `texts`, distinct, first match first, at most
/// [`MAX_AUTO_LINKS`] (`capped` when there were more).
fn detected(texts: &[&str], cap: usize) -> (Vec<WorkItemRef>, bool) {
    let mut out: Vec<WorkItemRef> = Vec::new();
    let mut capped = false;
    for text in texts {
        for reference in find_refs(text) {
            if out.contains(&reference) {
                continue;
            }
            if out.len() == cap {
                capped = true;
                break;
            }
            out.push(reference);
        }
    }
    (out, capped)
}

/// `ids` distinct, in order, at most [`MAX_EVENT_WORK_ITEMS`]; logs the
/// overflow once.
fn bounded_items(event: &SourceEvent, ids: Vec<i64>) -> Vec<i64> {
    let mut seen = HashSet::new();
    let mut out: Vec<i64> = ids.into_iter().filter(|id| seen.insert(*id)).collect();
    if out.len() > MAX_EVENT_WORK_ITEMS {
        tracing::warn!(
            target: "forge_db",
            event = "forge.work_items.event_items_capped",
            subscriber = WORK_ITEM_SYNC,
            request_id = %event.request_id,
            source_seq = event.seq,
            event_type = %event.event_type,
            found = out.len(),
            recorded = MAX_EVENT_WORK_ITEMS,
            "an event concerns more work items than are recorded for it; the first ones were recorded"
        );
        out.truncate(MAX_EVENT_WORK_ITEMS);
    }
    out
}

fn link_event(change: &WorkItemChange) -> Result<DomainEvent, ForgeError> {
    Ok(DomainEvent {
        event_type: change.event_type(),
        schema_version: 1,
        aggregate_type: "merge_request",
        aggregate_id: change.merge_request().get(),
        project_id: Some(change.project()),
        payload: change.to_value(),
        dedup_key: Some(DedupKey::new(
            change.dedup_prefix(),
            &[DedupPart::Id(change.link())],
        )?),
    })
}

impl WorkItemSync {
    async fn on_merge_request(
        event: &SourceEvent,
        tx: &mut DerivedTx<'_>,
    ) -> Result<(), ForgeError> {
        let project = project_of(event)?;
        let id = MergeRequestId::new(positive(event, "merge_request_id")?);
        // The lock order of every writer of links: the project row, then
        // the merge request (a person's link and an unbinding lock the
        // project FOR UPDATE first): the binding read below cannot change
        // under the step, and no cycle can form.
        lock_project(tx, project).await?;
        let row: Option<(i64, i64, String, String)> = sqlx::query_as(
            "SELECT project_id, iid, title, source_branch FROM forge.merge_requests \
             WHERE id = $1 FOR NO KEY UPDATE",
        )
        .bind(id.get())
        .fetch_optional(tx.conn())
        .await
        .map_err(|e| query_error("work_items.sync.merge_request", e))?;
        // Purged with its project: nothing left to link (the event stays).
        let Some((row_project, iid, title, source_branch)) = row else {
            return Ok(());
        };
        if row_project != project.get() {
            return Err(bad_payload(event, "project_id"));
        }
        // Without a binding no work item is recorded: PBI numbers are per
        // kanflow project, and only the current binding's are this
        // project's (I1b review).
        let Some(bound) = binding_of(tx.conn(), project).await? else {
            return Ok(());
        };
        Self::sync_links(
            event,
            tx,
            (project, id, iid),
            &bound,
            &title,
            &source_branch,
        )
        .await?;
        let items: Vec<i64> = open_links(tx.conn(), id)
            .await?
            .into_iter()
            .filter(|l| bound.owns(&l.work_item))
            .map(|l| l.work_item.id)
            .collect();
        let items = bounded_items(event, items);
        record_event_items(tx.conn(), event.seq, &items).await
    }

    /// Bring the merge request's auto links in line with its title and
    /// source branch (module docs).
    async fn sync_links(
        event: &SourceEvent,
        tx: &mut DerivedTx<'_>,
        (project, id, iid): (ProjectId, MergeRequestId, i64),
        bound: &BoundTo,
        title: &str,
        source_branch: &str,
    ) -> Result<(), ForgeError> {
        let branch = source_branch
            .strip_prefix(BRANCH_PREFIX)
            .unwrap_or(source_branch);
        let (references, capped) = detected(&[title, branch], MAX_AUTO_LINKS);
        if capped {
            tracing::warn!(
                target: "forge_db",
                event = "forge.work_items.detected_capped",
                subscriber = WORK_ITEM_SYNC,
                request_id = %event.request_id,
                source_seq = event.seq,
                project_id = project.get(),
                merge_request_id = id.get(),
                linked = MAX_AUTO_LINKS,
                "a merge request references more work items than are linked automatically; the first ones were linked"
            );
        }
        let mut wanted = Vec::with_capacity(references.len());
        for reference in &references {
            wanted.push(work_item(tx.conn(), bound, reference).await?);
        }
        let open = open_links(tx.conn(), id).await?;
        // The work items a person (`human`, I1b review: not an agent, a
        // service account or a bot) unlinked last from this merge request.
        let unlinked_by_people: Vec<i64> = sqlx::query_scalar(
            "SELECT work_item_id FROM (SELECT DISTINCT ON (work_item_id) work_item_id, \
             unlink_source, unlinked_by_kind FROM forge.merge_request_work_items \
             WHERE merge_request_id = $1 ORDER BY work_item_id, id DESC) AS last \
             WHERE unlink_source = 'manual' AND unlinked_by_kind = 'human'",
        )
        .bind(id.get())
        .fetch_all(tx.conn())
        .await
        .map_err(|e| query_error("work_items.sync.unlinked_by_people", e))?;
        let wanted_ids: HashSet<i64> = wanted.iter().map(|w| w.id).collect();
        for link in &open {
            if link.source == LinkSource::Auto && !wanted_ids.contains(&link.work_item.id) {
                let ended = sqlx::query(
                    "UPDATE forge.merge_request_work_items SET unlinked_at = now(), \
                     unlink_source = 'auto' WHERE id = $1 AND unlinked_at IS NULL",
                )
                .bind(link.link)
                .execute(tx.conn())
                .await
                .map_err(|e| query_error("work_items.sync.unlink", e))?;
                if ended.rows_affected() == 1 {
                    let change = WorkItemChange::Unlinked {
                        project,
                        merge_request: id,
                        iid,
                        link: link.link,
                        work_item: link.work_item.clone(), // clone: the event owns it
                        source: UnlinkSource::Auto,
                        by: None,
                    };
                    tx.emit(link_event(&change)?).await?;
                }
            }
        }
        for item in wanted {
            let already = open.iter().any(|l| l.work_item.id == item.id);
            if already || unlinked_by_people.contains(&item.id) {
                continue;
            }
            let link: i64 = sqlx::query_scalar(
                "INSERT INTO forge.merge_request_work_items (merge_request_id, project_id, \
                 work_item_id, source, linked_seq) VALUES ($1, $2, $3, 'auto', $4) RETURNING id",
            )
            .bind(id.get())
            .bind(project.get())
            .bind(item.id)
            .bind(event.seq)
            .fetch_one(tx.conn())
            .await
            .map_err(|e| query_error("work_items.sync.link", e))?;
            let change = WorkItemChange::Linked {
                project,
                merge_request: id,
                iid,
                link,
                work_item: item,
                source: LinkSource::Auto,
                by: None,
                replaces: None,
            };
            tx.emit(link_event(&change)?).await?;
        }
        Ok(())
    }

    async fn on_push(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        let project = project_of(event)?;
        // The project lock first, as every link writer (on_merge_request).
        lock_project(tx, project).await?;
        let refs = event
            .payload
            .get("refs")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| bad_payload(event, "refs"))?;
        let mut branches: Vec<String> = Vec::new();
        for entry in refs {
            let name = entry
                .get("ref")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| bad_payload(event, "refs.ref"))?;
            if name.starts_with(BRANCH_PREFIX) {
                branches.push(name.to_owned());
            }
        }
        if branches.is_empty() {
            return Ok(());
        }
        // The current binding's work items only (I1b review); none without.
        let Some(bound) = binding_of(tx.conn(), project).await? else {
            return Ok(());
        };
        let mut items: Vec<i64> = Vec::new();
        let names: Vec<&str> = branches
            .iter()
            .map(|b| b.strip_prefix(BRANCH_PREFIX).unwrap_or(b))
            .collect();
        // One more than recorded, so the overflow is logged.
        let (references, _) = detected(&names, MAX_EVENT_WORK_ITEMS + 1);
        for reference in &references {
            items.push(work_item(tx.conn(), &bound, reference).await?.id);
        }
        let from_merge_requests: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT l.work_item_id FROM forge.merge_request_work_items AS l \
             JOIN forge.merge_requests AS m ON m.id = l.merge_request_id \
             JOIN forge.work_items AS w ON w.id = l.work_item_id \
             WHERE m.project_id = $1 AND m.state = 'open' AND m.source_branch = ANY($2) \
             AND l.unlinked_at IS NULL AND w.integration_id = $4 \
             AND w.external_project_id = $5 ORDER BY l.work_item_id LIMIT $3",
        )
        .bind(project.get())
        .bind(&branches)
        .bind(i64::try_from(MAX_EVENT_WORK_ITEMS + 1).unwrap_or(i64::MAX))
        .bind(bound.integration.get())
        .bind(&bound.external_project_id)
        .fetch_all(tx.conn())
        .await
        .map_err(|e| query_error("work_items.sync.push_links", e))?;
        items.extend(from_merge_requests);
        let items = bounded_items(event, items);
        record_event_items(tx.conn(), event.seq, &items).await
    }

    async fn on_link(event: &SourceEvent, tx: &mut DerivedTx<'_>) -> Result<(), ForgeError> {
        let project = project_of(event)?;
        lock_project(tx, project).await?;
        let item = positive(event, "work_item_id")?;
        // Recorded only while the work item is the current binding's.
        let Some(bound) = binding_of(tx.conn(), project).await? else {
            return Ok(());
        };
        let owned: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.work_items WHERE id = $1 \
             AND integration_id = $2 AND external_project_id = $3)",
        )
        .bind(item)
        .bind(bound.integration.get())
        .bind(&bound.external_project_id)
        .fetch_one(tx.conn())
        .await
        .map_err(|e| query_error("work_items.sync.link_owner", e))?;
        if !owned {
            return Ok(());
        }
        record_event_items(tx.conn(), event.seq, &[item]).await
    }
}

/// Lock `project`'s row `FOR SHARE` (see `on_merge_request`).
async fn lock_project(tx: &mut DerivedTx<'_>, project: ProjectId) -> Result<(), ForgeError> {
    sqlx::query("SELECT 1 FROM forge.projects WHERE id = $1 FOR SHARE")
        .bind(project.get())
        .execute(tx.conn())
        .await
        .map_err(|e| query_error("work_items.sync.project_lock", e))?;
    Ok(())
}

impl Subscriber for WorkItemSync {
    fn name(&self) -> &'static str {
        WORK_ITEM_SYNC
    }

    fn ordering(&self) -> Ordering {
        Ordering::Ordered
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            let Some(kind) = EventType::parse(&event.event_type) else {
                // A type this build does not know (a newer writer). This
                // subscriber is ordered: skipping it could lose the links
                // it carries, so it fails (stuck after five, FG-DB-0008;
                // the worker logs it with the event's seq and request id)
                // until a build that knows it runs (I1b review).
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "unknown_event_type")
                    .with_context("subscriber", WORK_ITEM_SYNC)
                    .with_context("event_type", event.event_type.clone()) // clone: the context owns it
                    .with_context("seq", event.seq));
            };
            match kind {
                EventType::MergeRequestOpened
                | EventType::MergeRequestUpdated
                | EventType::MergeRequestDraftChanged
                | EventType::MergeRequestClosed
                | EventType::MergeRequestReopened
                | EventType::MergeRequestPushed
                | EventType::MergeRequestConflictDetected
                | EventType::MergeRequestConflictResolved
                | EventType::MergeRequestConflictPredicted
                | EventType::MergeRequestCommented
                | EventType::MergeRequestMerged
                | EventType::MergeRequestSourceBranchDeleted
                | EventType::MergeRequestApproved
                | EventType::MergeRequestUnapproved => Self::on_merge_request(event, tx).await,
                EventType::RepositoryPushed => Self::on_push(event, tx).await,
                EventType::WorkItemLinked | EventType::WorkItemUnlinked => {
                    Self::on_link(event, tx).await
                }
                EventType::ProjectCreated
                | EventType::RepositoryCreated
                | EventType::TokenCreated
                | EventType::TokenRevoked
                | EventType::MemberAdded
                | EventType::MemberRoleChanged
                | EventType::MemberRemoved
                | EventType::ProtectedRefCreated
                | EventType::ProtectedRefUpdated
                | EventType::ProtectedRefDeleted
                | EventType::ProjectVisibilityChanged
                | EventType::ProjectDeleted
                | EventType::ProjectRestored
                | EventType::ProjectPurged
                | EventType::ProjectMoved
                | EventType::GroupMoved
                | EventType::UserRenamed
                | EventType::CodeownersEnforcementChanged
                | EventType::CodeownersEnforcementRestored
                // M3 C1: a check run names a commit, not a merge request;
                // which merge requests' work items it concerns is the
                // feed's to resolve (I1c), not recorded here.
                | EventType::CheckCompleted
                // M3 I2: a webhook's own state, no merge request.
                | EventType::WebhookDisabled => Ok(()),
            }
        })
    }
}
